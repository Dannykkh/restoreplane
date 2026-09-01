use std::str::FromStr;

use dbbackup_catalog::{
    ClaimRequest, ConnectionProfileSpec, OperationWorkSpec, OperationalStore, WorkspaceSpec,
};
use dbbackup_domain::{ConnectionProfileId, OperationRunId, WorkItemId, WorkspaceId};
use tempfile::TempDir;

#[test]
fn claim_rotation_is_fair_while_idle_capacity_remains_work_conserving() {
    let temp = TempDir::new().expect("temp directory");
    let store = OperationalStore::open(temp.path().join("operational.db")).expect("store");
    let alpha = workspace_id("11111111-1111-4111-8111-111111111111");
    let beta = workspace_id("22222222-2222-4222-8222-222222222222");
    store
        .workspace()
        .create(WorkspaceSpec::new(alpha.clone(), "Alpha", 10))
        .expect("alpha");
    store
        .workspace()
        .create(WorkspaceSpec::new(beta.clone(), "Beta", 11))
        .expect("beta");

    let alpha_connection = scoped_connection(&store, &alpha, "alpha", 20);
    let beta_connection = scoped_connection(&store, &beta, "beta", 21);
    let alpha_first = enqueue_probe(&store, &alpha_connection, 100);
    let alpha_second = enqueue_probe(&store, &alpha_connection, 101);
    let beta_first = enqueue_probe(&store, &beta_connection, 102);

    let first = store
        .runtime()
        .claim_next(ClaimRequest::new("worker-1", "boot", 200, 1_000))
        .expect("first claim")
        .expect("first work");
    let second = store
        .runtime()
        .claim_next(ClaimRequest::new("worker-2", "boot", 201, 1_000))
        .expect("second claim")
        .expect("second work");
    let third = store
        .runtime()
        .claim_next(ClaimRequest::new("worker-3", "boot", 202, 1_000))
        .expect("third claim")
        .expect("third work");

    assert_eq!(first.work_item_id(), &alpha_first);
    assert_eq!(second.work_item_id(), &beta_first);
    assert_eq!(third.work_item_id(), &alpha_second);
}

fn scoped_connection(
    store: &OperationalStore,
    workspace_id: &WorkspaceId,
    name: &str,
    now_ms: i64,
) -> ConnectionProfileId {
    let id = ConnectionProfileId::new();
    store
        .configuration()
        .insert_connection_in_workspace(
            workspace_id,
            ConnectionProfileSpec::file(
                id.clone(),
                format!("{name} SQLite"),
                "sqlite",
                format!("D:/data/{name}.db"),
                now_ms,
            ),
        )
        .expect("connection");
    id
}

fn enqueue_probe(
    store: &OperationalStore,
    connection_id: &ConnectionProfileId,
    now_ms: i64,
) -> WorkItemId {
    let work_item_id = WorkItemId::new();
    store
        .runtime()
        .enqueue_operation_work(OperationWorkSpec::new(
            OperationRunId::new(),
            work_item_id.clone(),
            "connection_probe",
            "connection_profile",
            connection_id.as_str(),
            format!(r#"{{"schemaVersion":1,"connectionId":"{connection_id}"}}"#),
            now_ms,
        ))
        .expect("probe work");
    work_item_id
}

fn workspace_id(value: &str) -> WorkspaceId {
    WorkspaceId::from_str(value).expect("workspace id")
}
