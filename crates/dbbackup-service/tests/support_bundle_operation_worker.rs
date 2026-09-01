use std::sync::Arc;

use dbbackup_catalog::{OperationWorkSpec, OperationalStore};
use dbbackup_domain::{OperationRunId, WorkItemId};
use dbbackup_service::operations::{
    ConnectionInspection, ConnectionInspector, ConnectionOperationError, DatabaseCredentials,
    ServiceOperationWorker, WorkerTick,
};
use tempfile::TempDir;

struct UnusedInspector;

impl ConnectionInspector for UnusedInspector {
    fn inspect(
        &self,
        _profile: &dbbackup_catalog::ConnectionExecutionBinding,
        _credentials: Option<&DatabaseCredentials>,
        _discover: bool,
    ) -> Result<ConnectionInspection, ConnectionOperationError> {
        panic!("support bundle must not invoke a database inspector");
    }
}

#[test]
fn support_bundle_is_claimed_and_committed_as_a_durable_operation_run() {
    let temp = TempDir::new().expect("temp directory");
    let store =
        OperationalStore::open(temp.path().join("operational.db")).expect("operational store");
    let operation_run_id = OperationRunId::new();
    let work_item_id = WorkItemId::new();
    store
        .runtime()
        .enqueue_operation_work(OperationWorkSpec::new(
            operation_run_id.clone(),
            work_item_id.clone(),
            "support_bundle",
            "service_instance",
            "local",
            r#"{"schemaVersion":1}"#,
            10_000,
        ))
        .expect("enqueue support bundle");
    let worker = ServiceOperationWorker::with_clock(
        store.clone(),
        None,
        Arc::new(UnusedInspector),
        "support-worker",
        "support-boot",
        fixed_clock,
    )
    .with_support_bundle_root(temp.path().join("support-bundles"));

    assert_eq!(
        worker.run_once().expect("support worker tick"),
        WorkerTick::Completed(work_item_id.clone())
    );
    let work = store
        .console_query()
        .work_item(work_item_id.as_str())
        .expect("committed work item");
    assert_eq!(work.state, "succeeded");
    assert_eq!(
        work.operation_run_id.as_deref(),
        Some(operation_run_id.as_str())
    );
    assert_eq!(work.operation_kind.as_deref(), Some("support_bundle"));
    let result = work.result.expect("support bundle result");
    assert_eq!(result["schemaVersion"], 1);
    assert_eq!(result["entryCount"], 5);
    assert_eq!(result["checksumBlake3"].as_str().map(str::len), Some(64));
    let path = result["path"].as_str().expect("bundle path");
    assert!(std::path::Path::new(path).exists());
}

fn fixed_clock() -> i64 {
    10_000
}
