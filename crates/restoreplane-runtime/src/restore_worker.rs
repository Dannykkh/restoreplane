use std::sync::Arc;

use async_trait::async_trait;
use restoreplane_catalog::{
    BeginCommitDisposition, ClaimCompletionSpec, ClaimRequest, ClaimTerminalOutcome, ClaimedWork,
    RepositoryError, RestoreClaimContext, RetryDisposition, RuntimeRepository,
};
use restoreplane_domain::WorkItemKind;
use serde_json::{json, Map, Value};
use thiserror::Error;

use crate::RuntimeClock;

const MAX_EVIDENCE_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug)]
pub struct RestoreWorkerConfig {
    worker_id: String,
    worker_boot_id: String,
    lease_duration_ms: i64,
    retry_backoff_ms: i64,
}

impl RestoreWorkerConfig {
    pub fn new(
        worker_id: impl Into<String>,
        worker_boot_id: impl Into<String>,
        lease_duration_ms: i64,
        retry_backoff_ms: i64,
    ) -> Result<Self, RestoreWorkerError> {
        let value = Self {
            worker_id: worker_id.into(),
            worker_boot_id: worker_boot_id.into(),
            lease_duration_ms,
            retry_backoff_ms,
        };
        if value.worker_id.is_empty()
            || value.worker_boot_id.is_empty()
            || value.lease_duration_ms <= 0
            || value.retry_backoff_ms < 0
        {
            return Err(RestoreWorkerError::ContractInvalid);
        }
        Ok(value)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RestoreExecutionEvidence {
    native_exit_code: i32,
    target_created: bool,
    evidence: Value,
}

impl RestoreExecutionEvidence {
    pub fn completed(
        native_exit_code: i32,
        target_created: bool,
        evidence: Value,
    ) -> Result<Self, RestoreExecutionError> {
        if native_exit_code != 0 || !target_created || !safe_evidence(&evidence) {
            return Err(RestoreExecutionError {
                code: "RESTORE_EXECUTION_EVIDENCE_INVALID".to_owned(),
                retryable: false,
                target_created,
                native_exit_code: Some(native_exit_code),
                evidence: json!({}),
            });
        }
        Ok(Self {
            native_exit_code,
            target_created,
            evidence,
        })
    }

    pub const fn native_exit_code(&self) -> i32 {
        self.native_exit_code
    }

    pub const fn target_created(&self) -> bool {
        self.target_created
    }

    pub const fn evidence(&self) -> &Value {
        &self.evidence
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RestoreExecutionError {
    code: String,
    retryable: bool,
    target_created: bool,
    native_exit_code: Option<i32>,
    evidence: Value,
}

impl RestoreExecutionError {
    pub fn retryable(code: impl Into<String>, evidence: Value) -> Result<Self, RestoreWorkerError> {
        Self::new(code, true, false, None, evidence)
    }

    pub fn terminal(
        code: impl Into<String>,
        target_created: bool,
        native_exit_code: Option<i32>,
        evidence: Value,
    ) -> Result<Self, RestoreWorkerError> {
        Self::new(code, false, target_created, native_exit_code, evidence)
    }

    fn new(
        code: impl Into<String>,
        retryable: bool,
        target_created: bool,
        native_exit_code: Option<i32>,
        evidence: Value,
    ) -> Result<Self, RestoreWorkerError> {
        let code = code.into();
        if !valid_error_code(&code) || !safe_evidence(&evidence) || (retryable && target_created) {
            return Err(RestoreWorkerError::ContractInvalid);
        }
        Ok(Self {
            code,
            retryable,
            target_created,
            native_exit_code,
            evidence,
        })
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    pub const fn is_retryable(&self) -> bool {
        self.retryable
    }

    pub const fn target_created(&self) -> bool {
        self.target_created
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RestoreValidationEvidence {
    evidence: Value,
}

impl RestoreValidationEvidence {
    pub fn passed(evidence: Value) -> Result<Self, RestoreValidationError> {
        if !safe_evidence(&evidence) {
            return Err(RestoreValidationError {
                code: "RESTORE_VALIDATION_EVIDENCE_INVALID".to_owned(),
                evidence: json!({}),
            });
        }
        Ok(Self { evidence })
    }

    pub const fn evidence(&self) -> &Value {
        &self.evidence
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RestoreValidationError {
    code: String,
    evidence: Value,
}

impl RestoreValidationError {
    pub fn failed(code: impl Into<String>, evidence: Value) -> Result<Self, RestoreWorkerError> {
        let code = code.into();
        if !valid_error_code(&code) || !safe_evidence(&evidence) {
            return Err(RestoreWorkerError::ContractInvalid);
        }
        Ok(Self { code, evidence })
    }

    pub fn code(&self) -> &str {
        &self.code
    }
}

#[async_trait]
pub trait RestoreExecutor: Send + Sync {
    async fn admit(
        &self,
        _claim: &ClaimedWork,
        _context: &RestoreClaimContext,
    ) -> Result<RestoreAdmission, RestoreExecutionError> {
        Ok(RestoreAdmission::Admitted)
    }

    async fn restore(
        &self,
        claim: &ClaimedWork,
        context: &RestoreClaimContext,
    ) -> Result<RestoreExecutionEvidence, RestoreExecutionError>;

    async fn validate(
        &self,
        claim: &ClaimedWork,
        context: &RestoreClaimContext,
        restore: &RestoreExecutionEvidence,
    ) -> Result<RestoreValidationEvidence, RestoreValidationError>;

    fn release_claim_resources(&self, _claim: &ClaimedWork, _context: &RestoreClaimContext) {}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestoreAdmission {
    Admitted,
    Deferred { wait_reason: String },
}

impl RestoreAdmission {
    pub fn deferred(wait_reason: impl Into<String>) -> Result<Self, RestoreWorkerError> {
        let wait_reason = wait_reason.into();
        if wait_reason.is_empty()
            || wait_reason.len() > 1_024
            || wait_reason.contains(['\0', '\r', '\n'])
        {
            return Err(RestoreWorkerError::ContractInvalid);
        }
        Ok(Self::Deferred { wait_reason })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestoreWorkerOutcome {
    Idle,
    Deferred {
        restore_run_id: String,
        wait_reason: String,
        available_at_ms: i64,
    },
    Succeeded {
        restore_run_id: String,
        measured_rto_ms: i64,
    },
    RetryScheduled {
        restore_run_id: String,
        error_code: String,
        available_at_ms: i64,
    },
    Failed {
        restore_run_id: String,
        error_code: String,
        cleanup_required: bool,
    },
    Cancelled {
        restore_run_id: String,
        cleanup_required: bool,
    },
}

#[derive(Debug, Error)]
pub enum RestoreWorkerError {
    #[error("RESTORE_WORKER_CONTRACT_INVALID")]
    ContractInvalid,
    #[error("RESTORE_WORKER_CLOCK_INVALID")]
    ClockInvalid,
    #[error("RESTORE_WORKER_EVIDENCE_INVALID")]
    EvidenceInvalid,
    #[error("{0}")]
    Repository(#[from] RepositoryError),
}

impl RestoreWorkerError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::ContractInvalid => "RESTORE_WORKER_CONTRACT_INVALID",
            Self::ClockInvalid => "RESTORE_WORKER_CLOCK_INVALID",
            Self::EvidenceInvalid => "RESTORE_WORKER_EVIDENCE_INVALID",
            Self::Repository(error) => error.code(),
        }
    }
}

pub struct RestoreWorker {
    repository: RuntimeRepository,
    executor: Arc<dyn RestoreExecutor>,
    clock: Arc<dyn RuntimeClock>,
    config: RestoreWorkerConfig,
}

impl RestoreWorker {
    pub fn new(
        repository: RuntimeRepository,
        executor: Arc<dyn RestoreExecutor>,
        clock: Arc<dyn RuntimeClock>,
        config: RestoreWorkerConfig,
    ) -> Self {
        Self {
            repository,
            executor,
            clock,
            config,
        }
    }

    pub async fn run_once(&self) -> Result<RestoreWorkerOutcome, RestoreWorkerError> {
        let claim_at_ms = self.now_ms()?;
        let Some(claim) = self.repository.claim_next(
            ClaimRequest::new(
                &self.config.worker_id,
                &self.config.worker_boot_id,
                claim_at_ms,
                self.config.lease_duration_ms,
            )
            .for_kind(WorkItemKind::Restore),
        )?
        else {
            return Ok(RestoreWorkerOutcome::Idle);
        };
        let context_at_ms = self.now_ms()?;
        let context = self
            .repository
            .restore_claim_context(&claim, context_at_ms)?;
        let restore_run_id = context.restore_run_id().to_string();
        if !context.authoritatively_eligible() {
            let completed_at_ms = self.now_ms()?;
            let evidence = terminal_evidence(
                "preflight",
                None,
                false,
                json!({}),
                "not_run",
                json!({}),
                false,
                Some("RESTORE_PREFLIGHT_STALE"),
            )?;
            self.repository.complete_claim(ClaimCompletionSpec::failed(
                claim,
                completed_at_ms,
                "RESTORE_PREFLIGHT_STALE",
                Some(evidence),
            ))?;
            return Ok(RestoreWorkerOutcome::Failed {
                restore_run_id,
                error_code: "RESTORE_PREFLIGHT_STALE".to_owned(),
                cleanup_required: false,
            });
        }

        match self.executor.admit(&claim, &context).await {
            Ok(RestoreAdmission::Admitted) => {}
            Ok(RestoreAdmission::Deferred { wait_reason }) => {
                let deferred_at_ms = self.now_ms()?;
                let available_at_ms = deferred_at_ms
                    .checked_add(self.config.retry_backoff_ms)
                    .ok_or(RestoreWorkerError::ClockInvalid)?;
                self.repository.defer_claim(
                    &claim,
                    deferred_at_ms,
                    available_at_ms,
                    &wait_reason,
                )?;
                return Ok(RestoreWorkerOutcome::Deferred {
                    restore_run_id,
                    wait_reason,
                    available_at_ms,
                });
            }
            Err(error) => {
                let completed_at_ms = self.now_ms()?;
                let evidence = terminal_evidence(
                    "preflight",
                    error.native_exit_code,
                    false,
                    error.evidence.clone(),
                    "not_run",
                    json!({}),
                    false,
                    Some(error.code.as_str()),
                )?;
                if error.is_retryable() {
                    let available_at_ms = completed_at_ms
                        .checked_add(self.config.retry_backoff_ms)
                        .ok_or(RestoreWorkerError::ClockInvalid)?;
                    return match self.repository.retry_claim(
                        &claim,
                        completed_at_ms,
                        available_at_ms,
                        error.code(),
                        Some(evidence),
                    )? {
                        RetryDisposition::Requeued => Ok(RestoreWorkerOutcome::RetryScheduled {
                            restore_run_id,
                            error_code: error.code,
                            available_at_ms,
                        }),
                        RetryDisposition::Exhausted => Ok(RestoreWorkerOutcome::Failed {
                            restore_run_id,
                            error_code: "RETRY_BUDGET_EXHAUSTED".to_owned(),
                            cleanup_required: false,
                        }),
                        RetryDisposition::Cancelled => Ok(RestoreWorkerOutcome::Cancelled {
                            restore_run_id,
                            cleanup_required: false,
                        }),
                    };
                }
                self.repository.complete_claim(ClaimCompletionSpec::failed(
                    claim,
                    completed_at_ms,
                    error.code(),
                    Some(evidence),
                ))?;
                return Ok(RestoreWorkerOutcome::Failed {
                    restore_run_id,
                    error_code: error.code,
                    cleanup_required: false,
                });
            }
        }

        let started_at_ms = self.now_ms()?;
        self.repository.start_claim(&claim, started_at_ms)?;
        let running_at_ms = self.now_ms()?;
        self.repository
            .mark_in_process_claim_running(&claim, running_at_ms)?;
        if self
            .repository
            .claim_cancel_requested(&claim, running_at_ms)?
        {
            return self.cancel(&claim, &context, &restore_run_id, None);
        }

        let execution = match self.executor.restore(&claim, &context).await {
            Ok(execution) => execution,
            Err(error) => {
                let completed_at_ms = self.now_ms()?;
                if error.code == "RECOVERY_CANCEL_REQUESTED"
                    && self
                        .repository
                        .claim_cancel_requested(&claim, completed_at_ms)?
                {
                    return self.cancel_with_details(
                        &claim,
                        &context,
                        &restore_run_id,
                        completed_at_ms,
                        error.native_exit_code,
                        error.target_created,
                        error.evidence,
                    );
                }
                let evidence = terminal_evidence(
                    "restore",
                    error.native_exit_code,
                    error.target_created,
                    error.evidence.clone(),
                    "not_run",
                    json!({}),
                    error.target_created,
                    Some(error.code.as_str()),
                )?;
                if error.is_retryable() {
                    let available_at_ms = completed_at_ms
                        .checked_add(self.config.retry_backoff_ms)
                        .ok_or(RestoreWorkerError::ClockInvalid)?;
                    return match self.repository.retry_claim(
                        &claim,
                        completed_at_ms,
                        available_at_ms,
                        error.code(),
                        Some(evidence),
                    )? {
                        RetryDisposition::Requeued => Ok(RestoreWorkerOutcome::RetryScheduled {
                            restore_run_id,
                            error_code: error.code,
                            available_at_ms,
                        }),
                        RetryDisposition::Exhausted => Ok(RestoreWorkerOutcome::Failed {
                            restore_run_id,
                            error_code: "RETRY_BUDGET_EXHAUSTED".to_owned(),
                            cleanup_required: false,
                        }),
                        RetryDisposition::Cancelled => Ok(RestoreWorkerOutcome::Cancelled {
                            restore_run_id,
                            cleanup_required: false,
                        }),
                    };
                }
                self.repository.complete_claim(ClaimCompletionSpec::failed(
                    claim,
                    completed_at_ms,
                    error.code(),
                    Some(evidence),
                ))?;
                return Ok(RestoreWorkerOutcome::Failed {
                    restore_run_id,
                    error_code: error.code,
                    cleanup_required: error.target_created,
                });
            }
        };

        let restored_at_ms = self.now_ms()?;
        if self
            .repository
            .claim_cancel_requested(&claim, restored_at_ms)?
        {
            return self.cancel(&claim, &context, &restore_run_id, Some(&execution));
        }
        self.repository
            .begin_restore_validation(&claim, restored_at_ms)?;
        let validation = match self.executor.validate(&claim, &context, &execution).await {
            Ok(validation) => validation,
            Err(error) => {
                let completed_at_ms = self.now_ms()?;
                if error.code() == "RECOVERY_CANCEL_REQUESTED"
                    && self
                        .repository
                        .claim_cancel_requested(&claim, completed_at_ms)?
                {
                    return self.cancel_with_details(
                        &claim,
                        &context,
                        &restore_run_id,
                        completed_at_ms,
                        Some(execution.native_exit_code),
                        execution.target_created,
                        execution.evidence,
                    );
                }
                let evidence = terminal_evidence(
                    "validation",
                    Some(execution.native_exit_code),
                    execution.target_created,
                    execution.evidence,
                    "failed",
                    error.evidence.clone(),
                    true,
                    Some(error.code.as_str()),
                )?;
                self.repository.complete_claim(ClaimCompletionSpec::failed(
                    claim,
                    completed_at_ms,
                    error.code(),
                    Some(evidence),
                ))?;
                return Ok(RestoreWorkerOutcome::Failed {
                    restore_run_id,
                    error_code: error.code,
                    cleanup_required: true,
                });
            }
        };

        let commit_at_ms = self.now_ms()?;
        if self.repository.begin_claim_commit(&claim, commit_at_ms)?
            == BeginCommitDisposition::CancelRequested
        {
            return self.cancel(&claim, &context, &restore_run_id, Some(&execution));
        }
        let completed_at_ms = self.now_ms()?;
        let evidence = terminal_evidence(
            "completed",
            Some(execution.native_exit_code),
            execution.target_created,
            execution.evidence,
            "passed",
            validation.evidence,
            false,
            None,
        )?;
        let outcome = self
            .repository
            .complete_claim(ClaimCompletionSpec::succeeded(
                claim,
                completed_at_ms,
                Some(evidence),
            ))?;
        if outcome == ClaimTerminalOutcome::Cancelled {
            Ok(RestoreWorkerOutcome::Cancelled {
                restore_run_id,
                cleanup_required: true,
            })
        } else {
            Ok(RestoreWorkerOutcome::Succeeded {
                restore_run_id,
                measured_rto_ms: completed_at_ms
                    .checked_sub(started_at_ms)
                    .ok_or(RestoreWorkerError::ClockInvalid)?,
            })
        }
    }

    fn cancel(
        &self,
        claim: &restoreplane_catalog::ClaimedWork,
        context: &RestoreClaimContext,
        restore_run_id: &str,
        execution: Option<&RestoreExecutionEvidence>,
    ) -> Result<RestoreWorkerOutcome, RestoreWorkerError> {
        let completed_at_ms = self.now_ms()?;
        let target_created = execution.is_some_and(RestoreExecutionEvidence::target_created);
        self.cancel_with_details(
            claim,
            context,
            restore_run_id,
            completed_at_ms,
            execution.map(RestoreExecutionEvidence::native_exit_code),
            target_created,
            execution
                .map(|value| value.evidence.clone())
                .unwrap_or_else(|| json!({})),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn cancel_with_details(
        &self,
        claim: &restoreplane_catalog::ClaimedWork,
        context: &RestoreClaimContext,
        restore_run_id: &str,
        completed_at_ms: i64,
        native_exit_code: Option<i32>,
        target_created: bool,
        restore_evidence: Value,
    ) -> Result<RestoreWorkerOutcome, RestoreWorkerError> {
        self.executor.release_claim_resources(claim, context);
        let evidence = terminal_evidence(
            "cancelled",
            native_exit_code,
            target_created,
            restore_evidence,
            "not_run",
            json!({}),
            target_created,
            Some("CANCEL_REQUESTED"),
        )?;
        self.repository
            .complete_claim(ClaimCompletionSpec::cancelled(
                claim.clone(),
                completed_at_ms,
                Some("CANCEL_REQUESTED".to_owned()),
                Some(evidence),
            ))?;
        Ok(RestoreWorkerOutcome::Cancelled {
            restore_run_id: restore_run_id.to_owned(),
            cleanup_required: target_created,
        })
    }

    fn now_ms(&self) -> Result<i64, RestoreWorkerError> {
        let value = self.clock.now_ms();
        if value < 0 {
            return Err(RestoreWorkerError::ClockInvalid);
        }
        Ok(value)
    }
}

#[allow(clippy::too_many_arguments)]
fn terminal_evidence(
    phase: &str,
    native_exit_code: Option<i32>,
    target_created: bool,
    restore_evidence: Value,
    validation_status: &str,
    validation_evidence: Value,
    cleanup_required: bool,
    cleanup_reason: Option<&str>,
) -> Result<String, RestoreWorkerError> {
    if !matches!(
        phase,
        "preflight" | "restore" | "validation" | "cancelled" | "completed" | "reconcile"
    ) || !matches!(validation_status, "passed" | "failed" | "not_run")
        || cleanup_required != target_created && cleanup_required
        || !safe_evidence(&restore_evidence)
        || !safe_evidence(&validation_evidence)
    {
        return Err(RestoreWorkerError::EvidenceInvalid);
    }
    let value = json!({
        "schemaVersion": 1,
        "phase": phase,
        "restore": {
            "nativeExitCode": native_exit_code,
            "targetCreated": target_created,
            "evidence": restore_evidence,
        },
        "validation": {
            "status": validation_status,
            "evidence": validation_evidence,
        },
        "cleanup": {
            "required": cleanup_required,
            "reason": cleanup_reason,
            "targetPreserved": target_created,
        }
    });
    let encoded = serde_json::to_string(&value).map_err(|_| RestoreWorkerError::EvidenceInvalid)?;
    if encoded.len() > MAX_EVIDENCE_BYTES {
        return Err(RestoreWorkerError::EvidenceInvalid);
    }
    Ok(encoded)
}

fn safe_evidence(value: &Value) -> bool {
    if !value.is_object()
        || serde_json::to_vec(value)
            .map(|bytes| bytes.len() > MAX_EVIDENCE_BYTES)
            .unwrap_or(true)
    {
        return false;
    }
    safe_evidence_value(value)
}

fn safe_evidence_value(value: &Value) -> bool {
    match value {
        Value::Object(values) => safe_evidence_object(values),
        Value::Array(values) => values.len() <= 10_000 && values.iter().all(safe_evidence_value),
        Value::String(value) => {
            value.len() <= 4 * 1024 && !value.contains('\0') && !contains_connection_secret(value)
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => true,
    }
}

fn safe_evidence_object(values: &Map<String, Value>) -> bool {
    values.len() <= 1_000
        && values.iter().all(|(key, value)| {
            let normalized = key.to_ascii_lowercase().replace(['-', '_'], "");
            ![
                "password",
                "passwd",
                "pwd",
                "secret",
                "token",
                "credential",
                "authorization",
                "privatekey",
                "connectionstring",
            ]
            .iter()
            .any(|forbidden| normalized.contains(forbidden))
                && safe_evidence_value(value)
        })
}

fn contains_connection_secret(value: &str) -> bool {
    let normalized = value.to_ascii_lowercase();
    ["postgres://", "postgresql://", "mysql://", "mariadb://"]
        .iter()
        .any(|prefix| normalized.contains(prefix))
        || ["password=", "passwd=", "pwd=", "token="]
            .iter()
            .any(|needle| normalized.contains(needle))
}

fn valid_error_code(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}
