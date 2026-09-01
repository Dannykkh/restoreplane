use std::future::Future;
use std::sync::Arc;

use async_trait::async_trait;
use ed25519_dalek::VerifyingKey;
use restoreplane_catalog::{
    ClaimCompletionSpec, ClaimTerminalOutcome, ClaimedWork, RepositoryError, RetryDisposition,
    RuntimeRepository,
};
use restoreplane_pipeline::{
    repair_replica_v2, RepairSourceV2, ReplicaEndpointV2, ReplicaPublishError, SignedSetManifestV2,
};
use thiserror::Error;
use tokio::time::Instant;

use crate::dispatcher::RuntimeClock;
use crate::retention_worker::RetentionDeleteCancellationToken;

const MAX_RESOLUTION_DIAGNOSTIC_BYTES: usize = 4 * 1024;
const DEADLINE_EXCEEDED_CODE: &str = "REPLICA_REPAIR_DEADLINE_EXCEEDED";
const SERVICE_CANCELLED_CODE: &str = "REPLICA_REPAIR_SERVICE_CANCELLED";

#[async_trait]
pub trait ReplicaRepairResolverV2: Send + Sync {
    async fn resolve(
        &self,
        request: &restoreplane_catalog::ClaimedReplicaRepairV2,
    ) -> Result<ResolvedReplicaRepairV2, ReplicaRepairResolutionErrorV2>;
}

pub struct ResolvedReplicaRepairV2 {
    signed: SignedSetManifestV2,
    source: RepairSourceV2,
    target: ReplicaEndpointV2,
    config: restoreplane_pipeline::PipelineCfg,
    verifying_key: VerifyingKey,
}

impl ResolvedReplicaRepairV2 {
    pub fn new(
        signed: SignedSetManifestV2,
        source: RepairSourceV2,
        target: ReplicaEndpointV2,
        config: restoreplane_pipeline::PipelineCfg,
        verifying_key: VerifyingKey,
    ) -> Result<Self, ReplicaRepairWorkerErrorV2> {
        if config.max_parallel_destinations == 0 || signed.verify(&verifying_key).is_err() {
            return Err(ReplicaRepairWorkerErrorV2::InputInvalid);
        }
        Ok(Self {
            signed,
            source,
            target,
            config,
            verifying_key,
        })
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("{message}")]
pub struct ReplicaRepairResolutionErrorV2 {
    message: String,
    retryable: bool,
}

impl ReplicaRepairResolutionErrorV2 {
    pub fn retryable(message: impl Into<String>) -> Result<Self, ReplicaRepairWorkerErrorV2> {
        Self::new(message, true)
    }

    pub fn terminal(message: impl Into<String>) -> Result<Self, ReplicaRepairWorkerErrorV2> {
        Self::new(message, false)
    }

    fn new(
        message: impl Into<String>,
        retryable: bool,
    ) -> Result<Self, ReplicaRepairWorkerErrorV2> {
        let message = message.into();
        if !valid_diagnostic(&message) {
            return Err(ReplicaRepairWorkerErrorV2::InputInvalid);
        }
        Ok(Self { message, retryable })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplicaRepairWorkerOutcomeV2 {
    Terminal(ClaimTerminalOutcome),
    Retry(RetryDisposition),
}

#[derive(Debug, Error)]
pub enum ReplicaRepairWorkerErrorV2 {
    #[error("REPLICA_REPAIR_WORKER_INPUT_INVALID")]
    InputInvalid,
    #[error(transparent)]
    Repository(#[from] RepositoryError),
}

impl ReplicaRepairWorkerErrorV2 {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InputInvalid => "REPLICA_REPAIR_WORKER_INPUT_INVALID",
            Self::Repository(error) => error.code(),
        }
    }
}

pub struct ReplicaRepairWorkerV2 {
    repository: RuntimeRepository,
    resolver: Arc<dyn ReplicaRepairResolverV2>,
    clock: Arc<dyn RuntimeClock>,
    lease_duration_ms: i64,
    retry_delay_ms: i64,
}

impl ReplicaRepairWorkerV2 {
    pub fn new(
        repository: RuntimeRepository,
        resolver: Arc<dyn ReplicaRepairResolverV2>,
        clock: Arc<dyn RuntimeClock>,
        lease_duration_ms: i64,
        retry_delay_ms: i64,
    ) -> Result<Self, ReplicaRepairWorkerErrorV2> {
        if lease_duration_ms <= 0 || retry_delay_ms <= 0 {
            return Err(ReplicaRepairWorkerErrorV2::InputInvalid);
        }
        Ok(Self {
            repository,
            resolver,
            clock,
            lease_duration_ms,
            retry_delay_ms,
        })
    }

    pub async fn execute(
        &self,
        claim: &ClaimedWork,
    ) -> Result<ReplicaRepairWorkerOutcomeV2, ReplicaRepairWorkerErrorV2> {
        self.execute_inner(claim, None).await
    }

    pub async fn execute_with_control(
        &self,
        claim: &ClaimedWork,
        cancellation: &RetentionDeleteCancellationToken,
        deadline: Instant,
    ) -> Result<ReplicaRepairWorkerOutcomeV2, ReplicaRepairWorkerErrorV2> {
        self.execute_inner(claim, Some((cancellation, deadline)))
            .await
    }

    async fn execute_inner(
        &self,
        claim: &ClaimedWork,
        control: Option<(&RetentionDeleteCancellationToken, Instant)>,
    ) -> Result<ReplicaRepairWorkerOutcomeV2, ReplicaRepairWorkerErrorV2> {
        let started_at_ms = self.clock.now_ms();
        if started_at_ms < 0 {
            return Err(ReplicaRepairWorkerErrorV2::InputInvalid);
        }
        let execution = self
            .repository
            .load_claimed_replica_repair_v2(claim, started_at_ms)?;
        self.repository.start_claim(claim, started_at_ms)?;
        let resolved = match await_with_control(self.resolver.resolve(&execution), control).await {
            Err(RepairInterruption::Cancelled) => {
                return self.settle_failure(claim, SERVICE_CANCELLED_CODE, true, 0, started_at_ms);
            }
            Err(RepairInterruption::DeadlineExceeded) => {
                return self.settle_failure(claim, DEADLINE_EXCEEDED_CODE, true, 0, started_at_ms);
            }
            Ok(Ok(resolved)) => resolved,
            Ok(Err(error)) => {
                return self.settle_failure(
                    claim,
                    "REPLICA_REPAIR_RESOLUTION_FAILED",
                    error.retryable,
                    0,
                    started_at_ms,
                );
            }
        };

        let ResolvedReplicaRepairV2 {
            signed,
            source,
            target,
            config,
            verifying_key,
        } = resolved;
        let heartbeat_at_ms = self.clock.now_ms();
        if heartbeat_at_ms < started_at_ms {
            return Err(ReplicaRepairWorkerErrorV2::InputInvalid);
        }
        let lease_expires_at_ms = heartbeat_at_ms
            .checked_add(self.lease_duration_ms)
            .ok_or(ReplicaRepairWorkerErrorV2::InputInvalid)?;
        self.repository
            .heartbeat_claim(claim, heartbeat_at_ms, lease_expires_at_ms)?;
        let repair = match await_with_control(
            repair_replica_v2(&signed, source, target, &config, &verifying_key),
            control,
        )
        .await
        {
            Ok(repair) => repair,
            Err(RepairInterruption::Cancelled) => {
                return self.settle_failure(claim, SERVICE_CANCELLED_CODE, true, 0, started_at_ms);
            }
            Err(RepairInterruption::DeadlineExceeded) => {
                return self.settle_failure(claim, DEADLINE_EXCEEDED_CODE, true, 0, started_at_ms);
            }
        };
        match repair {
            Err(error) => self.settle_failure(
                claim,
                error.code(),
                replica_publish_error_retryable(&error),
                0,
                started_at_ms,
            ),
            Ok(outcome) if outcome.result.is_err() => {
                let error = outcome
                    .result
                    .as_ref()
                    .err()
                    .ok_or(ReplicaRepairWorkerErrorV2::InputInvalid)?;
                self.settle_failure(
                    claim,
                    error.code(),
                    replica_publish_error_retryable(error),
                    outcome.attempts,
                    started_at_ms,
                )
            }
            Ok(outcome) => {
                let completed_at_ms = self.completed_at_ms(started_at_ms)?;
                let terminal = self.repository.complete_replica_repair_v2(
                    claim,
                    signed,
                    verifying_key,
                    outcome,
                    completed_at_ms,
                )?;
                Ok(ReplicaRepairWorkerOutcomeV2::Terminal(terminal))
            }
        }
    }

    fn settle_failure(
        &self,
        claim: &ClaimedWork,
        error_code: &str,
        retryable: bool,
        attempts: u32,
        lower_bound_ms: i64,
    ) -> Result<ReplicaRepairWorkerOutcomeV2, ReplicaRepairWorkerErrorV2> {
        let completed_at_ms = self.completed_at_ms(lower_bound_ms)?;
        let result_json = serde_json::to_string(&serde_json::json!({
            "attempts": attempts,
            "error_code": error_code,
            "schema_version": 1,
        }))
        .map_err(|_| ReplicaRepairWorkerErrorV2::InputInvalid)?;
        if retryable {
            let retry_available_at_ms = completed_at_ms
                .checked_add(self.retry_delay_ms)
                .ok_or(ReplicaRepairWorkerErrorV2::InputInvalid)?;
            let disposition = self.repository.retry_claim(
                claim,
                completed_at_ms,
                retry_available_at_ms,
                error_code,
                Some(result_json),
            )?;
            Ok(ReplicaRepairWorkerOutcomeV2::Retry(disposition))
        } else {
            let terminal = self.repository.complete_claim(ClaimCompletionSpec::failed(
                claim.clone(),
                completed_at_ms,
                error_code,
                Some(result_json),
            ))?;
            Ok(ReplicaRepairWorkerOutcomeV2::Terminal(terminal))
        }
    }

    fn completed_at_ms(&self, lower_bound_ms: i64) -> Result<i64, ReplicaRepairWorkerErrorV2> {
        let now_ms = self.clock.now_ms();
        if now_ms < 0 {
            return Err(ReplicaRepairWorkerErrorV2::InputInvalid);
        }
        Ok(now_ms.max(lower_bound_ms))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RepairInterruption {
    Cancelled,
    DeadlineExceeded,
}

async fn await_with_control<F, T>(
    future: F,
    control: Option<(&RetentionDeleteCancellationToken, Instant)>,
) -> Result<T, RepairInterruption>
where
    F: Future<Output = T>,
{
    let Some((cancellation, deadline)) = control else {
        return Ok(future.await);
    };
    tokio::pin!(future);
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(RepairInterruption::Cancelled),
        _ = tokio::time::sleep_until(deadline) => Err(RepairInterruption::DeadlineExceeded),
        output = &mut future => Ok(output),
    }
}

fn replica_publish_error_retryable(error: &ReplicaPublishError) -> bool {
    match error {
        ReplicaPublishError::Upload { source, .. } => source.is_retryable(),
        ReplicaPublishError::Reader { source, .. } => source.is_retryable(),
        ReplicaPublishError::ManifestInvalid { .. }
        | ReplicaPublishError::SourceSetInvalid { .. }
        | ReplicaPublishError::EndpointSetInvalid { .. }
        | ReplicaPublishError::ConfigInvalid { .. }
        | ReplicaPublishError::ReaderChecksumMismatch
        | ReplicaPublishError::ManifestRoundTripInvalid => false,
    }
}

fn valid_diagnostic(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= MAX_RESOLUTION_DIAGNOSTIC_BYTES
        && !value.chars().any(|character| character.is_control())
}
