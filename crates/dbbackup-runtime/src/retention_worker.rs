use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dbbackup_catalog::{
    ClaimedWork, DestructiveStepDisposition, RepositoryError, RetentionDeleteSettlementOutcome,
    RetentionDeleteSettlementSpec, RuntimeRepository,
};
use dbbackup_pipeline::{execute_retention_delete_v2, DeleteErrorV2, DestinationDeleteV2};
use thiserror::Error;
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::dispatcher::RuntimeClock;

const MAX_RESOLUTION_DIAGNOSTIC_BYTES: usize = 4 * 1024;
const MAX_DEFAULT_REMOTE_DELETE_DEADLINE: Duration = Duration::from_secs(20);
const DEADLINE_EXCEEDED_CODE: &str = "RETENTION_DELETE_DEADLINE_EXCEEDED";
const SHUTDOWN_REQUESTED_CODE: &str = "RETENTION_DELETE_SHUTDOWN_REQUESTED";

#[derive(Clone, Debug, Default)]
pub struct RetentionDeleteCancellationToken {
    state: Arc<RetentionDeleteCancellationState>,
}

#[derive(Debug, Default)]
struct RetentionDeleteCancellationState {
    cancelled: AtomicBool,
    notify: Notify,
}

impl RetentionDeleteCancellationToken {
    pub fn cancel(&self) {
        if !self.state.cancelled.swap(true, Ordering::AcqRel) {
            self.state.notify.notify_waiters();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
    }

    pub async fn cancelled(&self) {
        loop {
            let notified = self.state.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

pub trait RetentionDeleteOperatorResolverV2: Send + Sync {
    fn resolve(
        &self,
        destination_fingerprint: &str,
        delete_secret_ref_id: Option<&str>,
    ) -> Result<Box<dyn DestinationDeleteV2>, RetentionDeleteResolutionErrorV2>;
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("{message}")]
pub struct RetentionDeleteResolutionErrorV2 {
    message: String,
    retryable: bool,
}

impl RetentionDeleteResolutionErrorV2 {
    pub fn retryable(message: impl Into<String>) -> Result<Self, RetentionDeleteWorkerErrorV2> {
        Self::new(message, true)
    }

    pub fn terminal(message: impl Into<String>) -> Result<Self, RetentionDeleteWorkerErrorV2> {
        Self::new(message, false)
    }

    fn new(
        message: impl Into<String>,
        retryable: bool,
    ) -> Result<Self, RetentionDeleteWorkerErrorV2> {
        let message = message.into();
        if message.trim().is_empty() || message.len() > MAX_RESOLUTION_DIAGNOSTIC_BYTES {
            return Err(RetentionDeleteWorkerErrorV2::InputInvalid);
        }
        Ok(Self { message, retryable })
    }
}

#[derive(Debug, Error)]
pub enum RetentionDeleteWorkerErrorV2 {
    #[error("RETENTION_DELETE_WORKER_INPUT_INVALID")]
    InputInvalid,
    #[error(transparent)]
    Repository(#[from] RepositoryError),
}

impl RetentionDeleteWorkerErrorV2 {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InputInvalid => "RETENTION_DELETE_WORKER_INPUT_INVALID",
            Self::Repository(error) => error.code(),
        }
    }
}

pub struct RetentionDeleteWorkerV2 {
    repository: RuntimeRepository,
    resolver: Arc<dyn RetentionDeleteOperatorResolverV2>,
    clock: Arc<dyn RuntimeClock>,
    lease_duration_ms: i64,
    retry_delay_ms: i64,
    remote_delete_deadline: Duration,
}

impl RetentionDeleteWorkerV2 {
    pub fn new(
        repository: RuntimeRepository,
        resolver: Arc<dyn RetentionDeleteOperatorResolverV2>,
        clock: Arc<dyn RuntimeClock>,
        lease_duration_ms: i64,
        retry_delay_ms: i64,
    ) -> Result<Self, RetentionDeleteWorkerErrorV2> {
        if lease_duration_ms <= 0 || retry_delay_ms <= 0 {
            return Err(RetentionDeleteWorkerErrorV2::InputInvalid);
        }
        Ok(Self {
            repository,
            resolver,
            clock,
            lease_duration_ms,
            retry_delay_ms,
            remote_delete_deadline: default_remote_delete_deadline(lease_duration_ms),
        })
    }

    pub fn with_remote_delete_deadline(
        mut self,
        remote_delete_deadline: Duration,
    ) -> Result<Self, RetentionDeleteWorkerErrorV2> {
        if remote_delete_deadline.is_zero()
            || remote_delete_deadline.as_millis() > self.lease_duration_ms as u128
        {
            return Err(RetentionDeleteWorkerErrorV2::InputInvalid);
        }
        self.remote_delete_deadline = remote_delete_deadline;
        Ok(self)
    }

    pub async fn execute(
        &self,
        claim: &ClaimedWork,
    ) -> Result<RetentionDeleteSettlementOutcome, RetentionDeleteWorkerErrorV2> {
        self.execute_with_cancellation(claim, &RetentionDeleteCancellationToken::default())
            .await
    }

    pub async fn execute_with_cancellation(
        &self,
        claim: &ClaimedWork,
        cancellation: &RetentionDeleteCancellationToken,
    ) -> Result<RetentionDeleteSettlementOutcome, RetentionDeleteWorkerErrorV2> {
        let started_at_ms = self.clock.now_ms();
        if started_at_ms < 0 {
            return Err(RetentionDeleteWorkerErrorV2::InputInvalid);
        }
        let remote_delete_deadline = Instant::now()
            .checked_add(self.remote_delete_deadline)
            .ok_or(RetentionDeleteWorkerErrorV2::InputInvalid)?;
        let execution = self
            .repository
            .load_claimed_retention_delete_v2(claim, started_at_ms)?;
        self.repository.start_claim(claim, started_at_ms)?;

        let destination_operator: Box<dyn DestinationDeleteV2> = match self.resolver.resolve(
            execution.destination_fingerprint(),
            execution.delete_secret_ref_id(),
        ) {
            Ok(operator) => operator,
            Err(error) => Box::new(ResolutionFailureOperator { error }),
        };
        let operator = FencedDeleteOperator {
            repository: self.repository.clone(),
            claim: claim.clone(),
            clock: Arc::clone(&self.clock),
            lease_duration_ms: self.lease_duration_ms,
            destination_operator,
            cancellation: cancellation.clone(),
            remote_delete_deadline,
        };
        let report = execute_retention_delete_v2(execution.confirmed(), &operator).await;
        let completed_at_ms = self.clock.now_ms().max(started_at_ms);
        let retry_available_at_ms = completed_at_ms.saturating_add(self.retry_delay_ms);
        self.repository
            .settle_retention_delete(
                claim,
                RetentionDeleteSettlementSpec::new(
                    execution.recovery_point_id().clone(),
                    execution.destination_fingerprint(),
                    report,
                    completed_at_ms,
                    retry_available_at_ms,
                ),
            )
            .map_err(RetentionDeleteWorkerErrorV2::from)
    }
}

struct FencedDeleteOperator {
    repository: RuntimeRepository,
    claim: ClaimedWork,
    clock: Arc<dyn RuntimeClock>,
    lease_duration_ms: i64,
    destination_operator: Box<dyn DestinationDeleteV2>,
    cancellation: RetentionDeleteCancellationToken,
    remote_delete_deadline: Instant,
}

#[async_trait]
impl DestinationDeleteV2 for FencedDeleteOperator {
    async fn delete_object(&self, key: &str, version: Option<&str>) -> Result<(), DeleteErrorV2> {
        if self.cancellation.is_cancelled() {
            return Err(worker_gate_error(key, SHUTDOWN_REQUESTED_CODE, true));
        }
        if Instant::now() >= self.remote_delete_deadline {
            return Err(worker_gate_error(key, DEADLINE_EXCEEDED_CODE, true));
        }
        let heartbeat_at_ms = self.clock.now_ms();
        if heartbeat_at_ms < 0 {
            return Err(worker_gate_error(
                key,
                "RETENTION_DELETE_CLOCK_INVALID",
                false,
            ));
        }
        let lease_expires_at_ms = heartbeat_at_ms.saturating_add(self.lease_duration_ms);
        match self.repository.renew_destructive_step(
            &self.claim,
            heartbeat_at_ms,
            lease_expires_at_ms,
        ) {
            Ok(DestructiveStepDisposition::Proceed) => {}
            Ok(DestructiveStepDisposition::CancelRequested) => {
                return Err(worker_gate_error(
                    key,
                    "RETENTION_DELETE_CANCEL_REQUESTED",
                    false,
                ));
            }
            Err(error) => {
                return Err(worker_gate_error(key, error.code(), true));
            }
        }
        tokio::select! {
            biased;
            result = self.destination_operator.delete_object(key, version) => result,
            _ = self.cancellation.cancelled() => {
                Err(worker_gate_error(key, SHUTDOWN_REQUESTED_CODE, true))
            }
            _ = tokio::time::sleep_until(self.remote_delete_deadline) => {
                Err(worker_gate_error(key, DEADLINE_EXCEEDED_CODE, true))
            }
        }
    }
}

fn default_remote_delete_deadline(lease_duration_ms: i64) -> Duration {
    let lease_bounded = Duration::from_millis((lease_duration_ms / 2).max(1) as u64);
    lease_bounded.min(MAX_DEFAULT_REMOTE_DELETE_DEADLINE)
}

struct ResolutionFailureOperator {
    error: RetentionDeleteResolutionErrorV2,
}

fn worker_gate_error(key: &str, message: &str, retryable: bool) -> DeleteErrorV2 {
    DeleteErrorV2::Backend {
        key: key.to_owned(),
        message: message.to_owned(),
        retryable,
    }
}

#[async_trait]
impl DestinationDeleteV2 for ResolutionFailureOperator {
    async fn delete_object(&self, key: &str, _version: Option<&str>) -> Result<(), DeleteErrorV2> {
        Err(DeleteErrorV2::Backend {
            key: key.to_owned(),
            message: self.error.message.clone(),
            retryable: self.error.retryable,
        })
    }
}
