use std::sync::Arc;

use restoreplane_catalog::{
    ConnectionProfileSpec, DatabaseTargetSpec, OperationWorkSpec, OperationalStore,
};
use restoreplane_domain::{ConnectionProfileId, DatabaseTargetId, OperationRunId, WorkItemId};
use restoreplane_service::operations::{
    ConnectionInspection, ConnectionInspector, ConnectionOperationError, DatabaseCredentials,
    DatabaseObjectInspection, DiscoveredDatabaseObject, ServiceOperationWorker, WorkerTick,
};
use tempfile::TempDir;

struct ScopeInspector;

impl ConnectionInspector for ScopeInspector {
    fn inspect(
        &self,
        _profile: &restoreplane_catalog::ConnectionExecutionBinding,
        _credentials: Option<&DatabaseCredentials>,
        _discover: bool,
    ) -> Result<ConnectionInspection, ConnectionOperationError> {
        panic!("database object discovery must use the target-scoped inspector")
    }

    fn inspect_database_objects(
        &self,
        profile: &restoreplane_catalog::ConnectionExecutionBinding,
        _credentials: Option<&DatabaseCredentials>,
        target: &restoreplane_catalog::SelectedDatabaseTarget,
    ) -> Result<DatabaseObjectInspection, ConnectionOperationError> {
        assert_eq!(profile.engine(), "postgresql");
        assert_eq!(target.display_name(), "app");
        Ok(DatabaseObjectInspection::new(vec![
            DiscoveredDatabaseObject::eligible_table("relation:42", "public", "accounts"),
            DiscoveredDatabaseObject::eligible_table("relation:43", "sales", "orders"),
            DiscoveredDatabaseObject::excluded(
                "relation:44",
                "sales",
                "orders_all",
                "partitioned_table",
                "partitioned parent",
            ),
        ]))
    }
}

#[test]
fn target_scoped_discovery_persists_paged_exact_table_inventory() {
    let temp = TempDir::new().expect("temp directory");
    let store =
        OperationalStore::open(temp.path().join("operational.db")).expect("operational store");
    let connection_id = ConnectionProfileId::new();
    store
        .configuration()
        .insert_connection(ConnectionProfileSpec::tcp(
            connection_id.clone(),
            "PostgreSQL",
            "postgresql",
            "db.internal",
            5432,
            100,
        ))
        .expect("connection");
    let target_id = DatabaseTargetId::new();
    store
        .configuration()
        .insert_target(DatabaseTargetSpec::new(
            target_id.clone(),
            connection_id,
            "database:app",
            "app",
            "app",
            "a".repeat(64),
            100,
        ))
        .expect("target");
    let operation_id = OperationRunId::new();
    let work_id = WorkItemId::new();
    store
        .runtime()
        .enqueue_operation_work(OperationWorkSpec::new(
            operation_id.clone(),
            work_id.clone(),
            "connection_discovery",
            "database_target",
            target_id.as_str(),
            serde_json::json!({
                "schemaVersion": 2,
                "kind": "backup_scope_object_discovery",
                "databaseTargetId": target_id.as_str(),
            })
            .to_string(),
            200,
        ))
        .expect("enqueue discovery");
    let worker = ServiceOperationWorker::with_clock(
        store.clone(),
        None,
        Arc::new(ScopeInspector),
        "scope-worker",
        "scope-boot",
        || 1_000,
    );

    assert_eq!(
        worker.run_once().expect("run target discovery"),
        WorkerTick::Completed(work_id)
    );
    let page = store
        .backup_scopes()
        .object_page(&target_id, None, 2)
        .expect("first page");
    assert_eq!(page.discovery_run_id(), &operation_id);
    assert_eq!(page.total_count(), 3);
    assert_eq!(page.eligible_count(), 2);
    assert_eq!(page.items().len(), 2);
    assert!(page.next_cursor().is_some());
    assert_eq!(page.items()[0].object_name, "accounts");
    assert_eq!(page.items()[1].object_name, "orders");
    assert!(page
        .items()
        .iter()
        .all(|object| object.object_fingerprint_blake3.len() == 64));
}
