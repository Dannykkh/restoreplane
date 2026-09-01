use std::sync::{Arc, Mutex};

use restoreplane_catalog::{ConnectionProfileSpec, OperationWorkSpec, OperationalStore};
use restoreplane_domain::{ConnectionProfileId, OperationRunId, WorkItemId};
use restoreplane_service::operations::{
    ConnectionInspection, ConnectionInspector, DiscoveredDatabase, NativeConnectionInspector,
    ServiceOperationWorker, WorkerTick,
};
use tempfile::TempDir;

#[derive(Default)]
struct RecordingInspector {
    calls: Mutex<Vec<(String, bool)>>,
}

impl ConnectionInspector for RecordingInspector {
    fn inspect(
        &self,
        profile: &restoreplane_catalog::ConnectionExecutionBinding,
        _credentials: Option<&restoreplane_service::operations::DatabaseCredentials>,
        discover: bool,
    ) -> Result<ConnectionInspection, restoreplane_service::operations::ConnectionOperationError>
    {
        self.calls
            .lock()
            .expect("recording inspector lock")
            .push((profile.engine().to_owned(), discover));
        Ok(ConnectionInspection::new(
            "PostgreSQL 17.5",
            serde_json::json!({
                "probe": "passed",
                "discover": discover,
                "transportSecurity": profile.tls_mode(),
            }),
            if discover {
                vec![
                    DiscoveredDatabase::eligible("database:orders", "orders"),
                    DiscoveredDatabase::eligible("database:billing", "billing"),
                ]
            } else {
                Vec::new()
            },
        ))
    }
}

#[test]
fn durable_worker_filters_supported_operations_and_commits_probe_and_discovery_evidence() {
    let temp = TempDir::new().expect("temp directory");
    let store =
        OperationalStore::open(temp.path().join("operational.db")).expect("open operational store");
    let connection_id = ConnectionProfileId::new();
    store
        .configuration()
        .insert_connection(
            ConnectionProfileSpec::tcp(
                connection_id.clone(),
                "Primary PostgreSQL",
                "postgresql",
                "db.internal",
                5432,
                100,
            )
            .with_tls_mode("verify_full"),
        )
        .expect("insert connection");

    let unrelated_work_id = enqueue(
        &store,
        "tool_rescan",
        "service",
        "local",
        serde_json::json!({"schemaVersion": 1}),
        150,
    );
    let probe_work_id = enqueue(
        &store,
        "connection_probe",
        "connection_profile",
        connection_id.as_str(),
        serde_json::json!({
            "schemaVersion": 1,
            "connectionId": connection_id.as_str(),
        }),
        200,
    );

    let inspector = Arc::new(RecordingInspector::default());
    let worker = ServiceOperationWorker::with_clock(
        store.clone(),
        None,
        inspector.clone(),
        "connection-worker-test",
        "connection-worker-boot",
        || 1_000,
    );
    assert_eq!(
        worker.run_once().expect("run probe"),
        WorkerTick::Completed(probe_work_id.clone())
    );
    assert_eq!(
        store
            .console_query()
            .work_item(unrelated_work_id.as_str())
            .expect("unrelated operation")
            .state,
        "queued",
        "the connection worker must not steal another operation worker's subject"
    );
    let probe = store
        .console_query()
        .work_item(probe_work_id.as_str())
        .expect("probe work");
    assert_eq!(probe.state, "succeeded");
    assert_eq!(
        probe.result.expect("probe result")["serverVersion"],
        "PostgreSQL 17.5"
    );
    let connection = store
        .console_query()
        .connection(connection_id.as_str())
        .expect("connection projection");
    assert_eq!(connection.state, "ready");
    assert_eq!(connection.last_probe_status.as_deref(), Some("passed"));

    let discovery_run_id = OperationRunId::new();
    let discovery_work_id = WorkItemId::new();
    store
        .runtime()
        .enqueue_operation_work(OperationWorkSpec::new(
            discovery_run_id.clone(),
            discovery_work_id.clone(),
            "connection_discovery",
            "connection_profile",
            connection_id.as_str(),
            serde_json::json!({
                "schemaVersion": 1,
                "connectionId": connection_id.as_str(),
            })
            .to_string(),
            1_000,
        ))
        .expect("enqueue discovery");

    assert_eq!(
        worker.run_once().expect("run discovery"),
        WorkerTick::Completed(discovery_work_id.clone())
    );
    let discovery = store
        .console_query()
        .work_item(discovery_work_id.as_str())
        .expect("discovery work");
    assert_eq!(discovery.state, "succeeded");
    assert_eq!(discovery.discovered.len(), 2);
    assert_eq!(discovery.discovered[0].database_name, "billing");
    assert_eq!(discovery.discovered[1].database_name, "orders");
    assert_eq!(discovery.discovered_count, 2);
    assert_eq!(discovery.eligible_discovered_count, 2);
    let work_wire = serde_json::to_value(&discovery).expect("serialize work summary");
    assert!(work_wire.get("discovered").is_none());
    assert_eq!(work_wire["discoveredCount"], 2);
    let discovery_page = store
        .console_query()
        .discovery_results_in_workspace(
            restoreplane_catalog::DEFAULT_WORKSPACE_ID,
            discovery_run_id.as_str(),
            None,
            1,
            None,
            Some("eligible"),
        )
        .expect("paged discovery result");
    assert_eq!(discovery_page.total_count, 2);
    assert_eq!(discovery_page.items.len(), 1);
    assert!(discovery_page.next_cursor.is_some());
    assert_eq!(
        inspector.calls.lock().expect("calls").as_slice(),
        &[
            ("postgresql".to_owned(), false),
            ("postgresql".to_owned(), true)
        ]
    );
}

fn enqueue(
    store: &OperationalStore,
    operation_kind: &str,
    subject_type: &str,
    subject_id: &str,
    request: serde_json::Value,
    available_at_ms: i64,
) -> WorkItemId {
    let work_item_id = WorkItemId::new();
    store
        .runtime()
        .enqueue_operation_work(OperationWorkSpec::new(
            OperationRunId::new(),
            work_item_id.clone(),
            operation_kind,
            subject_type,
            subject_id,
            request.to_string(),
            available_at_ms,
        ))
        .expect("enqueue operation");
    work_item_id
}

#[test]
fn native_sqlite_probe_and_discovery_use_file_identity_without_database_credentials() {
    let temp = TempDir::new().expect("temp directory");
    let source = temp.path().join("application.sqlite");
    let source_connection =
        restoreplane_catalog::rusqlite::Connection::open(&source).expect("open SQLite fixture");
    source_connection
        .execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE items(id INTEGER PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO items(value) VALUES ('one'), ('two');",
        )
        .expect("create SQLite fixture");
    drop(source_connection);

    let store =
        OperationalStore::open(temp.path().join("operational.db")).expect("open operational store");
    let connection_id = ConnectionProfileId::new();
    store
        .configuration()
        .insert_connection(ConnectionProfileSpec::file(
            connection_id.clone(),
            "SQLite application",
            "sqlite",
            source.display().to_string(),
            100,
        ))
        .expect("insert SQLite connection");
    let probe_work_id = enqueue(
        &store,
        "connection_probe",
        "connection_profile",
        connection_id.as_str(),
        serde_json::json!({
            "schemaVersion": 1,
            "connectionId": connection_id.as_str(),
        }),
        200,
    );
    let worker = ServiceOperationWorker::with_clock(
        store.clone(),
        None,
        Arc::new(NativeConnectionInspector::default()),
        "sqlite-worker-test",
        "sqlite-worker-boot",
        || 1_000,
    );
    assert_eq!(
        worker.run_once().expect("run SQLite probe"),
        WorkerTick::Completed(probe_work_id.clone())
    );
    let probe = store
        .console_query()
        .work_item(probe_work_id.as_str())
        .expect("probe projection");
    let result = probe.result.expect("probe result");
    assert!(result["serverVersion"]
        .as_str()
        .unwrap()
        .starts_with("SQLite "));
    assert_eq!(result["capabilities"]["onlineBackup"], "supported");
    assert_eq!(result["capabilities"]["journalMode"], "wal");
    assert!(result["capabilities"]["fileObjectId"]
        .as_str()
        .is_some_and(|value| !value.is_empty()));

    let discovery_work_id = enqueue(
        &store,
        "connection_discovery",
        "connection_profile",
        connection_id.as_str(),
        serde_json::json!({
            "schemaVersion": 1,
            "connectionId": connection_id.as_str(),
        }),
        1_000,
    );
    assert_eq!(
        worker.run_once().expect("run SQLite discovery"),
        WorkerTick::Completed(discovery_work_id.clone())
    );
    let discovery = store
        .console_query()
        .work_item(discovery_work_id.as_str())
        .expect("discovery projection");
    assert_eq!(discovery.discovered.len(), 1);
    assert_eq!(discovery.discovered[0].database_name, "application.sqlite");
    assert_eq!(discovery.discovered[0].eligibility, "eligible");
    assert_eq!(discovery.discovered_count, 1);
    assert_eq!(discovery.eligible_discovered_count, 1);
    let work_wire = serde_json::to_value(&discovery).expect("serialize SQLite work summary");
    assert!(work_wire.get("discovered").is_none());
}
