use std::sync::{mpsc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use restoreplane_catalog::{
    OperationalStore, RepositoryError, SchedulerCursorAdvanceSpec, SchedulerRepository,
};
use restoreplane_scheduler::occurrence::{
    evaluate_due_window, CatchUpPolicy, MissedReason, WakeMode, MAX_EVALUATION_WINDOW_MS,
};
use restoreplane_scheduler::parse_cron_subset;
use restoreplane_scheduler::service_wake::ServiceWake;
use thiserror::Error;

use crate::host::{ClaimRuntime, HostError, ServiceHealthProbe};

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(30);
const REGULAR_WAKE_MAX_LAG_MS: i64 = 5 * 60 * 1_000;
const MAX_SERVICE_CATCHUP_WINDOW_MS: i64 = 31 * 24 * 60 * 60 * 1_000;

#[derive(Clone)]
pub struct ServiceScheduler {
    repository: SchedulerRepository,
    runtime: restoreplane_catalog::RuntimeRepository,
}

impl ServiceScheduler {
    pub fn new(store: OperationalStore) -> Self {
        Self {
            repository: store.scheduler(),
            runtime: store.runtime(),
        }
    }

    pub fn run_once(&self, now_ms: i64) -> Result<SchedulerTickReport, ServiceSchedulerError> {
        if now_ms < 0 {
            return Err(ServiceSchedulerError::ClockInvalid);
        }
        let plans = self.repository.scheduled_plans()?;
        let mut report = SchedulerTickReport::default();
        for plan in plans {
            report.plans_evaluated += 1;
            let cursor_ms = plan
                .last_evaluated_at_ms()
                .unwrap_or(plan.published_at_ms());
            if now_ms <= cursor_ms {
                continue;
            }
            let bounded_window = MAX_SERVICE_CATCHUP_WINDOW_MS.min(MAX_EVALUATION_WINDOW_MS);
            let evaluation_end_ms = cursor_ms.saturating_add(bounded_window).min(now_ms);
            if cursor_ms >= evaluation_end_ms {
                continue;
            }

            let schedule = match parse_cron_subset(plan.schedule_expression()) {
                Ok(schedule) => schedule,
                Err(_) => {
                    report.plan_errors += 1;
                    let _ = self
                        .repository
                        .advance_cursor(SchedulerCursorAdvanceSpec::failure(
                            plan.plan_revision_id().clone(),
                            now_ms,
                            "SCHEDULE_CRON_INVALID",
                        ));
                    continue;
                }
            };
            let catchup_policy = match plan.catchup_policy() {
                "skip" => CatchUpPolicy::Skip,
                "bounded_latest_one" => CatchUpPolicy::BoundedLatestOne,
                _ => {
                    report.plan_errors += 1;
                    let _ = self
                        .repository
                        .advance_cursor(SchedulerCursorAdvanceSpec::failure(
                            plan.plan_revision_id().clone(),
                            now_ms,
                            "SCHEDULE_CATCHUP_POLICY_INVALID",
                        ));
                    continue;
                }
            };
            let wake_mode = if plan.last_evaluated_at_ms().is_some()
                && evaluation_end_ms.saturating_sub(cursor_ms) <= REGULAR_WAKE_MAX_LAG_MS
            {
                WakeMode::Regular
            } else {
                WakeMode::Recovery
            };
            let historical_chunk_precedes_latest = evaluation_end_ms < now_ms;
            let evaluation_mode = if historical_chunk_precedes_latest
                && catchup_policy == CatchUpPolicy::BoundedLatestOne
            {
                WakeMode::Regular
            } else {
                wake_mode
            };
            let mut evaluation = match evaluate_due_window(
                &schedule,
                plan.timezone(),
                cursor_ms,
                evaluation_end_ms,
                evaluation_mode,
                catchup_policy,
            ) {
                Ok(evaluation) => evaluation,
                Err(error) => {
                    report.plan_errors += 1;
                    let _ = self
                        .repository
                        .advance_cursor(SchedulerCursorAdvanceSpec::failure(
                            plan.plan_revision_id().clone(),
                            now_ms,
                            error.code(),
                        ));
                    continue;
                }
            };
            if historical_chunk_precedes_latest && catchup_policy == CatchUpPolicy::BoundedLatestOne
            {
                evaluation.mark_all_due_missed(MissedReason::SupersededByLatestCatchUp);
            }

            if plan.overlap_policy() == "coalesce_one"
                && plan.has_active_backup_work()
                && !evaluation.due().is_empty()
            {
                report.plans_coalesced += 1;
                continue;
            }

            let materialized_for_ms = evaluation
                .due()
                .iter()
                .map(|occurrence| occurrence.scheduled_for_ms())
                .max();
            let wake = ServiceWake::new(self.runtime.clone(), plan.plan_revision_id().clone());
            let materialized = match wake.materialize(&evaluation, now_ms) {
                Ok(materialized) => materialized,
                Err(_) => {
                    report.plan_errors += 1;
                    continue;
                }
            };
            report.created_occurrences += materialized.created_occurrences();
            report.created_backup_runs += materialized.created_backup_runs();
            report.created_work_items += materialized.created_work_items();
            report.missed_occurrences += materialized.missed_occurrences();

            let advanced = self
                .repository
                .advance_cursor(SchedulerCursorAdvanceSpec::success(
                    plan.plan_revision_id().clone(),
                    evaluation_end_ms,
                    materialized_for_ms,
                ))?;
            if !advanced.advanced() {
                report.cursor_conflicts += 1;
            }
        }
        Ok(report)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SchedulerTickReport {
    plans_evaluated: usize,
    plans_coalesced: usize,
    created_occurrences: usize,
    created_backup_runs: usize,
    created_work_items: usize,
    missed_occurrences: usize,
    plan_errors: usize,
    cursor_conflicts: usize,
}

impl SchedulerTickReport {
    pub const fn plans_evaluated(self) -> usize {
        self.plans_evaluated
    }

    pub const fn plans_coalesced(self) -> usize {
        self.plans_coalesced
    }

    pub const fn created_occurrences(self) -> usize {
        self.created_occurrences
    }

    pub const fn created_backup_runs(self) -> usize {
        self.created_backup_runs
    }

    pub const fn created_work_items(self) -> usize {
        self.created_work_items
    }

    pub const fn missed_occurrences(self) -> usize {
        self.missed_occurrences
    }

    pub const fn plan_errors(self) -> usize {
        self.plan_errors
    }

    pub const fn cursor_conflicts(self) -> usize {
        self.cursor_conflicts
    }
}

#[derive(Debug, Error)]
pub enum ServiceSchedulerError {
    #[error("SERVICE_SCHEDULER_CLOCK_INVALID")]
    ClockInvalid,
    #[error("SERVICE_SCHEDULER_REPOSITORY")]
    Repository(#[from] RepositoryError),
}

#[derive(Debug, Error)]
pub enum ServiceSchedulerStartError {
    #[error("SERVICE_SCHEDULER_INPUT_INVALID")]
    InputInvalid,
    #[error("SERVICE_SCHEDULER_THREAD_START")]
    ThreadStart(#[source] std::io::Error),
}

pub struct ServiceSchedulerHandle {
    stop: mpsc::SyncSender<()>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl ServiceSchedulerHandle {
    pub fn spawn(
        store: OperationalStore,
        health: ServiceHealthProbe,
    ) -> Result<Self, ServiceSchedulerStartError> {
        Self::spawn_with_interval(store, health, DEFAULT_POLL_INTERVAL)
    }

    fn spawn_with_interval(
        store: OperationalStore,
        health: ServiceHealthProbe,
        poll_interval: Duration,
    ) -> Result<Self, ServiceSchedulerStartError> {
        if poll_interval.is_zero() {
            return Err(ServiceSchedulerStartError::InputInvalid);
        }
        let scheduler = ServiceScheduler::new(store);
        let (stop, stop_receiver) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("restoreplane-program-scheduler".to_owned())
            .spawn(move || loop {
                if health.snapshot().claims_enabled() {
                    match scheduler.run_once(unix_time_ms()) {
                        Ok(report) => {
                            if report.plan_errors() > 0 || report.cursor_conflicts() > 0 {
                                tracing::warn!(
                                    plan_errors = report.plan_errors(),
                                    cursor_conflicts = report.cursor_conflicts(),
                                    "program-owned scheduler tick completed with bounded errors"
                                );
                            }
                        }
                        Err(error) => {
                            tracing::error!(
                                error = %error,
                                "program-owned scheduler tick failed before plan evaluation"
                            );
                        }
                    }
                }
                match stop_receiver.recv_timeout(poll_interval) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
            })
            .map_err(ServiceSchedulerStartError::ThreadStart)?;
        Ok(Self {
            stop,
            thread: Mutex::new(Some(thread)),
        })
    }

    pub fn request_stop(&self) {
        let _ = self.stop.try_send(());
    }

    pub fn wait_stopped(&self) -> Result<(), HostError> {
        let thread = self
            .thread
            .lock()
            .map_err(|_| HostError::InternalState)?
            .take();
        if let Some(thread) = thread {
            thread.join().map_err(|_| HostError::RuntimeFailed)?;
        }
        Ok(())
    }
}

impl ClaimRuntime for ServiceSchedulerHandle {
    fn request_stop(&self) {
        Self::request_stop(self);
    }

    fn wait_stopped(&self) -> Result<(), HostError> {
        Self::wait_stopped(self)
    }
}

impl Drop for ServiceSchedulerHandle {
    fn drop(&mut self) {
        self.request_stop();
        let _ = self.wait_stopped();
    }
}

fn unix_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use restoreplane_catalog::rusqlite::OptionalExtension;
    use restoreplane_catalog::{
        ConnectionProfileSpec, DatabaseTargetSpec, DestinationSpec, PlanDraftSpec,
        StandaloneAuthorityBootstrapSpec,
    };
    use restoreplane_domain::{
        BackupPlanId, ConnectionProfileId, DatabaseTargetId, DestinationId, DestinationRequirement,
        PlanRevisionId,
    };
    use tempfile::TempDir;
    use uuid::Uuid;

    use crate::host::{HostConfig, HostRuntime, ServiceHost, ShutdownReason};

    use super::*;

    const TEST_POLL_INTERVAL: Duration = Duration::from_millis(20);
    const WAIT_TIMEOUT: Duration = Duration::from_secs(3);

    #[test]
    fn scheduler_handle_runs_an_immediate_tick_when_claims_are_enabled() {
        let fixture = SchedulerHandleFixture::new(true);
        let handle = ServiceSchedulerHandle::spawn_with_interval(
            fixture.store.clone(),
            fixture.host.health_probe(),
            TEST_POLL_INTERVAL,
        )
        .expect("spawn scheduler handle");

        assert!(wait_until(|| fixture.cursor_updated_at_ms().is_some()));

        handle.request_stop();
        handle.wait_stopped().expect("join scheduler handle");
        fixture.shutdown();
    }

    #[test]
    fn scheduler_handle_repeats_ticks_at_the_configured_interval() {
        let fixture = SchedulerHandleFixture::new(true);
        let handle = ServiceSchedulerHandle::spawn_with_interval(
            fixture.store.clone(),
            fixture.host.health_probe(),
            TEST_POLL_INTERVAL,
        )
        .expect("spawn scheduler handle");
        assert!(wait_until(|| fixture.cursor_updated_at_ms().is_some()));
        let first_updated_at_ms = fixture
            .cursor_updated_at_ms()
            .expect("initial scheduler cursor heartbeat");

        assert!(wait_until(|| fixture.cursor_updated_at_ms().is_some_and(
            |updated_at_ms| updated_at_ms > first_updated_at_ms
        )));

        handle.request_stop();
        handle.wait_stopped().expect("join scheduler handle");
        fixture.shutdown();
    }

    #[test]
    fn scheduler_handle_does_not_tick_while_claims_are_disabled() {
        let fixture = SchedulerHandleFixture::new(false);
        let handle = ServiceSchedulerHandle::spawn_with_interval(
            fixture.store.clone(),
            fixture.host.health_probe(),
            TEST_POLL_INTERVAL,
        )
        .expect("spawn scheduler handle");

        std::thread::sleep(TEST_POLL_INTERVAL.saturating_mul(5));
        assert_eq!(fixture.cursor_updated_at_ms(), None);

        handle.request_stop();
        handle.wait_stopped().expect("join scheduler handle");
        fixture.shutdown();
    }

    #[test]
    fn scheduler_handle_stop_and_join_prevent_further_catalog_changes() {
        let fixture = SchedulerHandleFixture::new(true);
        let handle = ServiceSchedulerHandle::spawn_with_interval(
            fixture.store.clone(),
            fixture.host.health_probe(),
            TEST_POLL_INTERVAL,
        )
        .expect("spawn scheduler handle");
        assert!(wait_until(|| fixture.cursor_updated_at_ms().is_some()));

        handle.request_stop();
        handle.wait_stopped().expect("join scheduler handle");
        let stopped_updated_at_ms = fixture
            .cursor_updated_at_ms()
            .expect("scheduler cursor before stop");
        std::thread::sleep(TEST_POLL_INTERVAL.saturating_mul(5));

        assert_eq!(fixture.cursor_updated_at_ms(), Some(stopped_updated_at_ms));
        fixture.shutdown();
    }

    #[test]
    fn scheduler_handle_rejects_a_zero_poll_interval() {
        let fixture = SchedulerHandleFixture::new(true);
        let result = ServiceSchedulerHandle::spawn_with_interval(
            fixture.store.clone(),
            fixture.host.health_probe(),
            Duration::ZERO,
        );

        assert!(matches!(
            result,
            Err(ServiceSchedulerStartError::InputInvalid)
        ));
        fixture.shutdown();
    }

    struct SchedulerHandleFixture {
        host: ServiceHost<NoopHostRuntime>,
        store: OperationalStore,
        database_path: std::path::PathBuf,
        revision_id: PlanRevisionId,
        _temp: TempDir,
    }

    impl SchedulerHandleFixture {
        fn new(claims_enabled: bool) -> Self {
            let temp = TempDir::new().expect("temp directory");
            let database_path = temp.path().join("operational.db");
            let mut host_config = HostConfig::new(
                format!("scheduler-handle-{}", Uuid::new_v4()),
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
                        unix_time_ms().saturating_sub(1_000),
                    ),
                );
            }
            let host = ServiceHost::start(host_config, Arc::new(NoopHostRuntime))
                .expect("start service host");
            assert_eq!(host.health().claims_enabled(), claims_enabled);
            let store = host.operational_store().expect("operational store");
            let revision_id = seed_scheduled_plan(&store);
            Self {
                host,
                store,
                database_path,
                revision_id,
                _temp: temp,
            }
        }

        fn cursor_updated_at_ms(&self) -> Option<i64> {
            let connection = restoreplane_catalog::rusqlite::Connection::open(&self.database_path)
                .expect("open scheduler lifecycle evidence");
            connection
                .busy_timeout(Duration::from_secs(5))
                .expect("scheduler lifecycle busy timeout");
            connection
                .query_row(
                    "SELECT updated_at_ms FROM scheduler_cursors WHERE plan_revision_id = ?1",
                    [self.revision_id.as_str()],
                    |row| row.get(0),
                )
                .optional()
                .expect("query scheduler cursor")
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

    fn seed_scheduled_plan(store: &OperationalStore) -> PlanRevisionId {
        let now_ms = unix_time_ms();
        let published_at_ms = now_ms.saturating_sub(3 * 60 * 60 * 1_000);
        let connection_id = ConnectionProfileId::new();
        let target_id = DatabaseTargetId::new();
        let destination_id = DestinationId::new();
        let plan_id = BackupPlanId::new();
        let revision_id = PlanRevisionId::new();
        let configuration = store.configuration();
        configuration
            .insert_connection(ConnectionProfileSpec::tcp(
                connection_id.clone(),
                "scheduler-postgresql",
                "postgresql",
                "db.internal",
                5432,
                published_at_ms.saturating_sub(500),
            ))
            .expect("insert scheduler connection");
        configuration
            .insert_target(DatabaseTargetSpec::new(
                target_id.clone(),
                connection_id,
                "oid:1",
                "orders",
                "orders",
                "orders-fingerprint",
                published_at_ms.saturating_sub(400),
            ))
            .expect("insert scheduler target");
        configuration
            .insert_destination(DestinationSpec::local(
                destination_id.clone(),
                "scheduler-vault",
                r#"{"path":"D:\\backup"}"#,
                published_at_ms.saturating_sub(300),
            ))
            .expect("insert scheduler destination");
        configuration
            .insert_plan_draft(PlanDraftSpec::cron(
                plan_id,
                revision_id.clone(),
                "hourly",
                1,
                "0 * * * *",
                "UTC",
                "e".repeat(64),
                published_at_ms.saturating_sub(200),
            ))
            .expect("insert scheduler plan");
        configuration
            .add_revision_target(
                &revision_id,
                &target_id,
                published_at_ms.saturating_sub(100),
            )
            .expect("add scheduler target");
        configuration
            .add_revision_destination(
                &revision_id,
                &destination_id,
                DestinationRequirement::Required,
                published_at_ms.saturating_sub(50),
            )
            .expect("add scheduler destination");
        configuration
            .publish_revision(&revision_id, published_at_ms)
            .expect("publish scheduler plan");
        revision_id
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
}
