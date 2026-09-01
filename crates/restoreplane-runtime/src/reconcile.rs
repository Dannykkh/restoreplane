use std::path::PathBuf;
use std::sync::Arc;

use restoreplane_catalog::{
    ClaimedWork, CompleteRecoveryPointSpec, ExpiredClaim, RecordedProcessIdentity, RepositoryError,
    RuntimeRepository,
};
use restoreplane_domain::WorkItemKind;
use thiserror::Error;

use crate::RuntimeClock;

#[derive(Debug, Error)]
pub enum ReconcileError {
    #[error("RECONCILE_CONFIG_INVALID")]
    ConfigInvalid,
    #[error("RECONCILE_CLOCK_INVALID")]
    ClockInvalid,
    #[error("RECONCILE_ARITHMETIC_OVERFLOW")]
    ArithmeticOverflow,
    #[error("RECONCILE_PROCESS_INSPECTION_FAILED")]
    ProcessInspectionFailed,
    #[error("{0}")]
    Repository(#[from] RepositoryError),
}

impl ReconcileError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::ConfigInvalid => "RECONCILE_CONFIG_INVALID",
            Self::ClockInvalid => "RECONCILE_CLOCK_INVALID",
            Self::ArithmeticOverflow => "RECONCILE_ARITHMETIC_OVERFLOW",
            Self::ProcessInspectionFailed => "RECONCILE_PROCESS_INSPECTION_FAILED",
            Self::Repository(error) => error.code(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessProbe {
    process_id: i64,
    process_start_time_ms: i64,
    job_object_key: String,
    executable_path: Option<PathBuf>,
    executable_sha256: Option<String>,
}

impl ProcessProbe {
    pub fn new(
        process_id: i64,
        process_start_time_ms: i64,
        job_object_key: impl Into<String>,
    ) -> Result<Self, ReconcileError> {
        let job_object_key = job_object_key.into();
        if process_id <= 0
            || u32::try_from(process_id).is_err()
            || process_start_time_ms < 0
            || !job_object_key.starts_with(r"Local\RestorePlane.Job.")
            || job_object_key.len() > 256
            || job_object_key.contains('\0')
        {
            return Err(ReconcileError::ConfigInvalid);
        }
        Ok(Self {
            process_id,
            process_start_time_ms,
            job_object_key,
            executable_path: None,
            executable_sha256: None,
        })
    }

    pub const fn process_id(&self) -> i64 {
        self.process_id
    }

    pub const fn process_start_time_ms(&self) -> i64 {
        self.process_start_time_ms
    }

    pub fn job_object_key(&self) -> &str {
        &self.job_object_key
    }
}

impl From<&RecordedProcessIdentity> for ProcessProbe {
    fn from(value: &RecordedProcessIdentity) -> Self {
        Self {
            process_id: value.process_id(),
            process_start_time_ms: value.process_start_time_ms(),
            job_object_key: value.job_object_key().to_owned(),
            executable_path: Some(PathBuf::from(value.executable_path())),
            executable_sha256: value.executable_sha256().map(str::to_owned),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessObservation {
    Missing,
    Reused,
    Exited,
    MatchingOwned,
    MatchingUnowned,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnedProcessSnapshot {
    process_id: i64,
    process_start_time_ms: i64,
    executable_path: PathBuf,
}

impl SpawnedProcessSnapshot {
    pub const fn process_id(&self) -> i64 {
        self.process_id
    }

    pub const fn process_start_time_ms(&self) -> i64 {
        self.process_start_time_ms
    }

    pub fn executable_path(&self) -> &std::path::Path {
        &self.executable_path
    }
}

/// Reads the immutable OS identity of a just-spawned native child. Callers persist this
/// snapshot before consuming any child output so PID reuse can never be mistaken for the
/// originally fenced attempt.
#[cfg(windows)]
pub fn snapshot_spawned_process(process_id: i64) -> Result<SpawnedProcessSnapshot, ReconcileError> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let process_id = u32::try_from(process_id).map_err(|_| ReconcileError::ConfigInvalid)?;
    let process = unsafe {
        OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id)
            .map(InspectorHandle)
            .map_err(|_| ReconcileError::ProcessInspectionFailed)?
    };
    let mut creation_time = FILETIME::default();
    let mut exit_time = FILETIME::default();
    let mut kernel_time = FILETIME::default();
    let mut user_time = FILETIME::default();
    unsafe {
        GetProcessTimes(
            process.0,
            &mut creation_time,
            &mut exit_time,
            &mut kernel_time,
            &mut user_time,
        )
        .map_err(|_| ReconcileError::ProcessInspectionFailed)?;
    }
    let mut image = vec![0_u16; 32_768];
    let mut image_len = u32::try_from(image.len()).map_err(|_| ReconcileError::ConfigInvalid)?;
    unsafe {
        QueryFullProcessImageNameW(
            process.0,
            PROCESS_NAME_WIN32,
            PWSTR(image.as_mut_ptr()),
            &mut image_len,
        )
        .map_err(|_| ReconcileError::ProcessInspectionFailed)?;
    }
    image.truncate(usize::try_from(image_len).map_err(|_| ReconcileError::ConfigInvalid)?);
    let executable_path = PathBuf::from(
        String::from_utf16(&image).map_err(|_| ReconcileError::ProcessInspectionFailed)?,
    );
    if !executable_path.is_absolute() {
        return Err(ReconcileError::ProcessInspectionFailed);
    }
    Ok(SpawnedProcessSnapshot {
        process_id: i64::from(process_id),
        process_start_time_ms: filetime_to_unix_ms(creation_time)?,
        executable_path,
    })
}

#[cfg(not(windows))]
pub fn snapshot_spawned_process(
    _process_id: i64,
) -> Result<SpawnedProcessSnapshot, ReconcileError> {
    Err(ReconcileError::ProcessInspectionFailed)
}

pub trait ProcessInspector: Send + Sync {
    fn inspect(&self, probe: &ProcessProbe) -> Result<ProcessObservation, ReconcileError>;
}

pub trait VerifiedRecoveryPointSource: Send + Sync {
    fn verified_complete(
        &self,
        claim: &ClaimedWork,
    ) -> Result<Option<CompleteRecoveryPointSpec>, ReconcileError>;
}

#[cfg(windows)]
pub struct WindowsProcessInspector;

#[cfg(windows)]
impl ProcessInspector for WindowsProcessInspector {
    fn inspect(&self, probe: &ProcessProbe) -> Result<ProcessObservation, ReconcileError> {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::{
            GetLastError, SetLastError, BOOL, ERROR_FILE_NOT_FOUND, ERROR_INVALID_PARAMETER,
            FILETIME, WAIT_OBJECT_0, WAIT_TIMEOUT, WIN32_ERROR,
        };
        use windows::Win32::System::JobObjects::{IsProcessInJob, OpenJobObjectW};
        use windows::Win32::System::SystemServices::JOB_OBJECT_QUERY;
        use windows::Win32::System::Threading::{
            GetProcessTimes, OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
            PROCESS_SYNCHRONIZE,
        };

        let process_id =
            u32::try_from(probe.process_id).map_err(|_| ReconcileError::ConfigInvalid)?;
        let process = unsafe {
            SetLastError(WIN32_ERROR(0));
            match OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                process_id,
            ) {
                Ok(handle) => InspectorHandle(handle),
                Err(_) if GetLastError() == ERROR_INVALID_PARAMETER => {
                    return Ok(ProcessObservation::Missing);
                }
                Err(_) => return Err(ReconcileError::ProcessInspectionFailed),
            }
        };

        let mut creation_time = FILETIME::default();
        let mut exit_time = FILETIME::default();
        let mut kernel_time = FILETIME::default();
        let mut user_time = FILETIME::default();
        unsafe {
            GetProcessTimes(
                process.0,
                &mut creation_time,
                &mut exit_time,
                &mut kernel_time,
                &mut user_time,
            )
            .map_err(|_| ReconcileError::ProcessInspectionFailed)?;
        }
        let observed_start_time_ms = filetime_to_unix_ms(creation_time)?;
        if observed_start_time_ms != probe.process_start_time_ms {
            return Ok(ProcessObservation::Reused);
        }

        match unsafe { WaitForSingleObject(process.0, 0) } {
            WAIT_OBJECT_0 => return Ok(ProcessObservation::Exited),
            WAIT_TIMEOUT => {}
            _ => return Err(ReconcileError::ProcessInspectionFailed),
        }

        if let Some(expected_path) = probe.executable_path.as_ref() {
            let snapshot = snapshot_spawned_process(probe.process_id)?;
            if snapshot.process_start_time_ms != probe.process_start_time_ms
                || !snapshot
                    .executable_path
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&expected_path.to_string_lossy())
            {
                return Ok(ProcessObservation::Reused);
            }
            if let Some(expected_sha256) = probe.executable_sha256.as_deref() {
                let actual = crate::hash_executable(&snapshot.executable_path)
                    .map_err(|_| ReconcileError::ProcessInspectionFailed)?;
                if hex_digest(&actual) != expected_sha256 {
                    return Ok(ProcessObservation::Reused);
                }
            }
        }

        let mut job_name: Vec<u16> = probe.job_object_key.encode_utf16().collect();
        job_name.push(0);
        let job = unsafe {
            SetLastError(WIN32_ERROR(0));
            match OpenJobObjectW(JOB_OBJECT_QUERY, false, PCWSTR(job_name.as_ptr())) {
                Ok(handle) => InspectorHandle(handle),
                Err(_) if GetLastError() == ERROR_FILE_NOT_FOUND => {
                    return Ok(ProcessObservation::MatchingUnowned);
                }
                Err(_) => return Err(ReconcileError::ProcessInspectionFailed),
            }
        };
        let mut belongs = BOOL::default();
        unsafe {
            IsProcessInJob(process.0, job.0, &mut belongs)
                .map_err(|_| ReconcileError::ProcessInspectionFailed)?;
        }
        Ok(if belongs.as_bool() {
            ProcessObservation::MatchingOwned
        } else {
            ProcessObservation::MatchingUnowned
        })
    }
}

#[cfg(windows)]
struct InspectorHandle(windows::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for InspectorHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
fn filetime_to_unix_ms(value: windows::Win32::Foundation::FILETIME) -> Result<i64, ReconcileError> {
    const WINDOWS_TO_UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;
    const TICKS_PER_MILLISECOND: u64 = 10_000;

    let ticks = (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime);
    let unix_ticks = ticks
        .checked_sub(WINDOWS_TO_UNIX_EPOCH_TICKS)
        .ok_or(ReconcileError::ProcessInspectionFailed)?;
    i64::try_from(unix_ticks / TICKS_PER_MILLISECOND)
        .map_err(|_| ReconcileError::ProcessInspectionFailed)
}

#[cfg(windows)]
fn hex_digest(digest: &[u8; 32]) -> String {
    use std::fmt::Write;

    let mut encoded = String::with_capacity(64);
    for byte in digest {
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

#[derive(Clone, Debug)]
pub struct ReconcileConfig {
    retry_delay_ms: i64,
    scan_limit: usize,
}

impl ReconcileConfig {
    pub fn new(retry_delay_ms: i64, scan_limit: usize) -> Result<Self, ReconcileError> {
        if retry_delay_ms <= 0 || scan_limit == 0 || scan_limit > 1_000 {
            return Err(ReconcileError::ConfigInvalid);
        }
        Ok(Self {
            retry_delay_ms,
            scan_limit,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconcileOutcome {
    Idle,
    RecoveredCommit {
        work_item_id: String,
        prior_attempt_no: i64,
        recovery_point_id: String,
    },
    Requeued {
        work_item_id: String,
        prior_attempt_no: i64,
        reason: String,
    },
    CleanupRequired {
        work_item_id: String,
        prior_attempt_no: i64,
        reason: String,
    },
    AttentionRequired {
        work_item_id: String,
        prior_attempt_no: i64,
        reason: String,
    },
}

pub struct StartupReconciler {
    repository: RuntimeRepository,
    clock: Arc<dyn RuntimeClock>,
    config: ReconcileConfig,
    process_inspector: Option<Arc<dyn ProcessInspector>>,
    recovery_point_source: Option<Arc<dyn VerifiedRecoveryPointSource>>,
}

impl StartupReconciler {
    pub fn new(
        repository: RuntimeRepository,
        clock: Arc<dyn RuntimeClock>,
        config: ReconcileConfig,
    ) -> Self {
        Self {
            repository,
            clock,
            config,
            process_inspector: None,
            recovery_point_source: None,
        }
    }

    pub fn with_process_inspector(mut self, process_inspector: Arc<dyn ProcessInspector>) -> Self {
        self.process_inspector = Some(process_inspector);
        self
    }

    pub fn with_recovery_point_source(
        mut self,
        recovery_point_source: Arc<dyn VerifiedRecoveryPointSource>,
    ) -> Self {
        self.recovery_point_source = Some(recovery_point_source);
        self
    }

    pub fn reconcile_once(&self) -> Result<ReconcileOutcome, ReconcileError> {
        let now_ms = self.clock.now_ms();
        if now_ms < 0 {
            return Err(ReconcileError::ClockInvalid);
        }
        let Some(candidate) = self
            .repository
            .expired_claims(now_ms, self.config.scan_limit)?
            .into_iter()
            .next()
        else {
            return Ok(ReconcileOutcome::Idle);
        };
        self.reconcile_candidate(&candidate, now_ms)
    }

    /// Reconciles one bounded snapshot of expired claims without allowing an
    /// attention-only claim at the front of the queue to starve later claims.
    pub fn reconcile_batch(&self) -> Result<Vec<ReconcileOutcome>, ReconcileError> {
        let now_ms = self.clock.now_ms();
        if now_ms < 0 {
            return Err(ReconcileError::ClockInvalid);
        }
        let candidates = self
            .repository
            .expired_claims(now_ms, self.config.scan_limit)?;
        if candidates.is_empty() {
            return Ok(vec![ReconcileOutcome::Idle]);
        }
        candidates
            .iter()
            .map(|candidate| self.reconcile_candidate(candidate, now_ms))
            .collect()
    }

    fn reconcile_candidate(
        &self,
        candidate: &ExpiredClaim,
        now_ms: i64,
    ) -> Result<ReconcileOutcome, ReconcileError> {
        let mut process_gone_confirmed = false;
        let process_reason = if let Some(recorded) = candidate.process() {
            let Some(inspector) = self.process_inspector.as_ref() else {
                return Ok(ReconcileOutcome::AttentionRequired {
                    work_item_id: candidate.claim().work_item_id().to_string(),
                    prior_attempt_no: candidate.claim().attempt_no(),
                    reason: "process_inspection_required".to_owned(),
                });
            };
            let observation = inspector.inspect(&ProcessProbe::from(recorded))?;
            match observation {
                ProcessObservation::MatchingOwned => {
                    return Ok(ReconcileOutcome::AttentionRequired {
                        work_item_id: candidate.claim().work_item_id().to_string(),
                        prior_attempt_no: candidate.claim().attempt_no(),
                        reason: "process_still_running_owned".to_owned(),
                    });
                }
                ProcessObservation::MatchingUnowned => {
                    return Ok(ReconcileOutcome::AttentionRequired {
                        work_item_id: candidate.claim().work_item_id().to_string(),
                        prior_attempt_no: candidate.claim().attempt_no(),
                        reason: "process_still_running_unowned".to_owned(),
                    });
                }
                ProcessObservation::Missing => {
                    process_gone_confirmed = true;
                    "process_missing"
                }
                ProcessObservation::Reused => {
                    process_gone_confirmed = true;
                    "process_reused"
                }
                ProcessObservation::Exited => {
                    process_gone_confirmed = true;
                    "process_exited_uncommitted"
                }
            }
            .to_owned()
        } else {
            "process_missing".to_owned()
        };
        if candidate.claim().kind() == WorkItemKind::Backup {
            if candidate.work_state() != "committing" && candidate.attempt_state() != "committing" {
                let available_at_ms = now_ms
                    .checked_add(self.config.retry_delay_ms)
                    .ok_or(ReconcileError::ArithmeticOverflow)?;
                if candidate.process().is_some() {
                    self.repository.reconcile_expired_process_gone(
                        candidate,
                        now_ms,
                        available_at_ms,
                    )?;
                } else {
                    self.repository.reconcile_expired_without_process(
                        candidate,
                        now_ms,
                        available_at_ms,
                    )?;
                }
                return Ok(ReconcileOutcome::Requeued {
                    work_item_id: candidate.claim().work_item_id().to_string(),
                    prior_attempt_no: candidate.claim().attempt_no(),
                    reason: process_reason,
                });
            }
            let Some(source) = self.recovery_point_source.as_ref() else {
                return Ok(ReconcileOutcome::AttentionRequired {
                    work_item_id: candidate.claim().work_item_id().to_string(),
                    prior_attempt_no: candidate.claim().attempt_no(),
                    reason: "subject_evidence_required".to_owned(),
                });
            };
            let Some(spec) = source.verified_complete(candidate.claim())? else {
                return Ok(ReconcileOutcome::AttentionRequired {
                    work_item_id: candidate.claim().work_item_id().to_string(),
                    prior_attempt_no: candidate.claim().attempt_no(),
                    reason: "complete_manifest_not_found".to_owned(),
                });
            };
            let recovery_point_id = spec.id().to_string();
            self.repository.reconcile_expired_backup_commit(
                candidate,
                spec,
                now_ms,
                process_gone_confirmed,
            )?;
            return Ok(ReconcileOutcome::RecoveredCommit {
                work_item_id: candidate.claim().work_item_id().to_string(),
                prior_attempt_no: candidate.claim().attempt_no(),
                recovery_point_id,
            });
        }
        if candidate.claim().kind() == WorkItemKind::Restore {
            let available_at_ms = now_ms
                .checked_add(self.config.retry_delay_ms)
                .ok_or(ReconcileError::ArithmeticOverflow)?;
            if candidate.process().is_some() {
                self.repository.reconcile_expired_process_gone(
                    candidate,
                    now_ms,
                    available_at_ms,
                )?;
            } else {
                self.repository.reconcile_expired_without_process(
                    candidate,
                    now_ms,
                    available_at_ms,
                )?;
            }
            if !candidate.restore_destructive_step_started() {
                return Ok(ReconcileOutcome::Requeued {
                    work_item_id: candidate.claim().work_item_id().to_string(),
                    prior_attempt_no: candidate.claim().attempt_no(),
                    reason: process_reason,
                });
            }
            return Ok(ReconcileOutcome::CleanupRequired {
                work_item_id: candidate.claim().work_item_id().to_string(),
                prior_attempt_no: candidate.claim().attempt_no(),
                reason: "restore_crash_target_preserved".to_owned(),
            });
        }
        if candidate.claim().kind() != WorkItemKind::Operation {
            return Ok(ReconcileOutcome::AttentionRequired {
                work_item_id: candidate.claim().work_item_id().to_string(),
                prior_attempt_no: candidate.claim().attempt_no(),
                reason: "subject_evidence_required".to_owned(),
            });
        }
        let available_at_ms = now_ms
            .checked_add(self.config.retry_delay_ms)
            .ok_or(ReconcileError::ArithmeticOverflow)?;
        if candidate.process().is_some() {
            self.repository
                .reconcile_expired_process_gone(candidate, now_ms, available_at_ms)?;
        } else {
            self.repository.reconcile_expired_without_process(
                candidate,
                now_ms,
                available_at_ms,
            )?;
        }
        Ok(ReconcileOutcome::Requeued {
            work_item_id: candidate.claim().work_item_id().to_string(),
            prior_attempt_no: candidate.claim().attempt_no(),
            reason: process_reason,
        })
    }
}
