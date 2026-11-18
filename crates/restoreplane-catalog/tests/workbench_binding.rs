use restoreplane_catalog::{
    rusqlite::Connection, ConnectionProfileSpec, DatabaseTargetSpec, OperationalStore,
    RepositoryError, WorkspaceSpec,
};
use restoreplane_domain::workbench::{Route, Scope};
use restoreplane_domain::{ConnectionProfileId, DatabaseTargetId, WorkspaceId};
use std::str::FromStr;

struct Fixture {
    _temp: tempfile::TempDir,
    path: std::path::PathBuf,
    store: OperationalStore,
    scope: Scope,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("store.db");
        let store = OperationalStore::open(path.clone()).unwrap();
        let scope = Scope {
            workspace_id: WorkspaceId::from_str(restoreplane_catalog::DEFAULT_WORKSPACE_ID)
                .unwrap(),
            connection_profile_id: ConnectionProfileId::new(),
            database_target_id: DatabaseTargetId::new(),
        };
        store
            .configuration()
            .insert_connection(
                ConnectionProfileSpec::tcp(
                    scope.connection_profile_id.clone(),
                    "Workbench PG",
                    "postgresql",
                    "db.example.test",
                    5432,
                    100,
                )
                .with_tls_mode("verify_full"),
            )
            .unwrap();
        store
            .configuration()
            .insert_target(DatabaseTargetSpec::new(
                scope.database_target_id.clone(),
                scope.connection_profile_id.clone(),
                "oid:42",
                "Orders",
                "Orders",
                "source-fingerprint",
                110,
            ))
            .unwrap();
        Self {
            _temp: temp,
            path,
            store,
            scope,
        }
    }
}

#[test]
fn binding_reads_real_v4_identity_without_sql_or_secret_export() {
    let fixture = Fixture::new();
    let binding = fixture
        .store
        .workbench_binding()
        .snapshot(&fixture.scope, &"a".repeat(64))
        .unwrap();
    assert!(
        matches!(binding.descriptor.route,Route::Tcp{ref host,port:5432} if host=="db.example.test")
    );
    assert_eq!(binding.descriptor.tls.mode, "verify-full");
    assert!(binding.observed_conflicts.is_empty());
    let public = serde_json::to_string(&binding.descriptor).unwrap();
    assert!(!public.contains("secretRefId"));
    let diagnostics = fixture.store.writer_diagnostics().unwrap();
    assert_eq!(diagnostics.user_version, 4);
}

#[test]
fn exact_database_name_preserves_case_instead_of_search_normalization() {
    let fixture = Fixture::new();
    let connection = restoreplane_catalog::rusqlite::Connection::open(&fixture.path).unwrap();
    connection.execute("UPDATE database_targets SET display_name='Orders',normalized_name='orders' WHERE id=?1", [fixture.scope.database_target_id.as_str()]).unwrap();
    let binding = fixture
        .store
        .workbench_binding()
        .snapshot(&fixture.scope, &"a".repeat(64))
        .unwrap();
    assert_eq!(binding.descriptor.database.name, "Orders");
    assert_eq!(binding.revision_input["databaseSearchName"], "orders");
}

#[test]
fn cross_workspace_and_archived_owner_membership_are_rejected() {
    let fixture = Fixture::new();
    let other = WorkspaceId::new();
    fixture
        .store
        .workspace()
        .create(WorkspaceSpec::new(other.clone(), "Other", 200))
        .unwrap();
    let mut forged = fixture.scope.clone();
    forged.workspace_id = other;
    assert!(matches!(
        fixture
            .store
            .workbench_binding()
            .snapshot(&forged, &"a".repeat(64)),
        Err(RepositoryError::WorkspaceScopeMismatch)
    ));
    Connection::open(&fixture.path)
        .unwrap()
        .execute(
            "UPDATE workspaces SET state='archived' WHERE id=?1",
            [fixture.scope.workspace_id.as_str()],
        )
        .unwrap();
    assert!(fixture
        .store
        .workbench_binding()
        .snapshot(&fixture.scope, &"a".repeat(64))
        .is_err());
}

#[test]
fn unknown_connect_options_fail_closed_and_known_profile_changes_change_revision_input() {
    let fixture = Fixture::new();
    let before = fixture
        .store
        .workbench_binding()
        .snapshot(&fixture.scope, &"a".repeat(64))
        .unwrap()
        .revision_input;
    let connection = Connection::open(&fixture.path).unwrap();
    connection
        .execute(
            "UPDATE connection_profiles SET host='new.example.test' WHERE id=?1",
            [fixture.scope.connection_profile_id.as_str()],
        )
        .unwrap();
    let after = fixture
        .store
        .workbench_binding()
        .snapshot(&fixture.scope, &"a".repeat(64))
        .unwrap()
        .revision_input;
    assert_ne!(before, after);
    connection.execute("UPDATE connection_profiles SET adapter_config_json='{\"unknownProxy\":\"host\"}' WHERE id=?1",[fixture.scope.connection_profile_id.as_str()]).unwrap();
    assert!(matches!(
        fixture
            .store
            .workbench_binding()
            .snapshot(&fixture.scope, &"a".repeat(64)),
        Err(RepositoryError::WorkbenchBindingUnproven)
    ));
}

#[test]
fn active_maintenance_is_observed_in_the_same_binding_read() {
    let fixture = Fixture::new();
    Connection::open(&fixture.path).unwrap().execute("INSERT INTO operation_runs(id,operation_kind,subject_type,subject_id,request_json,state,created_at_ms,updated_at_ms) VALUES('maintenance-1','control_store_restore','service','service','{}','running',200,200)",[]).unwrap();
    let binding = fixture
        .store
        .workbench_binding()
        .snapshot(&fixture.scope, &"a".repeat(64))
        .unwrap();
    assert_eq!(binding.observed_conflicts.len(), 1);
    assert_eq!(binding.observed_conflicts[0].kind, "maintenance");
}
