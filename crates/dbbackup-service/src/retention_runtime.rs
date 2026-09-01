use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use dbbackup_catalog::{
    ClaimRequest, OperationalStore, RepositoryError, RetentionMaintenanceReport,
    RetentionMaintenanceSpec,
};
use dbbackup_domain::WorkItemId;
use dbbackup_runtime::{
    RetentionDeleteCancellationToken, RetentionDeleteOperatorResolverV2,
    RetentionDeleteWorkerErrorV2, RetentionDeleteWorkerV2, RuntimeClock, SystemRuntimeClock,
};
use thiserror::Error;

use crate::host::{ClaimRuntime, HostError, ServiceHealthProbe};
use crate::retention::ServiceRetentionDeleteResolverV2;
use crate::secrets::ServiceSecretStore;

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(250);
const DEFAULT_MAINTENANCE_INTERVAL_MS: i64 = 15 * 60 * 1_000;
const DEFAULT_GRACE_PERIOD_MS: i64 = 24 * 60 * 60 * 1_000;
const DEFAULT_SCAN_LIMIT: usize = 1_024;
const LEASE_DURATION_MS: i64 = 30_000;
const RETRY_DELAY_MS: i64 = 5_000;
const REMOTE_DELETE_DEADLINE: Duration = Duration::from_secs(20);
const STOP_WAIT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RetentionRuntimeTick {
    Idle,
    Settled(WorkItemId),
}

pub struct ServiceRetentionRuntime {
    store: OperationalStore,
    worker: RetentionDeleteWorkerV2,
    clock: Arc<dyn RuntimeClock>,
    worker_id: String,
    worker_boot_id: String,
    grace_period_ms: i64,
    scan_limit: usize,
}

impl ServiceRetentionRuntime {
    pub fn new(
        store: OperationalStore,
        resolver: Arc<dyn RetentionDeleteOperatorResolverV2>,
        clock: Arc<dyn RuntimeClock>,
        worker_id: impl Into<String>,
        worker_boot_id: impl Into<String>,
        grace_period_ms: i64,
        scan_limit: usize,
    ) -> Result<Self, RetentionRuntimeError> {
        if grace_period_ms <= 0 || scan_limit == 0 {
            return Err(RetentionRuntimeError::InputInvalid);
        }
        let worker = RetentionDeleteWorkerV2::new(
            store.runtime(),
            resolver,
            Arc::clone(&clock),
            LEASE_DURATION_MS,
            RETRY_DELAY_MS,
        )?
        .with_remote_delete_deadline(REMOTE_DELETE_DEADLINE)?;
        Ok(Self {
            store,
            worker,
            clock,
            worker_id: worker_id.into(),
            worker_boot_id: worker_boot_id.into(),
            grace_period_ms,
            scan_limit,
        })
    }

    pub fn maintenance_once(
        &self,
        now_ms: i64,
    ) -> Result<RetentionMaintenanceReport, RetentionRuntimeError> {
        self.store
            .retention_runtime()
            .reconcile(RetentionMaintenanceSpec::new(
                now_ms,
                self.grace_period_ms,
                self.scan_limit,
            ))
            .map_err(RetentionRuntimeError::from)
    }

    pub async fn delete_once(&self) -> Result<RetentionRuntimeTick, RetentionRuntimeError> {
        self.delete_once_with_cancellation(&RetentionDeleteCancellationToken::default())
            .await
    }

    pub async fn delete_once_with_cancellation(
        &self,
        cancellation: &RetentionDeleteCancellationToken,
    ) -> Result<RetentionRuntimeTick, RetentionRuntimeError> {
        let now_ms = self.clock.now_ms();
        if now_ms < 0 {
            return Err(RetentionRuntimeError::ClockInvalid);
        }
        let Some(claim) = self.store.runtime().claim_next(
            ClaimRequest::new(
                &self.worker_id,
                &self.worker_boot_id,
                now_ms,
                LEASE_DURATION_MS,
            )
            .for_operation_kinds(["retention_delete"]),
        )?
        else {
            return Ok(RetentionRuntimeTick::Idle);
        };
        let work_item_id = claim.work_item_id().clone();
        self.worker
            .execute_with_cancellation(&claim, cancellation)
            .await?;
        Ok(RetentionRuntimeTick::Settled(work_item_id))
    }
}

#[derive(Debug, Error)]
pub enum RetentionRuntimeError {
    #[error("RETENTION_RUNTIME_INPUT_INVALID")]
    InputInvalid,
    #[error("RETENTION_RUNTIME_CLOCK_INVALID")]
    ClockInvalid,
    #[error(transparent)]
    Repository(#[from] RepositoryError),
    #[error(transparent)]
    Worker(#[from] RetentionDeleteWorkerErrorV2),
}

impl RetentionRuntimeError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InputInvalid => "RETENTION_RUNTIME_INPUT_INVALID",
            Self::ClockInvalid => "RETENTION_RUNTIME_CLOCK_INVALID",
            Self::Repository(error) => error.code(),
            Self::Worker(error) => error.code(),
        }
    }
}

#[derive(Debug, Error)]
pub enum RetentionRuntimeStartError {
    #[error("RETENTION_RUNTIME_WORKER_INIT")]
    Worker(#[from] RetentionDeleteWorkerErrorV2),
    #[error("RETENTION_RUNTIME_THREAD_START")]
    ThreadStart(#[source] std::io::Error),
    #[error("RETENTION_RUNTIME_ASYNC_INIT")]
    AsyncRuntime(#[source] std::io::Error),
    #[error("RETENTION_RUNTIME_INPUT_INVALID")]
    InputInvalid,
}

pub struct ServiceRetentionRuntimeHandle {
    stop: mpsc::SyncSender<()>,
    cancellation: RetentionDeleteCancellationToken,
    stopped: Mutex<Option<mpsc::Receiver<()>>>,
    stop_wait_timeout: Duration,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl ServiceRetentionRuntimeHandle {
    pub fn spawn(
        store: OperationalStore,
        secrets: Arc<ServiceSecretStore>,
        health: ServiceHealthProbe,
    ) -> Result<Self, RetentionRuntimeStartError> {
        Self::spawn_with_interval(
            store,
            secrets,
            health,
            DEFAULT_POLL_INTERVAL,
            DEFAULT_MAINTENANCE_INTERVAL_MS,
        )
    }

    fn spawn_with_interval(
        store: OperationalStore,
        secrets: Arc<ServiceSecretStore>,
        health: ServiceHealthProbe,
        poll_interval: Duration,
        maintenance_interval_ms: i64,
    ) -> Result<Self, RetentionRuntimeStartError> {
        if poll_interval.is_zero() || maintenance_interval_ms <= 0 {
            return Err(RetentionRuntimeStartError::InputInvalid);
        }
        let resolver: Arc<dyn RetentionDeleteOperatorResolverV2> = Arc::new(
            ServiceRetentionDeleteResolverV2::new(store.configuration(), secrets),
        );
        let clock: Arc<dyn RuntimeClock> = Arc::new(SystemRuntimeClock);
        let service = ServiceRetentionRuntime::new(
            store,
            resolver,
            Arc::clone(&clock),
            format!("retention-worker-{}", uuid::Uuid::new_v4()),
            uuid::Uuid::new_v4().to_string(),
            DEFAULT_GRACE_PERIOD_MS,
            DEFAULT_SCAN_LIMIT,
        )
        .map_err(|error| match error {
            RetentionRuntimeError::Worker(error) => RetentionRuntimeStartError::Worker(error),
            _ => RetentionRuntimeStartError::InputInvalid,
        })?;
        let async_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(RetentionRuntimeStartError::AsyncRuntime)?;
        let (stop, stop_receiver) = mpsc::sync_channel(1);
        let (stopped_sender, stopped_receiver) = mpsc::sync_channel(1);
        let cancellation = RetentionDeleteCancellationToken::default();
        let thread_cancellation = cancellation.clone();
        let thread = thread::Builder::new()
            .name("dbbackup-retention-runtime".to_owned())
            .spawn(move || {
                let mut next_maintenance_at_ms = 0_i64;
                loop {
                    if thread_cancellation.is_cancelled() {
                        break;
                    }
                    if health.snapshot().claims_enabled() {
                        let now_ms = clock.now_ms();
                        if now_ms >= next_maintenance_at_ms {
                            match service.maintenance_once(now_ms) {
                                Ok(report) => {
                                    if report.failures() > 0 {
                                        tracing::warn!(
                                            failures = report.failures(),
                                            candidates = report.candidates_evaluated(),
                                            "automatic retention maintenance failed closed for bounded candidates"
                                        );
                                    }
                                }
                                Err(error) => tracing::error!(
                                    error_code = error.code(),
                                    "automatic retention maintenance tick failed"
                                ),
                            }
                            next_maintenance_at_ms =
                                now_ms.saturating_add(maintenance_interval_ms);
                        }
                        if thread_cancellation.is_cancelled() {
                            break;
                        }
                        if let Err(error) = async_runtime.block_on(
                            service.delete_once_with_cancellation(&thread_cancellation),
                        ) {
                            tracing::error!(
                                error_code = error.code(),
                                "retention delete worker tick failed closed"
                            );
                        }
                    }
                    match stop_receiver.recv_timeout(poll_interval) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                }
                let _ = stopped_sender.send(());
            })
            .map_err(RetentionRuntimeStartError::ThreadStart)?;
        Ok(Self {
            stop,
            cancellation,
            stopped: Mutex::new(Some(stopped_receiver)),
            stop_wait_timeout: STOP_WAIT_TIMEOUT,
            thread: Mutex::new(Some(thread)),
        })
    }

    pub fn request_stop(&self) {
        self.cancellation.cancel();
        let _ = self.stop.try_send(());
    }

    pub fn wait_stopped(&self) -> Result<(), HostError> {
        let wait_started = Instant::now();
        let stopped = self
            .stopped
            .lock()
            .map_err(|_| HostError::InternalState)?
            .take();
        if let Some(stopped) = stopped {
            match stopped.recv_timeout(self.stop_wait_timeout) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.thread
                        .lock()
                        .map_err(|_| HostError::InternalState)?
                        .take();
                    return Err(HostError::RuntimeFailed);
                }
            }
        }
        let thread = self
            .thread
            .lock()
            .map_err(|_| HostError::InternalState)?
            .take();
        if let Some(thread) = thread {
            while !thread.is_finished() {
                let remaining = self
                    .stop_wait_timeout
                    .saturating_sub(wait_started.elapsed());
                if remaining.is_zero() {
                    return Err(HostError::RuntimeFailed);
                }
                thread::sleep(remaining.min(Duration::from_millis(5)));
            }
            thread.join().map_err(|_| HostError::RuntimeFailed)?;
        }
        Ok(())
    }
}

impl ClaimRuntime for ServiceRetentionRuntimeHandle {
    fn request_stop(&self) {
        Self::request_stop(self);
    }

    fn wait_stopped(&self) -> Result<(), HostError> {
        Self::wait_stopped(self)
    }
}

impl Drop for ServiceRetentionRuntimeHandle {
    fn drop(&mut self) {
        self.request_stop();
        let _ = self.wait_stopped();
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use dbbackup_catalog::rusqlite::OptionalExtension;
    use dbbackup_catalog::StandaloneAuthorityBootstrapSpec;
    use tempfile::TempDir;
    use uuid::Uuid;

    use crate::host::{HostConfig, HostRuntime, ServiceHost, ShutdownReason};
    use crate::ipc::{current_process_identity, WindowsSid};
    use crate::secrets::AclPolicy;

    use super::*;

    const TEST_POLL_INTERVAL: Duration = Duration::from_millis(20);
    const TEST_MAINTENANCE_INTERVAL_MS: i64 = 1;
    const WAIT_TIMEOUT: Duration = Duration::from_secs(3);

    #[test]
    fn retention_handle_runs_an_immediate_maintenance_tick_when_claims_are_enabled() {
        let fixture = RetentionHandleFixture::new(true);
        let handle = fixture.spawn_handle();

        assert!(wait_until(|| fixture.last_scan_at_ms().is_some()));

        handle.request_stop();
        handle.wait_stopped().expect("join retention handle");
        fixture.shutdown();
    }

    #[test]
    fn retention_handle_repeats_maintenance_at_the_configured_interval() {
        let fixture = RetentionHandleFixture::new(true);
        let handle = fixture.spawn_handle();
        assert!(wait_until(|| fixture.last_scan_at_ms().is_some()));
        let first_scan_at_ms = fixture
            .last_scan_at_ms()
            .expect("initial retention maintenance heartbeat");

        assert!(wait_until(|| fixture
            .last_scan_at_ms()
            .is_some_and(|scan_at_ms| scan_at_ms > first_scan_at_ms)));

        handle.request_stop();
        handle.wait_stopped().expect("join retention handle");
        fixture.shutdown();
    }

    #[test]
    fn retention_handle_does_not_run_maintenance_while_claims_are_disabled() {
        let fixture = RetentionHandleFixture::new(false);
        let handle = fixture.spawn_handle();

        std::thread::sleep(TEST_POLL_INTERVAL.saturating_mul(5));
        assert_eq!(fixture.last_scan_at_ms(), None);

        handle.request_stop();
        handle.wait_stopped().expect("join retention handle");
        fixture.shutdown();
    }

    #[test]
    fn retention_handle_stop_and_join_prevent_further_catalog_changes() {
        let fixture = RetentionHandleFixture::new(true);
        let handle = fixture.spawn_handle();
        assert!(wait_until(|| fixture.last_scan_at_ms().is_some()));

        handle.request_stop();
        handle.wait_stopped().expect("join retention handle");
        let stopped_scan_at_ms = fixture
            .last_scan_at_ms()
            .expect("retention heartbeat before stop");
        std::thread::sleep(TEST_POLL_INTERVAL.saturating_mul(5));

        assert_eq!(fixture.last_scan_at_ms(), Some(stopped_scan_at_ms));
        fixture.shutdown();
    }

    #[test]
    fn retention_handle_stop_wakes_a_pending_remote_delete_and_joins_within_bound() {
        let cancellation = RetentionDeleteCancellationToken::default();
        let thread_cancellation = cancellation.clone();
        let (stop, stop_receiver) = mpsc::sync_channel(1);
        let (stopped_sender, stopped_receiver) = mpsc::sync_channel(1);
        let thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test async runtime");
            runtime.block_on(async move {
                tokio::select! {
                    _ = std::future::pending::<()>() => unreachable!(),
                    _ = thread_cancellation.cancelled() => {}
                }
            });
            drop(stop_receiver);
            let _ = stopped_sender.send(());
        });
        let handle = ServiceRetentionRuntimeHandle {
            stop,
            cancellation,
            stopped: Mutex::new(Some(stopped_receiver)),
            stop_wait_timeout: Duration::from_millis(500),
            thread: Mutex::new(Some(thread)),
        };

        let started = Instant::now();
        handle.request_stop();
        handle
            .wait_stopped()
            .expect("join cancelled retention handle");

        assert!(
            started.elapsed() < Duration::from_millis(500),
            "service stop must not wait for a blackhole destination"
        );
    }

    #[test]
    fn retention_handle_wait_has_an_explicit_bound_for_an_uncooperative_thread() {
        let cancellation = RetentionDeleteCancellationToken::default();
        let (stop, stop_receiver) = mpsc::sync_channel(1);
        let (stopped_sender, stopped_receiver) = mpsc::sync_channel(1);
        let thread = thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(stop_receiver);
            let _ = stopped_sender.send(());
        });
        let handle = ServiceRetentionRuntimeHandle {
            stop,
            cancellation,
            stopped: Mutex::new(Some(stopped_receiver)),
            stop_wait_timeout: Duration::from_millis(20),
            thread: Mutex::new(Some(thread)),
        };

        let started = Instant::now();
        handle.request_stop();
        let error = handle
            .wait_stopped()
            .expect_err("uncooperative retention thread must time out");

        assert_eq!(error.code(), "SERVICE_RUNTIME_FAILED");
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn retention_handle_wait_does_not_join_past_the_bound_after_a_premature_stop_signal() {
        let cancellation = RetentionDeleteCancellationToken::default();
        let (stop, stop_receiver) = mpsc::sync_channel(1);
        let (stopped_sender, stopped_receiver) = mpsc::sync_channel(1);
        let thread = thread::spawn(move || {
            let _ = stopped_sender.send(());
            std::thread::sleep(Duration::from_millis(200));
            drop(stop_receiver);
        });
        let handle = ServiceRetentionRuntimeHandle {
            stop,
            cancellation,
            stopped: Mutex::new(Some(stopped_receiver)),
            stop_wait_timeout: Duration::from_millis(20),
            thread: Mutex::new(Some(thread)),
        };

        let started = Instant::now();
        handle.request_stop();
        let error = handle
            .wait_stopped()
            .expect_err("join must share the same stop budget");

        assert_eq!(error.code(), "SERVICE_RUNTIME_FAILED");
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    struct RetentionHandleFixture {
        host: ServiceHost<NoopHostRuntime>,
        store: OperationalStore,
        secrets: Arc<ServiceSecretStore>,
        database_path: std::path::PathBuf,
        _temp: TempDir,
    }

    impl RetentionHandleFixture {
        fn new(claims_enabled: bool) -> Self {
            let temp = TempDir::new().expect("temp directory");
            let database_path = temp.path().join("operational.db");
            let mut host_config = HostConfig::new(
                format!("retention-handle-{}", Uuid::new_v4()),
                database_path.clone(),
            )
            .expect("host config");
            if claims_enabled {
                host_config = host_config.with_pristine_standalone_bootstrap(
                    StandaloneAuthorityBootstrapSpec::new(
                        Uuid::new_v4(),
                        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                        "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
                        "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
                        system_time_ms().saturating_sub(1_000),
                    ),
                );
            }
            let host = ServiceHost::start(host_config, Arc::new(NoopHostRuntime))
                .expect("start service host");
            assert_eq!(host.health().claims_enabled(), claims_enabled);
            let store = host.operational_store().expect("operational store");
            let owner_sid = current_process_identity()
                .expect("current process identity")
                .user_sid()
                .clone();
            let acl = AclPolicy::new(
                WindowsSid::parse("S-1-5-80-1001").expect("test virtual service SID"),
                owner_sid,
            )
            .expect("secret ACL policy");
            let secrets = Arc::new(
                ServiceSecretStore::open(temp.path().join("secrets"), store.secret_refs(), acl)
                    .expect("open secret store"),
            );
            Self {
                host,
                store,
                secrets,
                database_path,
                _temp: temp,
            }
        }

        fn spawn_handle(&self) -> ServiceRetentionRuntimeHandle {
            ServiceRetentionRuntimeHandle::spawn_with_interval(
                self.store.clone(),
                Arc::clone(&self.secrets),
                self.host.health_probe(),
                TEST_POLL_INTERVAL,
                TEST_MAINTENANCE_INTERVAL_MS,
            )
            .expect("spawn retention handle")
        }

        fn last_scan_at_ms(&self) -> Option<i64> {
            let connection = dbbackup_catalog::rusqlite::Connection::open(&self.database_path)
                .expect("open retention lifecycle evidence");
            connection
                .busy_timeout(Duration::from_secs(5))
                .expect("retention lifecycle busy timeout");
            connection
                .query_row(
                    "SELECT updated_at_ms FROM app_meta WHERE key = 'retention.runtime.last_scan.v1'",
                    [],
                    |row| row.get(0),
                )
                .optional()
                .expect("query retention maintenance heartbeat")
        }

        fn shutdown(&self) {
            self.host
                .shutdown(ShutdownReason::Stop)
                .expect("shutdown service host");
        }
    }

    struct NoopHostRuntime;

    impl HostRuntime for NoopHostRuntime {
        fn started(&self, _diagnostics_only: bool) -> Result<(), HostError> {
            Ok(())
        }

        fn stop_accepting_ipc(&self) -> Result<(), HostError> {
            Ok(())
        }

        fn stop_claiming_work(&self) -> Result<(), HostError> {
            Ok(())
        }

        fn drain_clients(&self) -> Result<(), HostError> {
            Ok(())
        }

        fn checkpointed(&self) -> Result<(), HostError> {
            Ok(())
        }

        fn stopped(&self, _reason: ShutdownReason) {}
    }

    fn wait_until(mut predicate: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + WAIT_TIMEOUT;
        while Instant::now() < deadline {
            if predicate() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        predicate()
    }

    fn system_time_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or(0)
    }
}
