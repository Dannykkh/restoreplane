use std::sync::Arc;

use async_trait::async_trait;
use dbbackup_catalog::{
    BeginCommitDisposition, ClaimCompletionSpec, ClaimRequest, ClaimTerminalOutcome, ClaimedWork,
    RepositoryError, RetryDisposition, RuntimeRepository, VerificationClaimContext,
};
use dbbackup_domain::WorkItemKind;
use serde_json::Value;
use thiserror::Error;

use crate::RuntimeClock;

#[derive(Clone, Debug)]
pub struct VerificationWorkerConfig {
    worker_id: String,
    worker_boot_id: String,
    lease_duration_ms: i64,
    retry_backoff_ms: i64,
}

impl VerificationWorkerConfig {
    pub fn new(
        worker_id: impl Into<String>,
        worker_boot_id: impl Into<String>,
        lease_duration_ms: i64,
        retry_backoff_ms: i64,
    ) -> Result<Self, VerificationWorkerError> {
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
            return Err(VerificationWorkerError::ContractInvalid);
        }
        Ok(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerificationExecutionEvidence {
    evidence_json: String,
}

impl VerificationExecutionEvidence {
    pub fn new(evidence_json: impl Into<String>) -> Result<Self, VerificationWorkerError> {
        let evidence_json = evidence_json.into();
        let value: Value = serde_json::from_str(&evidence_json)
            .map_err(|_| VerificationWorkerError::EvidenceInvalid)?;
        if !value.is_object() {
            return Err(VerificationWorkerError::EvidenceInvalid);
        }
        Ok(Self { evidence_json })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerificationExecutionError {
    code: String,
    retryable: bool,
}

impl VerificationExecutionError {
    pub fn retryable(code: impl Into<String>) -> Result<Self, VerificationWorkerError> {
        Self::new(code, true)
    }

    pub fn terminal(code: impl Into<String>) -> Result<Self, VerificationWorkerError> {
        Self::new(code, false)
    }

    fn new(code: impl Into<String>, retryable: bool) -> Result<Self, VerificationWorkerError> {
        let code = code.into();
        if code.is_empty()
            || code.len() > 128
            || !code
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(VerificationWorkerError::ContractInvalid);
        }
        Ok(Self { code, retryable })
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    pub const fn is_retryable(&self) -> bool {
        self.retryable
    }
}

#[async_trait]
pub trait VerificationExecutor: Send + Sync {
    async fn admit(
        &self,
        _claim: &ClaimedWork,
        _context: &VerificationClaimContext,
    ) -> Result<VerificationAdmission, VerificationExecutionError> {
        Ok(VerificationAdmission::Admitted)
    }

    async fn execute(
        &self,
        claim: &ClaimedWork,
        context: &VerificationClaimContext,
    ) -> Result<VerificationExecutionEvidence, VerificationExecutionError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerificationAdmission {
    Admitted,
    Deferred { wait_reason: String },
}

impl VerificationAdmission {
    pub fn deferred(wait_reason: impl Into<String>) -> Result<Self, VerificationWorkerError> {
        let wait_reason = wait_reason.into();
        if wait_reason.is_empty()
            || wait_reason.len() > 1_024
            || wait_reason.contains(['\0', '\r', '\n'])
        {
            return Err(VerificationWorkerError::ContractInvalid);
        }
        Ok(Self::Deferred { wait_reason })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerificationWorkerOutcome {
    Idle,
    Deferred {
        verification_run_id: String,
        wait_reason: String,
        available_at_ms: i64,
    },
    Passed {
        verification_run_id: String,
        requested_level: i64,
    },
    RetryScheduled {
        verification_run_id: String,
        error_code: String,
        available_at_ms: i64,
    },
    Failed {
        verification_run_id: String,
        error_code: String,
    },
    Cancelled {
        verification_run_id: String,
    },
}

#[derive(Debug, Error)]
pub enum VerificationWorkerError {
    #[error("VERIFICATION_WORKER_CONTRACT_INVALID")]
    ContractInvalid,
    #[error("VERIFICATION_EVIDENCE_INVALID")]
    EvidenceInvalid,
    #[error("VERIFICATION_WORKER_CLOCK_INVALID")]
    ClockInvalid,
    #[error("{0}")]
    Repository(#[from] RepositoryError),
}

impl VerificationWorkerError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::ContractInvalid => "VERIFICATION_WORKER_CONTRACT_INVALID",
            Self::EvidenceInvalid => "VERIFICATION_EVIDENCE_INVALID",
            Self::ClockInvalid => "VERIFICATION_WORKER_CLOCK_INVALID",
            Self::Repository(error) => error.code(),
        }
    }
}

pub struct VerificationWorker {
    repository: RuntimeRepository,
    executor: Arc<dyn VerificationExecutor>,
    clock: Arc<dyn RuntimeClock>,
    config: VerificationWorkerConfig,
}

impl VerificationWorker {
    pub fn new(
        repository: RuntimeRepository,
        executor: Arc<dyn VerificationExecutor>,
        clock: Arc<dyn RuntimeClock>,
        config: VerificationWorkerConfig,
    ) -> Self {
        Self {
            repository,
            executor,
            clock,
            config,
        }
    }

    pub async fn run_once(&self) -> Result<VerificationWorkerOutcome, VerificationWorkerError> {
        let claim_time = self.now_ms()?;
        let Some(claim) = self.repository.claim_next(
            ClaimRequest::new(
                &self.config.worker_id,
                &self.config.worker_boot_id,
                claim_time,
                self.config.lease_duration_ms,
            )
            .for_kind(WorkItemKind::Verification),
        )?
        else {
            return Ok(VerificationWorkerOutcome::Idle);
        };
        let started_at_ms = self.now_ms()?;
        let context = self
            .repository
            .verification_claim_context(&claim, started_at_ms)?;
        match self.executor.admit(&claim, &context).await {
            Ok(VerificationAdmission::Admitted) => {}
            Ok(VerificationAdmission::Deferred { wait_reason }) => {
                let available_at_ms = started_at_ms
                    .checked_add(self.config.retry_backoff_ms)
                    .ok_or(VerificationWorkerError::ClockInvalid)?;
                self.repository.defer_claim(
                    &claim,
                    started_at_ms,
                    available_at_ms,
                    &wait_reason,
                )?;
                return Ok(VerificationWorkerOutcome::Deferred {
                    verification_run_id: context.verification_run_id().to_string(),
                    wait_reason,
                    available_at_ms,
                });
            }
            Err(error) if error.is_retryable() => {
                let available_at_ms = started_at_ms
                    .checked_add(self.config.retry_backoff_ms)
                    .ok_or(VerificationWorkerError::ClockInvalid)?;
                return match self.repository.retry_claim(
                    &claim,
                    started_at_ms,
                    available_at_ms,
                    error.code(),
                    None,
                )? {
                    RetryDisposition::Requeued => Ok(VerificationWorkerOutcome::RetryScheduled {
                        verification_run_id: context.verification_run_id().to_string(),
                        error_code: error.code,
                        available_at_ms,
                    }),
                    RetryDisposition::Exhausted => Ok(VerificationWorkerOutcome::Failed {
                        verification_run_id: context.verification_run_id().to_string(),
                        error_code: "RETRY_BUDGET_EXHAUSTED".to_owned(),
                    }),
                    RetryDisposition::Cancelled => Ok(VerificationWorkerOutcome::Cancelled {
                        verification_run_id: context.verification_run_id().to_string(),
                    }),
                };
            }
            Err(error) => {
                self.repository.complete_claim(ClaimCompletionSpec::failed(
                    claim,
                    started_at_ms,
                    error.code(),
                    None,
                ))?;
                return Ok(VerificationWorkerOutcome::Failed {
                    verification_run_id: context.verification_run_id().to_string(),
                    error_code: error.code,
                });
            }
        }
        self.repository.start_claim(&claim, started_at_ms)?;
        self.repository
            .mark_in_process_claim_running(&claim, started_at_ms)?;

        match self.executor.execute(&claim, &context).await {
            Ok(evidence) => {
                let commit_at_ms = self.now_ms()?;
                if self.repository.begin_claim_commit(&claim, commit_at_ms)?
                    == BeginCommitDisposition::CancelRequested
                {
                    let completed_at_ms = self.now_ms()?;
                    self.repository
                        .complete_claim(ClaimCompletionSpec::cancelled(
                            claim,
                            completed_at_ms,
                            Some("CANCEL_REQUESTED".to_owned()),
                            None,
                        ))?;
                    return Ok(VerificationWorkerOutcome::Cancelled {
                        verification_run_id: context.verification_run_id().to_string(),
                    });
                }
                let completed_at_ms = self.now_ms()?;
                let outcome = self
                    .repository
                    .complete_claim(ClaimCompletionSpec::succeeded(
                        claim,
                        completed_at_ms,
                        Some(evidence.evidence_json),
                    ))?;
                if outcome == ClaimTerminalOutcome::Cancelled {
                    Ok(VerificationWorkerOutcome::Cancelled {
                        verification_run_id: context.verification_run_id().to_string(),
                    })
                } else {
                    Ok(VerificationWorkerOutcome::Passed {
                        verification_run_id: context.verification_run_id().to_string(),
                        requested_level: context.requested_level(),
                    })
                }
            }
            Err(error) if error.is_retryable() => {
                let completed_at_ms = self.now_ms()?;
                let available_at_ms = completed_at_ms
                    .checked_add(self.config.retry_backoff_ms)
                    .ok_or(VerificationWorkerError::ClockInvalid)?;
                match self.repository.retry_claim(
                    &claim,
                    completed_at_ms,
                    available_at_ms,
                    error.code(),
                    None,
                )? {
                    RetryDisposition::Requeued => Ok(VerificationWorkerOutcome::RetryScheduled {
                        verification_run_id: context.verification_run_id().to_string(),
                        error_code: error.code,
                        available_at_ms,
                    }),
                    RetryDisposition::Exhausted => Ok(VerificationWorkerOutcome::Failed {
                        verification_run_id: context.verification_run_id().to_string(),
                        error_code: "RETRY_BUDGET_EXHAUSTED".to_owned(),
                    }),
                    RetryDisposition::Cancelled => Ok(VerificationWorkerOutcome::Cancelled {
                        verification_run_id: context.verification_run_id().to_string(),
                    }),
                }
            }
            Err(error) => {
                let completed_at_ms = self.now_ms()?;
                self.repository.complete_claim(ClaimCompletionSpec::failed(
                    claim,
                    completed_at_ms,
                    error.code(),
                    None,
                ))?;
                Ok(VerificationWorkerOutcome::Failed {
                    verification_run_id: context.verification_run_id().to_string(),
                    error_code: error.code,
                })
            }
        }
    }

    fn now_ms(&self) -> Result<i64, VerificationWorkerError> {
        let value = self.clock.now_ms();
        if value < 0 {
            return Err(VerificationWorkerError::ClockInvalid);
        }
        Ok(value)
    }
}
