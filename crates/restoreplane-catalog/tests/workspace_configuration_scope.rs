use std::str::FromStr;

use restoreplane_catalog::{
    ConnectionProfileSpec, DestinationSpec, OperationalStore, PlanDraftSpec, WorkspaceSpec,
};
use restoreplane_domain::{
    BackupPlanId, ConnectionProfileId, DestinationId, PlanRevisionId, WorkspaceId,
};
use tempfile::TempDir;

#[test]
fn configuration_and_console_queries_are_isolated_by_workspace() {
    let temp = TempDir::new().expect("temp directory");
    let store = OperationalStore::open(temp.path().join("operational.db")).expect("store");
    let analytics = workspace_id("11111111-1111-4111-8111-111111111111");
    let commerce = workspace_id("22222222-2222-4222-8222-222222222222");
    store
        .workspace()
        .create(WorkspaceSpec::new(analytics.clone(), "Analytics", 10))
        .expect("analytics workspace");
    store
        .workspace()
        .create(WorkspaceSpec::new(commerce.clone(), "Commerce", 11))
        .expect("commerce workspace");

    let analytics_connection = ConnectionProfileId::new();
    let commerce_connection = ConnectionProfileId::new();
    store
        .configuration()
        .insert_connection_in_workspace(
            &analytics,
            ConnectionProfileSpec::file(
                analytics_connection.clone(),
                "Primary SQLite",
                "sqlite",
                "D:/data/analytics.db",
                20,
            ),
        )
        .expect("analytics connection");
    store
        .configuration()
        .insert_connection_in_workspace(
            &commerce,
            ConnectionProfileSpec::file(
                commerce_connection.clone(),
                "Primary SQLite",
                "sqlite",
                "D:/data/commerce.db",
                21,
            ),
        )
        .expect("commerce connection with same display name");

    let analytics_destination = DestinationId::new();
    store
        .configuration()
        .insert_destination_in_workspace(
            &analytics,
            DestinationSpec::local(
                analytics_destination,
                "Local vault",
                r#"{"path":"D:/backup/analytics"}"#,
                22,
            ),
        )
        .expect("analytics destination");
    store
        .configuration()
        .insert_plan_draft_in_workspace(
            &analytics,
            PlanDraftSpec::manual(
                BackupPlanId::new(),
                PlanRevisionId::new(),
                "Daily protection",
                1,
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                23,
            ),
        )
        .expect("analytics plan");

    let analytics_connections = store
        .console_query()
        .connections_in_workspace(analytics.as_str(), None, 100)
        .expect("analytics connections");
    let commerce_connections = store
        .console_query()
        .connections_in_workspace(commerce.as_str(), None, 100)
        .expect("commerce connections");
    assert_eq!(analytics_connections.total_count, 1);
    assert_eq!(commerce_connections.total_count, 1);
    assert_eq!(
        analytics_connections.items[0].id,
        analytics_connection.as_str()
    );
    assert_eq!(
        commerce_connections.items[0].id,
        commerce_connection.as_str()
    );

    assert_eq!(
        store
            .console_query()
            .destinations_in_workspace(analytics.as_str(), None, 100)
            .expect("analytics destinations")
            .total_count,
        1
    );
    assert_eq!(
        store
            .console_query()
            .destinations_in_workspace(commerce.as_str(), None, 100)
            .expect("commerce destinations")
            .total_count,
        0
    );
    assert_eq!(
        store
            .console_query()
            .plans_in_workspace(analytics.as_str(), None, 100)
            .expect("analytics plans")
            .total_count,
        1
    );

    let error = store
        .console_query()
        .connection_in_workspace(analytics.as_str(), commerce_connection.as_str())
        .expect_err("cross-workspace direct lookup");
    assert_eq!(error.code(), "OPERATIONAL_REPOSITORY_NOT_FOUND");
}

fn workspace_id(value: &str) -> WorkspaceId {
    WorkspaceId::from_str(value).expect("workspace id")
}
