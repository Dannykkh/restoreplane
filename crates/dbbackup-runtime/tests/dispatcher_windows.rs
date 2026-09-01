#![cfg(windows)]

use std::sync::Arc;
use std::time::Duration;

use dbbackup_catalog::{OperationWorkSpec, OperationalStore};
use dbbackup_domain::{OperationRunId, WorkItemId};
use dbbackup_runtime::{
    hash_executable, DispatchConfig, DispatchError, DispatchOutcome, DispatchPlan, DispatchPlanner,
    Dispatcher, LaunchSpec, SystemRuntimeClock, WindowsProcessLauncher,
};
use tempfile::TempDir;

const MODE_ENV: &str = "DBBACKUP_DISPATCH_TEST_MODE";

#[test]
fn real_supervisor_completes_a_durable_operation_through_the_dispatcher() {
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
            "real-supervisor",
            "{}",
            unix_time_ms(),
        ))
        .expect("enqueue real operation");

    let executable = std::env::current_exe().expect("test executable");
    let launch = LaunchSpec::new(
        executable.clone(),
        temp.path().to_path_buf(),
        hash_executable(&executable).expect("hash executable"),
    )
    .expect("launch spec")
    .with_args(["--exact", "helper_operation", "--nocapture"])
    .expect("helper args")
    .with_environment([(MODE_ENV, "success")])
    .expect("restricted helper environment");
    let planner = Arc::new(RealPlanner {
        plan: DispatchPlan::new(launch, "native_tool", Vec::new(), None)
            .expect("dispatch plan")
            .with_wait_policy(Duration::from_millis(10), Duration::from_secs(10))
            .expect("wait policy"),
    });
    let dispatcher = Dispatcher::new(
        repository,
        planner,
        Arc::new(WindowsProcessLauncher),
        Arc::new(SystemRuntimeClock),
        DispatchConfig::new("worker-real", "boot-real", 5_000, 100).expect("dispatch config"),
    );

    assert_eq!(
        dispatcher.dispatch_once().expect("real dispatch"),
        DispatchOutcome::Completed {
            work_item_id: work_item_id.to_string(),
            attempt_no: 1,
            exit_code: 0,
        }
    );

    let direct =
        dbbackup_catalog::rusqlite::Connection::open(db_path).expect("open real dispatch probe");
    let evidence: (String, String, String, i64, i64) = direct
        .query_row(
            "SELECT
                 work_items.state,
                 work_attempts.state,
                 process_instances.state,
                 process_instances.process_id,
                 process_instances.process_start_time_ms
             FROM work_items
             JOIN work_attempts
               ON work_attempts.work_item_id = work_items.id
             JOIN process_instances
               ON process_instances.work_attempt_id = work_attempts.id
             WHERE work_items.id = ?1",
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
        .expect("query real process evidence");
    assert_eq!(evidence.0, "succeeded");
    assert_eq!(evidence.1, "succeeded");
    assert_eq!(evidence.2, "exited");
    assert!(evidence.3 > 0);
    assert!(evidence.4 > 0);
}

#[test]
fn helper_operation() {
    if std::env::var(MODE_ENV).as_deref() == Ok("success") {
        println!("dispatcher helper stdout");
        eprintln!("dispatcher helper stderr");
    }
}

struct RealPlanner {
    plan: DispatchPlan,
}

impl DispatchPlanner for RealPlanner {
    fn plan(&self, _claim: &dbbackup_catalog::ClaimedWork) -> Result<DispatchPlan, DispatchError> {
        Ok(self.plan.clone())
    }
}

fn unix_time_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_millis(),
    )
    .expect("millisecond timestamp")
}
