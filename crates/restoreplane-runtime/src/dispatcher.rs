use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use restoreplane_catalog::{
    AdmissionRequest, BeginCommitDisposition, ClaimCompletionSpec, ClaimRequest,
    ClaimTerminalOutcome, ClaimedWork, ProcessExitSpec, ProcessStartSpec, RepositoryError,
    ResourceRequirement, RetryDisposition, RuntimeRepository, SpoolRequirement,
};
use restoreplane_domain::WorkItemKind;
use thiserror::Error;
use uuid::Uuid;

use crate::{LaunchSpec, SupervisorError};

const MAX_IDENTITY_TEXT: usize = 1024;
const MAX_PHASE_TEXT: usize = 256;
const WINDOWS_TO_UNIX_EPOCH_100NS: u64 = 116_444_736_000_000_000;

#[derive(Debug, Error)]
pub enum DispatchError {
    #[error("DISPATCH_CONFIG_INVALID")]
    ConfigInvalid,
    #[error("DISPATCH_PLAN_INVALID")]
    PlanInvalid,
    #[error("DISPATCH_CLOCK_INVALID")]
    ClockInvalid,
    #[error("DISPATCH_ARITHMETIC_OVERFLOW")]
    ArithmeticOverflow,
    #[error("{0}")]
    Repository(#[from] RepositoryError),
    #[error("{0}")]
    Supervisor(#[from] SupervisorError),
}

impl DispatchError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::ConfigInvalid => "DISPATCH_CONFIG_INVALID",
            Self::PlanInvalid => "DISPATCH_PLAN_INVALID",
            Self::ClockInvalid => "DISPATCH_CLOCK_INVALID",
            Self::ArithmeticOverflow => "DISPATCH_ARITHMETIC_OVERFLOW",
            Self::Repository(error) => error.code(),
            Self::Supervisor(error) => error.code(),
        }
    }
}

#[derive(Clone)]
pub struct DispatchPlan {
    launch: LaunchSpec,
    phase: String,
    resources: Vec<ResourceRequirement>,
    spool: Option<SpoolRequirement>,
    wait_slice: Duration,
    timeout: Duration,
    retry_delay: Option<Duration>,
}

impl DispatchPlan {
    pub fn new(
        launch: LaunchSpec,
        phase: impl Into<String>,
        resources: Vec<ResourceRequirement>,
        spool: Option<SpoolRequirement>,
    ) -> Result<Self, DispatchError> {
        let plan = Self {
            launch,
            phase: phase.into(),
            resources,
            spool,
            wait_slice: Duration::from_millis(250),
            timeout: Duration::from_secs(24 * 60 * 60),
            retry_delay: None,
        };
        plan.validate()?;
        Ok(plan)
    }

    pub fn with_wait_policy(
        mut self,
        wait_slice: Duration,
        timeout: Duration,
    ) -> Result<Self, DispatchError> {
        self.wait_slice = wait_slice;
        self.timeout = timeout;
        self.validate()?;
        Ok(self)
    }

    pub fn with_retry_delay(mut self, retry_delay: Duration) -> Result<Self, DispatchError> {
        self.retry_delay = Some(retry_delay);
        self.validate()?;
        Ok(self)
    }

    fn validate(&self) -> Result<(), DispatchError> {
        if self.phase.is_empty()
            || self.phase.len() > MAX_PHASE_TEXT
            || self.wait_slice.is_zero()
            || self.timeout < self.wait_slice
            || self.retry_delay.is_some_and(|delay| delay.is_zero())
            || self
                .retry_delay
                .is_some_and(|delay| i64::try_from(delay.as_millis()).is_err())
        {
            return Err(DispatchError::PlanInvalid);
        }
        Ok(())
    }

    fn validate_against(&self, config: &DispatchConfig) -> Result<(), DispatchError> {
        self.validate()?;
        let wait_slice_ms =
            i64::try_from(self.wait_slice.as_millis()).map_err(|_| DispatchError::PlanInvalid)?;
        if wait_slice_ms >= config.lease_duration_ms {
            return Err(DispatchError::PlanInvalid);
        }
        Ok(())
    }
}

pub trait DispatchPlanner: Send + Sync {
    fn plan(&self, claim: &ClaimedWork) -> Result<DispatchPlan, DispatchError>;
}

pub trait RuntimeClock: Send + Sync {
    fn now_ms(&self) -> i64;
}

pub struct SystemRuntimeClock;

impl RuntimeClock for SystemRuntimeClock {
    fn now_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| i64::try_from(duration.as_millis()).ok())
            .unwrap_or(-1)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagedProcessIdentity {
    process_id: i64,
    process_start_time_ms: i64,
    job_key: String,
}

impl ManagedProcessIdentity {
    pub fn new(
        process_id: i64,
        process_start_time_ms: i64,
        job_key: impl Into<String>,
    ) -> Result<Self, DispatchError> {
        let identity = Self {
            process_id,
            process_start_time_ms,
            job_key: job_key.into(),
        };
        if identity.process_id <= 0
            || identity.process_start_time_ms < 0
            || identity.job_key.is_empty()
            || identity.job_key.len() > MAX_IDENTITY_TEXT
        {
            return Err(DispatchError::PlanInvalid);
        }
        Ok(identity)
    }

    pub const fn process_id(&self) -> i64 {
        self.process_id
    }

    pub const fn process_start_time_ms(&self) -> i64 {
        self.process_start_time_ms
    }

    pub fn job_key(&self) -> &str {
        &self.job_key
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagedProcessExit {
    exit_code: i64,
    stdout_bytes: u64,
    stdout_truncated: bool,
    stderr_bytes: u64,
    stderr_truncated: bool,
}

impl ManagedProcessExit {
    pub fn new(
        exit_code: i64,
        stdout_bytes: u64,
        stdout_truncated: bool,
        stderr_bytes: u64,
        stderr_truncated: bool,
    ) -> Result<Self, DispatchError> {
        if exit_code < 0 {
            return Err(DispatchError::PlanInvalid);
        }
        Ok(Self {
            exit_code,
            stdout_bytes,
            stdout_truncated,
            stderr_bytes,
            stderr_truncated,
        })
    }

    pub const fn exit_code(&self) -> i64 {
        self.exit_code
    }
}

pub trait ManagedProcess: Send {
    fn identity(&self) -> &ManagedProcessIdentity;

    fn wait_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<ManagedProcessExit>, DispatchError>;

    fn terminate(&mut self) -> Result<ManagedProcessExit, DispatchError>;
}

pub trait ProcessLauncher: Send + Sync {
    fn launch(&self, spec: &LaunchSpec) -> Result<Box<dyn ManagedProcess>, DispatchError>;
}

#[derive(Clone, Debug)]
pub struct DispatchConfig {
    worker_id: String,
    worker_boot_id: String,
    lease_duration_ms: i64,
    admission_delay_ms: i64,
}

impl DispatchConfig {
    pub fn new(
        worker_id: impl Into<String>,
        worker_boot_id: impl Into<String>,
        lease_duration_ms: i64,
        admission_delay_ms: i64,
    ) -> Result<Self, DispatchError> {
        let config = Self {
            worker_id: worker_id.into(),
            worker_boot_id: worker_boot_id.into(),
            lease_duration_ms,
            admission_delay_ms,
        };
        if config.worker_id.is_empty()
            || config.worker_id.len() > MAX_IDENTITY_TEXT
            || config.worker_boot_id.is_empty()
            || config.worker_boot_id.len() > MAX_IDENTITY_TEXT
            || config.lease_duration_ms <= 0
            || config.admission_delay_ms <= 0
        {
            return Err(DispatchError::ConfigInvalid);
        }
        Ok(config)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DispatchOutcome {
    Idle,
    Deferred {
        work_item_id: String,
        reason: String,
    },
    Completed {
        work_item_id: String,
        attempt_no: i64,
        exit_code: i64,
    },
    Failed {
        work_item_id: String,
        attempt_no: i64,
        error_code: String,
    },
    RetryScheduled {
        work_item_id: String,
        attempt_no: i64,
        error_code: String,
    },
    Cancelled {
        work_item_id: String,
        attempt_no: i64,
        error_code: String,
    },
}

pub struct Dispatcher {
    repository: RuntimeRepository,
    planner: Arc<dyn DispatchPlanner>,
    launcher: Arc<dyn ProcessLauncher>,
    clock: Arc<dyn RuntimeClock>,
    config: DispatchConfig,
}

impl Dispatcher {
    pub fn new(
        repository: RuntimeRepository,
        planner: Arc<dyn DispatchPlanner>,
        launcher: Arc<dyn ProcessLauncher>,
        clock: Arc<dyn RuntimeClock>,
        config: DispatchConfig,
    ) -> Self {
        Self {
            repository,
            planner,
            launcher,
            clock,
            config,
        }
    }

    pub fn dispatch_once(&self) -> Result<DispatchOutcome, DispatchError> {
        let claim_time = self.now_ms()?;
        let Some(claim) = self.repository.claim_next(
            ClaimRequest::new(
                &self.config.worker_id,
                &self.config.worker_boot_id,
                claim_time,
                self.config.lease_duration_ms,
            )
            .for_kind(WorkItemKind::Operation),
        )?
        else {
            return Ok(DispatchOutcome::Idle);
        };
        if claim.kind() != WorkItemKind::Operation {
            return self.fail_claim(&claim, "DISPATCH_KIND_NOT_SUPPORTED", None);
        }

        let plan = match self.planner.plan(&claim).and_then(|plan| {
            plan.validate_against(&self.config)?;
            Ok(plan)
        }) {
            Ok(plan) => plan,
            Err(error) => {
                return self.fail_claim(&claim, error.code(), None);
            }
        };
        let admission_time = self.now_ms()?;
        let admission = self.repository.admit_claim(AdmissionRequest::new(
            claim.clone(),
            plan.resources.clone(),
            plan.spool.clone(),
            admission_time,
        ))?;
        if !admission.admitted() {
            let reason = admission
                .wait_reason()
                .ok_or(DispatchError::PlanInvalid)?
                .to_owned();
            let available_at_ms = admission_time
                .checked_add(self.config.admission_delay_ms)
                .ok_or(DispatchError::ArithmeticOverflow)?;
            self.repository
                .defer_claim(&claim, admission_time, available_at_ms, &reason)?;
            return Ok(DispatchOutcome::Deferred {
                work_item_id: claim.work_item_id().to_string(),
                reason,
            });
        }

        let start_time = self.now_ms()?;
        self.repository.start_claim(&claim, start_time)?;
        let mut process = match self.launcher.launch(&plan.launch) {
            Ok(process) => process,
            Err(error) => return self.fail_claim(&claim, error.code(), None),
        };
        let identity = process.identity().clone();
        let process_instance_id = Uuid::new_v4().to_string();
        let observed_start = self.now_ms()?;
        let digest = hex_digest(plan.launch.expected_sha256());
        if let Err(error) = self.repository.record_process_start(ProcessStartSpec::new(
            process_instance_id.clone(),
            claim.clone(),
            plan.phase.clone(),
            identity.process_id(),
            identity.process_start_time_ms(),
            &self.config.worker_boot_id,
            identity.job_key(),
            plan.launch.executable().to_string_lossy().into_owned(),
            Some(digest),
            observed_start,
        )) {
            let _ = process.terminate();
            return self.fail_claim(&claim, error.code(), None);
        }
        if let Err(error) =
            self.repository
                .mark_process_running(&claim, &process_instance_id, observed_start)
        {
            let exit = process.terminate()?;
            let ended_at = self.now_ms()?;
            let _ = self.repository.record_process_exit(
                &claim,
                ProcessExitSpec::new(
                    process_instance_id,
                    ended_at,
                    exit.exit_code(),
                    "dispatcher_start_failed",
                ),
            );
            return Err(error.into());
        }

        let (exit, termination_reason) =
            self.wait_with_heartbeats(&claim, &plan, process.as_mut())?;
        let ended_at = self.now_ms()?;
        self.repository.record_process_exit(
            &claim,
            ProcessExitSpec::new(
                process_instance_id,
                ended_at,
                exit.exit_code(),
                termination_reason,
            ),
        )?;
        let result_json = Some(
            serde_json::json!({
                "exitCode": exit.exit_code,
                "stdoutBytes": exit.stdout_bytes,
                "stdoutTruncated": exit.stdout_truncated,
                "stderrBytes": exit.stderr_bytes,
                "stderrTruncated": exit.stderr_truncated,
            })
            .to_string(),
        );
        if termination_reason == "cancelled" {
            return self.cancel_claim(&claim, result_json);
        }
        if exit.exit_code() == 0 && termination_reason == "exited" {
            let commit_time = self.now_ms()?;
            if self.repository.begin_claim_commit(&claim, commit_time)?
                == BeginCommitDisposition::CancelRequested
            {
                return self.cancel_claim(&claim, result_json);
            }
            let completed_at = self.now_ms()?;
            let actual = self
                .repository
                .complete_claim(ClaimCompletionSpec::succeeded(
                    claim.clone(),
                    completed_at,
                    result_json,
                ))?;
            if actual == ClaimTerminalOutcome::Cancelled {
                Ok(cancelled_outcome(&claim))
            } else {
                Ok(DispatchOutcome::Completed {
                    work_item_id: claim.work_item_id().to_string(),
                    attempt_no: claim.attempt_no(),
                    exit_code: exit.exit_code(),
                })
            }
        } else {
            let error_code = if termination_reason == "timeout" {
                "PROCESS_TIMEOUT"
            } else {
                "PROCESS_EXIT_NONZERO"
            };
            match plan.retry_delay {
                Some(retry_delay) => self.retry_claim(&claim, error_code, result_json, retry_delay),
                None => self.fail_claim(&claim, error_code, result_json),
            }
        }
    }

    fn wait_with_heartbeats(
        &self,
        claim: &ClaimedWork,
        plan: &DispatchPlan,
        process: &mut dyn ManagedProcess,
    ) -> Result<(ManagedProcessExit, &'static str), DispatchError> {
        let mut elapsed = Duration::ZERO;
        loop {
            if let Some(exit) = process.wait_timeout(plan.wait_slice)? {
                return Ok((exit, "exited"));
            }
            elapsed = elapsed
                .checked_add(plan.wait_slice)
                .ok_or(DispatchError::ArithmeticOverflow)?;
            let heartbeat_at = self.now_ms()?;
            if self
                .repository
                .claim_cancel_requested(claim, heartbeat_at)?
            {
                return process.terminate().map(|exit| (exit, "cancelled"));
            }
            if elapsed >= plan.timeout {
                return process.terminate().map(|exit| (exit, "timeout"));
            }
            let lease_expires_at = heartbeat_at
                .checked_add(self.config.lease_duration_ms)
                .ok_or(DispatchError::ArithmeticOverflow)?;
            if let Err(error) =
                self.repository
                    .heartbeat_claim(claim, heartbeat_at, lease_expires_at)
            {
                let _ = process.terminate();
                return Err(error.into());
            }
        }
    }

    fn fail_claim(
        &self,
        claim: &ClaimedWork,
        error_code: &str,
        result_json: Option<String>,
    ) -> Result<DispatchOutcome, DispatchError> {
        let completed_at = self.now_ms()?;
        let actual = self.repository.complete_claim(ClaimCompletionSpec::failed(
            claim.clone(),
            completed_at,
            normalize_error_code(error_code),
            result_json,
        ))?;
        if actual == ClaimTerminalOutcome::Cancelled {
            Ok(cancelled_outcome(claim))
        } else {
            Ok(DispatchOutcome::Failed {
                work_item_id: claim.work_item_id().to_string(),
                attempt_no: claim.attempt_no(),
                error_code: normalize_error_code(error_code).to_owned(),
            })
        }
    }

    fn cancel_claim(
        &self,
        claim: &ClaimedWork,
        result_json: Option<String>,
    ) -> Result<DispatchOutcome, DispatchError> {
        let completed_at = self.now_ms()?;
        let actual = self
            .repository
            .complete_claim(ClaimCompletionSpec::cancelled(
                claim.clone(),
                completed_at,
                Some("CANCEL_REQUESTED".to_owned()),
                result_json,
            ))?;
        if actual != ClaimTerminalOutcome::Cancelled {
            return Err(DispatchError::PlanInvalid);
        }
        Ok(cancelled_outcome(claim))
    }

    fn retry_claim(
        &self,
        claim: &ClaimedWork,
        error_code: &str,
        result_json: Option<String>,
        retry_delay: Duration,
    ) -> Result<DispatchOutcome, DispatchError> {
        let completed_at = self.now_ms()?;
        let delay_ms =
            i64::try_from(retry_delay.as_millis()).map_err(|_| DispatchError::PlanInvalid)?;
        let available_at = completed_at
            .checked_add(delay_ms)
            .ok_or(DispatchError::ArithmeticOverflow)?;
        match self.repository.retry_claim(
            claim,
            completed_at,
            available_at,
            normalize_error_code(error_code),
            result_json,
        )? {
            RetryDisposition::Requeued => Ok(DispatchOutcome::RetryScheduled {
                work_item_id: claim.work_item_id().to_string(),
                attempt_no: claim.attempt_no(),
                error_code: normalize_error_code(error_code).to_owned(),
            }),
            RetryDisposition::Exhausted => Ok(DispatchOutcome::Failed {
                work_item_id: claim.work_item_id().to_string(),
                attempt_no: claim.attempt_no(),
                error_code: "RETRY_BUDGET_EXHAUSTED".to_owned(),
            }),
            RetryDisposition::Cancelled => Ok(cancelled_outcome(claim)),
        }
    }

    fn now_ms(&self) -> Result<i64, DispatchError> {
        let now_ms = self.clock.now_ms();
        if now_ms < 0 {
            return Err(DispatchError::ClockInvalid);
        }
        Ok(now_ms)
    }
}

fn normalize_error_code(code: &str) -> &'static str {
    match code {
        "PROCESS_SPEC_INVALID" => "PROCESS_SPEC_INVALID",
        "PROCESS_EXECUTABLE_NOT_TRUSTED" => "PROCESS_EXECUTABLE_NOT_TRUSTED",
        "PROCESS_DEPENDENCY_NOT_TRUSTED" => "PROCESS_DEPENDENCY_NOT_TRUSTED",
        "PROCESS_IO" => "PROCESS_IO",
        "PROCESS_CREATE_FAILED" => "PROCESS_CREATE_FAILED",
        "PROCESS_JOB_FAILED" => "PROCESS_JOB_FAILED",
        "PROCESS_PIPE_FAILED" => "PROCESS_PIPE_FAILED",
        "PROCESS_HANDLE_INHERITANCE_FAILED" => "PROCESS_HANDLE_INHERITANCE_FAILED",
        "PROCESS_RESUME_FAILED" => "PROCESS_RESUME_FAILED",
        "PROCESS_WAIT_FAILED" => "PROCESS_WAIT_FAILED",
        "PROCESS_OUTPUT_DRAIN_FAILED" => "PROCESS_OUTPUT_DRAIN_FAILED",
        "PROCESS_TIMEOUT" => "PROCESS_TIMEOUT",
        "PROCESS_EXIT_NONZERO" => "PROCESS_EXIT_NONZERO",
        "CANCEL_REQUESTED" => "CANCEL_REQUESTED",
        "RETRY_BUDGET_EXHAUSTED" => "RETRY_BUDGET_EXHAUSTED",
        "DISPATCH_KIND_NOT_SUPPORTED" => "DISPATCH_KIND_NOT_SUPPORTED",
        _ => "DISPATCH_EXECUTION_FAILED",
    }
}

fn cancelled_outcome(claim: &ClaimedWork) -> DispatchOutcome {
    DispatchOutcome::Cancelled {
        work_item_id: claim.work_item_id().to_string(),
        attempt_no: claim.attempt_no(),
        error_code: "CANCEL_REQUESTED".to_owned(),
    }
}

fn hex_digest(digest: &[u8; 32]) -> String {
    use std::fmt::Write;

    let mut encoded = String::with_capacity(64);
    for byte in digest {
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

#[cfg(windows)]
pub struct WindowsProcessLauncher;

#[cfg(windows)]
impl ProcessLauncher for WindowsProcessLauncher {
    fn launch(&self, spec: &LaunchSpec) -> Result<Box<dyn ManagedProcess>, DispatchError> {
        let process = crate::ProcessSupervisor::launch(spec)?;
        let process_id = i64::from(process.id());
        let start_time_ms = filetime_100ns_to_unix_ms(process.start_time_100ns())?;
        let identity = ManagedProcessIdentity::new(process_id, start_time_ms, process.job_key())?;
        Ok(Box::new(WindowsManagedProcess { process, identity }))
    }
}

#[cfg(windows)]
struct WindowsManagedProcess {
    process: crate::SupervisedProcess,
    identity: ManagedProcessIdentity,
}

#[cfg(windows)]
impl ManagedProcess for WindowsManagedProcess {
    fn identity(&self) -> &ManagedProcessIdentity {
        &self.identity
    }

    fn wait_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<ManagedProcessExit>, DispatchError> {
        self.process
            .wait_timeout(timeout)?
            .map(managed_exit)
            .transpose()
    }

    fn terminate(&mut self) -> Result<ManagedProcessExit, DispatchError> {
        managed_exit(self.process.terminate()?)
    }
}

#[cfg(windows)]
fn managed_exit(exit: crate::ProcessExit) -> Result<ManagedProcessExit, DispatchError> {
    ManagedProcessExit::new(
        i64::from(exit.exit_code()),
        exit.stdout().total_bytes(),
        exit.stdout().truncated(),
        exit.stderr().total_bytes(),
        exit.stderr().truncated(),
    )
}

#[cfg(windows)]
fn filetime_100ns_to_unix_ms(value: u64) -> Result<i64, DispatchError> {
    let unix_100ns = value
        .checked_sub(WINDOWS_TO_UNIX_EPOCH_100NS)
        .ok_or(DispatchError::ClockInvalid)?;
    i64::try_from(unix_100ns / 10_000).map_err(|_| DispatchError::ClockInvalid)
}
