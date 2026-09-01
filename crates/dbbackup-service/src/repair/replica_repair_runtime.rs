use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use dbbackup_catalog::{ClaimRequest, OperationalStore, RepositoryError};
use dbbackup_domain::WorkItemId;
use dbbackup_pipeline::PipelineCfg;
use dbbackup_runtime::{
    ReplicaRepairResolverV2, ReplicaRepairWorkerErrorV2, ReplicaRepairWorkerV2,
    RetentionDeleteCancellationToken, RuntimeClock, SystemRuntimeClock,
};
use ed25519_dalek::VerifyingKey;
use thiserror::Error;

use crate::host::{ClaimRuntime, HostError, ServiceHealthProbe};
use crate::secrets::ServiceSecretStore;

use super::queue_runtime::{ClaimsEnabledProbe, QueueRuntimeHandle, QueueRuntimeStartError};
use super::ServiceReplicaRepairResolverV2;

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(250);
pub(super) const DEFAULT_REMOTE_REPAIR_DEADLINE: Duration = Duration::from_secs(20);
const STOP_WAIT_TIMEOUT: Duration = Duration::from_secs(10);
const LEASE_DURATION_MS: i64 = 30_000;
const RETRY_DELAY_MS: i64 = 5_000;

#[derive(Clone, Debug, PartialEq, Eq)]
enum ReplicaRepairRuntimeTick {
    Idle,
    Settled(WorkItemId),
}

struct ServiceReplicaRepairRuntime {
    store: OperationalStore,
    worker: ReplicaRepairWorkerV2,
    clock: Arc<dyn RuntimeClock>,
    worker_id: String,
    worker_boot_id: String,
    remote_repair_deadline: Duration,
}

impl ServiceReplicaRepairRuntime {
    fn new(
        store: OperationalStore,
        resolver: Arc<dyn ReplicaRepairResolverV2>,
        clock: Arc<dyn RuntimeClock>,
        worker_id: impl Into<String>,
        worker_boot_id: impl Into<String>,
        remote_repair_deadline: Duration,
    ) -> Result<Self, ReplicaRepairRuntimeError> {
        let worker_id = worker_id.into();
        let worker_boot_id = worker_boot_id.into();
        if worker_id.is_empty()
            || worker_id.len() > 128
            || worker_boot_id.is_empty()
            || worker_boot_id.len() > 128
            || remote_repair_deadline.is_zero()
            || remote_repair_deadline >= Duration::from_millis(LEASE_DURATION_MS as u64)
        {
            return Err(ReplicaRepairRuntimeError::InputInvalid);
        }
        let worker = ReplicaRepairWorkerV2::new(
            store.runtime(),
            resolver,
            Arc::clone(&clock),
            LEASE_DURATION_MS,
            RETRY_DELAY_MS,
        )?;
        Ok(Self {
            store,
            worker,
            clock,
            worker_id,
            worker_boot_id,
            remote_repair_deadline,
        })
    }

    async fn run_once_with_cancellation(
        &self,
        cancellation: &RetentionDeleteCancellationToken,
    ) -> Result<ReplicaRepairRuntimeTick, ReplicaRepairRuntimeError> {
        if cancellation.is_cancelled() {
            return Ok(ReplicaRepairRuntimeTick::Idle);
        }
        let now_ms = self.clock.now_ms();
        if now_ms < 0 {
            return Err(ReplicaRepairRuntimeError::ClockInvalid);
        }
        let Some(claim) = self.store.runtime().claim_next(
            ClaimRequest::new(
                &self.worker_id,
                &self.worker_boot_id,
                now_ms,
                LEASE_DURATION_MS,
            )
            .for_operation_kinds(["replica_repair"]),
        )?
        else {
            return Ok(ReplicaRepairRuntimeTick::Idle);
        };
        let work_item_id = claim.work_item_id().clone();
        self.worker
            .execute_with_control(
                &claim,
                cancellation,
                tokio::time::Instant::now() + self.remote_repair_deadline,
            )
            .await?;
        Ok(ReplicaRepairRuntimeTick::Settled(work_item_id))
    }
}

#[derive(Debug, Error)]
enum ReplicaRepairRuntimeError {
    #[error("REPLICA_REPAIR_RUNTIME_INPUT_INVALID")]
    InputInvalid,
    #[error("REPLICA_REPAIR_RUNTIME_CLOCK_INVALID")]
    ClockInvalid,
    #[error(transparent)]
    Repository(#[from] RepositoryError),
    #[error(transparent)]
    Worker(#[from] ReplicaRepairWorkerErrorV2),
}

impl ReplicaRepairRuntimeError {
    fn code(&self) -> &'static str {
        match self {
            Self::InputInvalid => "REPLICA_REPAIR_RUNTIME_INPUT_INVALID",
            Self::ClockInvalid => "REPLICA_REPAIR_RUNTIME_CLOCK_INVALID",
            Self::Repository(error) => error.code(),
            Self::Worker(error) => error.code(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ReplicaRepairRuntimeStartError {
    #[error("REPLICA_REPAIR_RUNTIME_INPUT_INVALID")]
    InputInvalid,
    #[error("REPLICA_REPAIR_RUNTIME_ASYNC_INIT")]
    AsyncRuntime(#[source] std::io::Error),
    #[error("REPLICA_REPAIR_RUNTIME_THREAD_START")]
    ThreadStart(#[source] std::io::Error),
}

impl ReplicaRepairRuntimeStartError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InputInvalid => "REPLICA_REPAIR_RUNTIME_INPUT_INVALID",
            Self::AsyncRuntime(_) => "REPLICA_REPAIR_RUNTIME_ASYNC_INIT",
            Self::ThreadStart(_) => "REPLICA_REPAIR_RUNTIME_THREAD_START",
        }
    }
}

pub struct ServiceReplicaRepairRuntimeHandle {
    inner: QueueRuntimeHandle,
}

impl ServiceReplicaRepairRuntimeHandle {
    pub fn spawn(
        store: OperationalStore,
        secrets: Arc<ServiceSecretStore>,
        health: ServiceHealthProbe,
        trusted_signing_keys: BTreeMap<String, VerifyingKey>,
        pipeline_config: PipelineCfg,
    ) -> Result<Self, ReplicaRepairRuntimeStartError> {
        let resolver: Arc<dyn ReplicaRepairResolverV2> = Arc::new(
            ServiceReplicaRepairResolverV2::new(
                store.configuration(),
                secrets,
                trusted_signing_keys,
                pipeline_config,
            )
            .map_err(|_| ReplicaRepairRuntimeStartError::InputInvalid)?,
        );
        let clock: Arc<dyn RuntimeClock> = Arc::new(SystemRuntimeClock);
        let runtime = ServiceReplicaRepairRuntime::new(
            store,
            resolver,
            clock,
            format!("replica-repair-worker-{}", uuid::Uuid::new_v4()),
            uuid::Uuid::new_v4().to_string(),
            DEFAULT_REMOTE_REPAIR_DEADLINE,
        )
        .map_err(|_| ReplicaRepairRuntimeStartError::InputInvalid)?;
        let async_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(ReplicaRepairRuntimeStartError::AsyncRuntime)?;
        let cancellation = RetentionDeleteCancellationToken::default();
        let thread_cancellation = cancellation.clone();
        let cancel_in_flight = cancellation.clone();
        let claims_enabled: ClaimsEnabledProbe =
            Arc::new(move || health.snapshot().claims_enabled());
        let inner = QueueRuntimeHandle::spawn(
            "dbbackup-replica-repair-runtime",
            DEFAULT_POLL_INTERVAL,
            STOP_WAIT_TIMEOUT,
            claims_enabled,
            Arc::new(move || cancel_in_flight.cancel()),
            move || {
                if let Err(error) =
                    async_runtime.block_on(runtime.run_once_with_cancellation(&thread_cancellation))
                {
                    tracing::error!(
                        error_code = error.code(),
                        "replica repair worker tick failed closed"
                    );
                }
            },
        )
        .map_err(map_queue_start)?;
        Ok(Self { inner })
    }

    pub fn request_stop(&self) {
        self.inner.request_stop();
    }

    pub fn wait_stopped(&self) -> Result<(), HostError> {
        self.inner.wait_stopped()
    }
}

impl ClaimRuntime for ServiceReplicaRepairRuntimeHandle {
    fn request_stop(&self) {
        Self::request_stop(self);
    }

    fn wait_stopped(&self) -> Result<(), HostError> {
        Self::wait_stopped(self)
    }
}

fn map_queue_start(error: QueueRuntimeStartError) -> ReplicaRepairRuntimeStartError {
    match error {
        QueueRuntimeStartError::InputInvalid => ReplicaRepairRuntimeStartError::InputInvalid,
        QueueRuntimeStartError::ThreadStart(error) => {
            ReplicaRepairRuntimeStartError::ThreadStart(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc};
    use std::time::Duration;

    use async_trait::async_trait;
    use dbbackup_catalog::{ClaimRequest, OperationWorkSpec, OperationalStore};
    use dbbackup_domain::{OperationRunId, WorkItemId};
    use dbbackup_runtime::{
        ReplicaRepairResolutionErrorV2, ReplicaRepairResolverV2, ResolvedReplicaRepairV2,
        RetentionDeleteCancellationToken, RuntimeClock,
    };
    use tempfile::TempDir;
    use uuid::Uuid;

    use super::{
        ServiceReplicaRepairRuntime, ServiceReplicaRepairRuntimeHandle,
        DEFAULT_REMOTE_REPAIR_DEADLINE,
    };
    use crate::repair::queue_runtime::QueueRuntimeHandle;

    #[tokio::test]
    async fn repair_runtime_only_claims_replica_repair_operations() {
        let temp = TempDir::new().expect("temp directory");
        let store =
            OperationalStore::open(temp.path().join("operational.db")).expect("operational store");
        let namespace = Uuid::new_v4();
        store
            .runtime()
            .enqueue_operation_work(OperationWorkSpec::new(
                OperationRunId::from_stable_name(namespace, b"support-bundle-run"),
                WorkItemId::from_stable_name(namespace, b"support-bundle-work"),
                "support_bundle",
                "workspace",
                Uuid::new_v4().to_string(),
                r#"{"schemaVersion":1}"#,
                1_000,
            ))
            .expect("enqueue unrelated operation");
        let runtime = ServiceReplicaRepairRuntime::new(
            store.clone(),
            Arc::new(NeverResolver),
            Arc::new(IncreasingClock::new(1_000)),
            "repair-worker",
            "repair-boot",
            DEFAULT_REMOTE_REPAIR_DEADLINE,
        )
        .expect("repair runtime");

        assert_eq!(
            runtime
                .run_once_with_cancellation(&RetentionDeleteCancellationToken::default())
                .await
                .expect("repair tick"),
            super::ReplicaRepairRuntimeTick::Idle
        );
        assert!(store
            .runtime()
            .claim_next(ClaimRequest::new(
                "other-worker",
                "other-boot",
                1_001,
                1_000
            ))
            .expect("claim unrelated operation")
            .is_some());
    }

    #[test]
    fn repair_handle_cancels_in_flight_and_stop_join_prevents_new_ticks() {
        let cancellation = RetentionDeleteCancellationToken::default();
        let cancel_hook = cancellation.clone();
        let thread_cancellation = cancellation.clone();
        let ticks = Arc::new(AtomicUsize::new(0));
        let tick_count = Arc::clone(&ticks);
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let async_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("async runtime");
        let inner = QueueRuntimeHandle::spawn(
            "repair-runtime-contract",
            Duration::from_millis(5),
            Duration::from_secs(1),
            Arc::new(|| true),
            Arc::new(move || cancel_hook.cancel()),
            move || {
                tick_count.fetch_add(1, Ordering::AcqRel);
                let _ = entered_tx.try_send(());
                async_runtime.block_on(thread_cancellation.cancelled());
            },
        )
        .expect("spawn repair runtime");
        let handle = ServiceReplicaRepairRuntimeHandle { inner };
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("repair tick entered");

        handle.request_stop();
        handle.wait_stopped().expect("join repair runtime");
        assert_eq!(ticks.load(Ordering::Acquire), 1);
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(ticks.load(Ordering::Acquire), 1);
    }

    #[test]
    fn repair_handle_does_not_tick_when_claims_are_disabled() {
        let ticks = Arc::new(AtomicUsize::new(0));
        let tick_count = Arc::clone(&ticks);
        let inner = QueueRuntimeHandle::spawn(
            "repair-runtime-disabled-contract",
            Duration::from_millis(5),
            Duration::from_secs(1),
            Arc::new(|| false),
            Arc::new(|| {}),
            move || {
                tick_count.fetch_add(1, Ordering::AcqRel);
            },
        )
        .expect("spawn repair runtime");
        let handle = ServiceReplicaRepairRuntimeHandle { inner };

        std::thread::sleep(Duration::from_millis(40));
        handle.request_stop();
        handle.wait_stopped().expect("join repair runtime");
        assert_eq!(ticks.load(Ordering::Acquire), 0);
    }

    struct NeverResolver;

    #[async_trait]
    impl ReplicaRepairResolverV2 for NeverResolver {
        async fn resolve(
            &self,
            _request: &dbbackup_catalog::ClaimedReplicaRepairV2,
        ) -> Result<ResolvedReplicaRepairV2, ReplicaRepairResolutionErrorV2> {
            panic!("unrelated operation must not reach repair resolver")
        }
    }

    struct IncreasingClock {
        now_ms: AtomicI64,
    }

    impl IncreasingClock {
        const fn new(now_ms: i64) -> Self {
            Self {
                now_ms: AtomicI64::new(now_ms),
            }
        }
    }

    impl RuntimeClock for IncreasingClock {
        fn now_ms(&self) -> i64 {
            self.now_ms.fetch_add(1, Ordering::AcqRel)
        }
    }
}
