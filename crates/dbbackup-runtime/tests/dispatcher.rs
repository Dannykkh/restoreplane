use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dbbackup_catalog::{
    CancelRequestOutcome, OperationWorkSpec, OperationalStore, ResourcePoolSpec,
    ResourceRequirement, RuntimeRepository,
};
use dbbackup_domain::{OperationRunId, WorkItemId};
use dbbackup_runtime::{
    hash_executable, DispatchConfig, DispatchError, DispatchOutcome, DispatchPlan, DispatchPlanner,
    Dispatcher, LaunchSpec, ManagedProcess, ManagedProcessExit, ManagedProcessIdentity,
    ProcessLauncher, RuntimeClock,
};
use tempfile::TempDir;

#[test]
fn one_dispatch_claims_admits_heartbeats_runs_and_settles_durably() {
    let temp = TempDir::new().expect("temp directory");
    let db_path = temp.path().join("operational.db");
    let store = OperationalStore::open(db_path.clone()).expect("open operational store");
    let repository = store.runtime();
    let pool_id = uuid::Uuid::new_v4().to_string();
    repository
        .register_resource_pool(ResourcePoolSpec::new(
            pool_id.clone(),
            "global_job",
            "service",
            1,
            900,
        ))
        .expect("register global pool");
    let work_item_id = WorkItemId::new();
    repository
        .enqueue_operation_work(OperationWorkSpec::new(
            OperationRunId::new(),
            work_item_id.clone(),
            "tool_rescan",
            "service",
            "dispatcher-test",
            "{}",
            1_000,
        ))
        .expect("enqueue operation");

    let executable = std::env::current_exe().expect("test executable");
    let launch = LaunchSpec::new(
        executable.clone(),
        temp.path().to_path_buf(),
        hash_executable(&executable).expect("hash executable"),
    )
    .expect("launch spec");
    let planner = Arc::new(FixedPlanner {
        plan: DispatchPlan::new(
            launch,
            "native_tool",
            vec![ResourceRequirement::new(pool_id, 1)],
            None,
        )
        .expect("dispatch plan")
        .with_wait_policy(Duration::from_millis(10), Duration::from_secs(1))
        .expect("wait policy"),
    });
    let launches = Arc::new(AtomicUsize::new(0));
    let launcher = Arc::new(FakeLauncher {
        launches: Arc::clone(&launches),
    });
    let clock = Arc::new(StepClock::new(1_000, 10));
    let dispatcher = Dispatcher::new(
        repository,
        planner,
        launcher,
        clock,
        DispatchConfig::new("worker-a", "boot-a", 500, 100).expect("dispatch config"),
    );

    let outcome = dispatcher.dispatch_once().expect("dispatch one work item");
    assert_eq!(
        outcome,
        DispatchOutcome::Completed {
            work_item_id: work_item_id.to_string(),
            attempt_no: 1,
            exit_code: 0,
        }
    );
    assert_eq!(launches.load(Ordering::SeqCst), 1);
    assert_eq!(
        dispatcher.dispatch_once().expect("empty second tick"),
        DispatchOutcome::Idle
    );

    let direct =
        dbbackup_catalog::rusqlite::Connection::open(db_path).expect("open dispatch probe");
    let state: DurableDispatchState = direct
        .query_row(
            "SELECT
                 work_items.state,
                 work_items.lease_owner,
                 work_attempts.state,
                 work_attempts.last_heartbeat_at_ms,
                 operation_runs.state,
                 resource_allocations.release_reason,
                 process_instances.state,
                 process_instances.exit_code
             FROM work_items
             JOIN work_attempts
               ON work_attempts.work_item_id = work_items.id
             JOIN operation_runs
               ON operation_runs.id = work_items.operation_run_id
             JOIN resource_allocations
               ON resource_allocations.work_attempt_id = work_attempts.id
             JOIN process_instances
               ON process_instances.work_attempt_id = work_attempts.id
             WHERE work_items.id = ?1",
            [work_item_id.as_str()],
            |row| {
                Ok(DurableDispatchState {
                    work_state: row.get(0)?,
                    lease_owner: row.get(1)?,
                    attempt_state: row.get(2)?,
                    heartbeat_at_ms: row.get(3)?,
                    operation_state: row.get(4)?,
                    release_reason: row.get(5)?,
                    process_state: row.get(6)?,
                    exit_code: row.get(7)?,
                })
            },
        )
        .expect("query durable dispatch state");
    assert_eq!(state.work_state, "succeeded");
    assert_eq!(state.lease_owner, None);
    assert_eq!(state.attempt_state, "succeeded");
    assert!(
        state.heartbeat_at_ms > 1_000,
        "one timeout slice must renew the lease"
    );
    assert_eq!(state.operation_state, "succeeded");
    assert_eq!(state.release_reason.as_deref(), Some("completed"));
    assert_eq!(state.process_state, "exited");
    assert_eq!(state.exit_code, Some(0));
}

#[test]
fn admission_wait_is_durably_deferred_without_launching_a_process() {
    let temp = TempDir::new().expect("temp directory");
    let db_path = temp.path().join("operational.db");
    let store = OperationalStore::open(db_path.clone()).expect("open operational store");
    let repository = store.runtime();
    let work_item_id = WorkItemId::new();
    repository
        .enqueue_operation_work(OperationWorkSpec::new(
            OperationRunId::new(),
            work_item_id.clone(),
            "tool_rescan",
            "service",
            "deferred-dispatch",
            "{}",
            2_000,
        ))
        .expect("enqueue deferred operation");
    let missing_pool_id = uuid::Uuid::new_v4().to_string();
    let executable = std::env::current_exe().expect("test executable");
    let launch = LaunchSpec::new(
        executable.clone(),
        temp.path().to_path_buf(),
        hash_executable(&executable).expect("hash executable"),
    )
    .expect("launch spec");
    let planner = Arc::new(FixedPlanner {
        plan: DispatchPlan::new(
            launch,
            "native_tool",
            vec![ResourceRequirement::new(missing_pool_id.clone(), 1)],
            None,
        )
        .expect("dispatch plan"),
    });
    let launches = Arc::new(AtomicUsize::new(0));
    let dispatcher = Dispatcher::new(
        repository,
        planner,
        Arc::new(FakeLauncher {
            launches: Arc::clone(&launches),
        }),
        Arc::new(StepClock::new(2_000, 10)),
        DispatchConfig::new("worker-wait", "boot-wait", 500, 100).expect("dispatch config"),
    );

    let reason = format!("resource_missing:{missing_pool_id}");
    assert_eq!(
        dispatcher.dispatch_once().expect("defer dispatch"),
        DispatchOutcome::Deferred {
            work_item_id: work_item_id.to_string(),
            reason: reason.clone(),
        }
    );
    assert_eq!(launches.load(Ordering::SeqCst), 0);

    let direct =
        dbbackup_catalog::rusqlite::Connection::open(db_path).expect("open deferral probe");
    let state: (String, Option<String>, String, Option<String>) = direct
        .query_row(
            "SELECT
                 work_items.state,
                 work_items.lease_owner,
                 work_attempts.state,
                 work_items.wait_reason
             FROM work_items
             JOIN work_attempts
               ON work_attempts.work_item_id = work_items.id
             WHERE work_items.id = ?1",
            [work_item_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("query durable deferral");
    assert_eq!(
        state,
        (
            "delayed".to_owned(),
            None,
            "abandoned".to_owned(),
            Some(reason)
        )
    );
}

#[test]
fn retryable_exit_requeues_until_attempt_budget_then_fails_terminally() {
    let temp = TempDir::new().expect("temp directory");
    let db_path = temp.path().join("operational.db");
    let store = OperationalStore::open(db_path.clone()).expect("open operational store");
    let repository = store.runtime();
    let pool_id = uuid::Uuid::new_v4().to_string();
    repository
        .register_resource_pool(ResourcePoolSpec::new(
            pool_id.clone(),
            "global_job",
            "service",
            1,
            2_900,
        ))
        .expect("register retry pool");
    let work_item_id = WorkItemId::new();
    repository
        .enqueue_operation_work(OperationWorkSpec::new(
            OperationRunId::new(),
            work_item_id.clone(),
            "tool_rescan",
            "service",
            "retry-budget",
            "{}",
            3_000,
        ))
        .expect("enqueue retry operation");

    let executable = std::env::current_exe().expect("test executable");
    let launch = LaunchSpec::new(
        executable.clone(),
        temp.path().to_path_buf(),
        hash_executable(&executable).expect("hash executable"),
    )
    .expect("launch spec");
    let planner = Arc::new(FixedPlanner {
        plan: DispatchPlan::new(
            launch,
            "native_tool",
            vec![ResourceRequirement::new(pool_id, 1)],
            None,
        )
        .expect("dispatch plan")
        .with_retry_delay(Duration::from_millis(1))
        .expect("retry policy"),
    });
    let launches = Arc::new(AtomicUsize::new(0));
    let dispatcher = Dispatcher::new(
        repository,
        planner,
        Arc::new(ImmediateExitLauncher {
            launches: Arc::clone(&launches),
            exit_code: 7,
        }),
        Arc::new(StepClock::new(3_000, 10)),
        DispatchConfig::new("worker-retry", "boot-retry", 500, 100).expect("dispatch config"),
    );

    for attempt_no in 1..=2 {
        assert_eq!(
            dispatcher.dispatch_once().expect("retry dispatch"),
            DispatchOutcome::RetryScheduled {
                work_item_id: work_item_id.to_string(),
                attempt_no,
                error_code: "PROCESS_EXIT_NONZERO".to_owned(),
            }
        );
    }
    assert_eq!(
        dispatcher.dispatch_once().expect("exhaust retry budget"),
        DispatchOutcome::Failed {
            work_item_id: work_item_id.to_string(),
            attempt_no: 3,
            error_code: "RETRY_BUDGET_EXHAUSTED".to_owned(),
        }
    );
    assert_eq!(launches.load(Ordering::SeqCst), 3);
    assert_eq!(
        dispatcher
            .dispatch_once()
            .expect("terminal work is not claimable"),
        DispatchOutcome::Idle
    );

    let direct = dbbackup_catalog::rusqlite::Connection::open(db_path).expect("open retry probe");
    let aggregate: (String, String, Option<String>, i64, i64) = direct
        .query_row(
            "SELECT
                 work_items.state,
                 operation_runs.state,
                 operation_runs.error_code,
                 COUNT(work_attempts.id),
                 SUM(CASE WHEN resource_allocations.released_at_ms IS NOT NULL THEN 1 ELSE 0 END)
             FROM work_items
             JOIN operation_runs
               ON operation_runs.id = work_items.operation_run_id
             JOIN work_attempts
               ON work_attempts.work_item_id = work_items.id
             JOIN resource_allocations
               ON resource_allocations.work_attempt_id = work_attempts.id
             WHERE work_items.id = ?1
             GROUP BY work_items.id, operation_runs.id",
            [work_item_id.as_str()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .expect("query retry settlement");
    assert_eq!(
        aggregate,
        (
            "failed_terminal".to_owned(),
            "failed".to_owned(),
            Some("RETRY_BUDGET_EXHAUSTED".to_owned()),
            3,
            3,
        )
    );
}

#[test]
fn durable_cancel_wins_a_simultaneous_successful_process_exit_exactly_once() {
    let temp = TempDir::new().expect("temp directory");
    let db_path = temp.path().join("operational.db");
    let store = OperationalStore::open(db_path.clone()).expect("open operational store");
    let repository = store.runtime();
    let pool_id = uuid::Uuid::new_v4().to_string();
    repository
        .register_resource_pool(ResourcePoolSpec::new(
            pool_id.clone(),
            "global_job",
            "service",
            1,
            3_900,
        ))
        .expect("register cancel pool");
    let work_item_id = WorkItemId::new();
    repository
        .enqueue_operation_work(OperationWorkSpec::new(
            OperationRunId::new(),
            work_item_id.clone(),
            "tool_rescan",
            "service",
            "cancel-exit-race",
            "{}",
            4_000,
        ))
        .expect("enqueue cancel operation");
    let executable = std::env::current_exe().expect("test executable");
    let launch = LaunchSpec::new(
        executable.clone(),
        temp.path().to_path_buf(),
        hash_executable(&executable).expect("hash executable"),
    )
    .expect("launch spec");
    let planner = Arc::new(FixedPlanner {
        plan: DispatchPlan::new(
            launch,
            "native_tool",
            vec![ResourceRequirement::new(pool_id, 1)],
            None,
        )
        .expect("dispatch plan"),
    });
    let dispatcher = Dispatcher::new(
        repository.clone(),
        planner,
        Arc::new(CancelOnExitLauncher {
            repository: repository.clone(),
            work_item_id: work_item_id.clone(),
        }),
        Arc::new(StepClock::new(4_000, 10)),
        DispatchConfig::new("worker-cancel", "boot-cancel", 500, 100).expect("dispatch config"),
    );

    assert_eq!(
        dispatcher.dispatch_once().expect("cancelled dispatch"),
        DispatchOutcome::Cancelled {
            work_item_id: work_item_id.to_string(),
            attempt_no: 1,
            error_code: "CANCEL_REQUESTED".to_owned(),
        }
    );
    assert_eq!(
        repository
            .request_cancel(&work_item_id, 1, 4_100)
            .expect("idempotent terminal cancel"),
        CancelRequestOutcome::AlreadyTerminal
    );

    let direct = dbbackup_catalog::rusqlite::Connection::open(db_path).expect("open cancel probe");
    let settled: (String, String, String, Option<String>, String, i64, i64) = direct
        .query_row(
            "SELECT
                 work_items.state,
                 work_attempts.state,
                 operation_runs.state,
                 operation_runs.error_code,
                 resource_allocations.release_reason,
                 COUNT(resource_allocations.id),
                 SUM(CASE WHEN resource_allocations.released_at_ms IS NOT NULL THEN 1 ELSE 0 END)
             FROM work_items
             JOIN work_attempts
               ON work_attempts.work_item_id = work_items.id
             JOIN operation_runs
               ON operation_runs.id = work_items.operation_run_id
             JOIN resource_allocations
               ON resource_allocations.work_attempt_id = work_attempts.id
             WHERE work_items.id = ?1
             GROUP BY work_items.id, work_attempts.id, operation_runs.id",
            [work_item_id.as_str()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .expect("query cancel settlement");
    assert_eq!(
        settled,
        (
            "cancelled".to_owned(),
            "cancelled".to_owned(),
            "cancelled".to_owned(),
            Some("CANCEL_REQUESTED".to_owned()),
            "cancelled".to_owned(),
            1,
            1,
        )
    );
}

struct FixedPlanner {
    plan: DispatchPlan,
}

struct DurableDispatchState {
    work_state: String,
    lease_owner: Option<String>,
    attempt_state: String,
    heartbeat_at_ms: i64,
    operation_state: String,
    release_reason: Option<String>,
    process_state: String,
    exit_code: Option<i64>,
}

impl DispatchPlanner for FixedPlanner {
    fn plan(&self, _claim: &dbbackup_catalog::ClaimedWork) -> Result<DispatchPlan, DispatchError> {
        Ok(self.plan.clone())
    }
}

struct StepClock {
    next: AtomicI64,
    step: i64,
}

impl StepClock {
    const fn new(start: i64, step: i64) -> Self {
        Self {
            next: AtomicI64::new(start),
            step,
        }
    }
}

impl RuntimeClock for StepClock {
    fn now_ms(&self) -> i64 {
        self.next.fetch_add(self.step, Ordering::SeqCst)
    }
}

struct FakeLauncher {
    launches: Arc<AtomicUsize>,
}

impl ProcessLauncher for FakeLauncher {
    fn launch(&self, _spec: &LaunchSpec) -> Result<Box<dyn ManagedProcess>, DispatchError> {
        self.launches.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(FakeProcess {
            waits: 0,
            identity: ManagedProcessIdentity::new(42, 1_000, "job/fake").expect("fake identity"),
        }))
    }
}

struct FakeProcess {
    waits: usize,
    identity: ManagedProcessIdentity,
}

impl ManagedProcess for FakeProcess {
    fn identity(&self) -> &ManagedProcessIdentity {
        &self.identity
    }

    fn wait_timeout(
        &mut self,
        _timeout: Duration,
    ) -> Result<Option<ManagedProcessExit>, DispatchError> {
        self.waits += 1;
        if self.waits == 1 {
            Ok(None)
        } else {
            Ok(Some(
                ManagedProcessExit::new(0, 128, false, 64, false).expect("successful fake exit"),
            ))
        }
    }

    fn terminate(&mut self) -> Result<ManagedProcessExit, DispatchError> {
        ManagedProcessExit::new(1, 0, false, 0, false)
    }
}

struct ImmediateExitLauncher {
    launches: Arc<AtomicUsize>,
    exit_code: i64,
}

impl ProcessLauncher for ImmediateExitLauncher {
    fn launch(&self, _spec: &LaunchSpec) -> Result<Box<dyn ManagedProcess>, DispatchError> {
        let process_id =
            i64::try_from(self.launches.fetch_add(1, Ordering::SeqCst) + 100).expect("process id");
        Ok(Box::new(ImmediateExitProcess {
            identity: ManagedProcessIdentity::new(process_id, 3_000, "job/retry")
                .expect("retry identity"),
            exit_code: self.exit_code,
        }))
    }
}

struct ImmediateExitProcess {
    identity: ManagedProcessIdentity,
    exit_code: i64,
}

impl ManagedProcess for ImmediateExitProcess {
    fn identity(&self) -> &ManagedProcessIdentity {
        &self.identity
    }

    fn wait_timeout(
        &mut self,
        _timeout: Duration,
    ) -> Result<Option<ManagedProcessExit>, DispatchError> {
        Ok(Some(
            ManagedProcessExit::new(self.exit_code, 0, false, 32, false).expect("retryable exit"),
        ))
    }

    fn terminate(&mut self) -> Result<ManagedProcessExit, DispatchError> {
        ManagedProcessExit::new(self.exit_code, 0, false, 32, false)
    }
}

struct CancelOnExitLauncher {
    repository: RuntimeRepository,
    work_item_id: WorkItemId,
}

impl ProcessLauncher for CancelOnExitLauncher {
    fn launch(&self, _spec: &LaunchSpec) -> Result<Box<dyn ManagedProcess>, DispatchError> {
        Ok(Box::new(CancelOnExitProcess {
            repository: self.repository.clone(),
            work_item_id: self.work_item_id.clone(),
            identity: ManagedProcessIdentity::new(777, 4_000, "job/cancel")
                .expect("cancel identity"),
        }))
    }
}

struct CancelOnExitProcess {
    repository: RuntimeRepository,
    work_item_id: WorkItemId,
    identity: ManagedProcessIdentity,
}

impl ManagedProcess for CancelOnExitProcess {
    fn identity(&self) -> &ManagedProcessIdentity {
        &self.identity
    }

    fn wait_timeout(
        &mut self,
        _timeout: Duration,
    ) -> Result<Option<ManagedProcessExit>, DispatchError> {
        assert_eq!(
            self.repository
                .request_cancel(&self.work_item_id, 1, 4_040)
                .expect("request cancel at exit"),
            CancelRequestOutcome::Requested
        );
        Ok(Some(
            ManagedProcessExit::new(0, 64, false, 0, false).expect("simultaneous successful exit"),
        ))
    }

    fn terminate(&mut self) -> Result<ManagedProcessExit, DispatchError> {
        ManagedProcessExit::new(1, 0, false, 0, false)
    }
}
