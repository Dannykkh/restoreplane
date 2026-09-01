use dbbackup_catalog::{
    DatabaseObjectDiscoverySpec, DatabaseTargetSpec, DestinationSpec, DiscoveredDatabaseObjectSpec,
    ExplicitBackupScopeSpec, OccurrenceMaterializationSpec, OperationalStore, PlanDraftSpec,
};
use dbbackup_domain::{
    BackupPlanId, BackupScopeId, ConnectionProfileId, DatabaseTargetId, DestinationId,
    DestinationRequirement, OccurrenceOrigin, OperationRunId, PlanRevisionId,
};
use rusqlite::{params, Connection};
use tempfile::TempDir;

#[test]
fn target_gets_a_compatible_whole_database_scope() {
    let fixture = Fixture::new();
    fixture.seed_target();

    let scopes = fixture
        .store
        .backup_scopes()
        .list_for_target(&fixture.target_id)
        .expect("list scopes");
    assert_eq!(scopes.len(), 1);
    assert_eq!(scopes[0].id().as_str(), fixture.target_id.as_str());
    assert_eq!(scopes[0].mode().as_str(), "whole_database");
    assert_eq!(scopes[0].resolved_object_count(), 0);
    assert_eq!(scopes[0].source_fingerprint(), "database-fingerprint");
}

#[test]
fn explicit_scope_is_built_only_from_persisted_eligible_discovery_and_bound_immutably() {
    let fixture = Fixture::new();
    fixture.seed_plan_draft();
    fixture.seed_discovery_operation();
    let scopes = fixture.store.backup_scopes();

    scopes
        .record_object_discovery(DatabaseObjectDiscoverySpec::new(
            fixture.discovery_id.clone(),
            fixture.target_id.clone(),
            "a".repeat(64),
            vec![
                DiscoveredDatabaseObjectSpec::eligible_table(
                    "opaque:orders",
                    "public",
                    "orders",
                    "b".repeat(64),
                ),
                DiscoveredDatabaseObjectSpec::eligible_table(
                    "opaque:invoices",
                    "billing",
                    "invoices",
                    "c".repeat(64),
                ),
                DiscoveredDatabaseObjectSpec::excluded(
                    "opaque:view",
                    "public",
                    "orders_view",
                    "view",
                    "not_a_base_table",
                    "d".repeat(64),
                ),
            ],
            200,
        ))
        .expect("record object discovery");
    Connection::open(&fixture.db_path)
        .expect("open eligibility writer")
        .execute(
            "UPDATE discovered_database_objects
             SET eligibility = 'unsupported'
             WHERE operation_run_id = ?1 AND engine_object_id = 'opaque:view'",
            [fixture.discovery_id.as_str()],
        )
        .expect("record exact unsupported wire eligibility");

    let page = scopes
        .object_page(&fixture.target_id, None, 2)
        .expect("first object page");
    assert_eq!(page.total_count(), 3);
    assert_eq!(page.eligible_count(), 2);
    assert_eq!(page.items().len(), 2);
    assert!(page.next_cursor().is_some());
    assert_eq!(page.inventory_hash_blake3(), "a".repeat(64));

    let eligible_first = scopes
        .object_page_filtered(
            &fixture.target_id,
            &fixture.discovery_id,
            None,
            1,
            None,
            None,
            Some("eligible"),
        )
        .expect("first filtered eligible object page");
    assert_eq!(eligible_first.total_count(), 2);
    assert_eq!(eligible_first.items().len(), 1);
    let eligible_cursor = eligible_first
        .next_cursor()
        .expect("filtered object cursor")
        .to_owned();
    let eligible_second = scopes
        .object_page_filtered(
            &fixture.target_id,
            &fixture.discovery_id,
            Some(&eligible_cursor),
            1,
            None,
            None,
            Some("eligible"),
        )
        .expect("second filtered eligible object page");
    assert_eq!(eligible_second.total_count(), 2);
    assert_eq!(eligible_second.items().len(), 1);
    assert_ne!(
        eligible_first.items()[0].engine_object_id,
        eligible_second.items()[0].engine_object_id
    );
    assert_eq!(eligible_second.next_cursor(), None);

    let public_order = scopes
        .object_page_filtered(
            &fixture.target_id,
            &fixture.discovery_id,
            None,
            100,
            Some("ORDERS"),
            Some("public"),
            Some("eligible"),
        )
        .expect("search and schema filtered page");
    assert_eq!(public_order.total_count(), 1);
    assert_eq!(public_order.items()[0].object_name, "orders");

    let unsupported = scopes
        .object_page_filtered(
            &fixture.target_id,
            &fixture.discovery_id,
            None,
            100,
            None,
            None,
            Some("unsupported"),
        )
        .expect("unsupported object filter");
    assert_eq!(unsupported.total_count(), 1);
    assert_eq!(unsupported.items()[0].eligibility, "unsupported");

    let stale_discovery = scopes
        .object_page_filtered(
            &fixture.target_id,
            &OperationRunId::new(),
            None,
            100,
            None,
            None,
            None,
        )
        .expect_err("an unrelated discovery run must fail closed");
    assert_eq!(stale_discovery.code(), "OPERATIONAL_REPOSITORY_NOT_FOUND");

    let created = scopes
        .create_explicit(ExplicitBackupScopeSpec::new(
            fixture.scope_id.clone(),
            fixture.target_id.clone(),
            "업무 테이블",
            fixture.discovery_id.clone(),
            vec!["opaque:invoices".to_owned(), "opaque:orders".to_owned()],
            210,
        ))
        .expect("create explicit scope");
    assert_eq!(created.mode().as_str(), "explicit_tables");
    assert_eq!(created.resolved_object_count(), 2);
    assert_ne!(created.source_fingerprint(), "database-fingerprint");

    scopes
        .bind_revision_scope(&fixture.revision_id, &fixture.scope_id, 211)
        .expect("bind scope to draft revision");
    fixture
        .store
        .configuration()
        .add_revision_destination(
            &fixture.revision_id,
            &fixture.destination_id,
            DestinationRequirement::Required,
            212,
        )
        .expect("destination membership");
    fixture
        .store
        .configuration()
        .publish_revision(&fixture.revision_id, 220)
        .expect("publish revision");
    fixture
        .store
        .runtime()
        .materialize_occurrence(OccurrenceMaterializationSpec::due(
            fixture.revision_id.clone(),
            OccurrenceOrigin::Scheduled,
            300,
            300,
        ))
        .expect("materialize selected scope");

    let reader = Connection::open(&fixture.db_path).expect("reader");
    let bound_scope: (String, String, i64) = reader
        .query_row(
            "SELECT backup_scope_id, scope_mode, resolved_object_count
             FROM plan_revision_scopes WHERE plan_revision_id = ?1",
            [fixture.revision_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("revision scope");
    assert_eq!(
        bound_scope,
        (
            fixture.scope_id.to_string(),
            "explicit_tables".to_owned(),
            2
        )
    );
    let run_scope: (String, String, String, i64) = reader
        .query_row(
            "SELECT run.source_fingerprint, snapshot.backup_scope_id,
                    snapshot.scope_mode, snapshot.resolved_object_count
             FROM backup_runs AS run
             JOIN backup_run_scopes AS snapshot ON snapshot.backup_run_id = run.id
             WHERE run.plan_revision_id = ?1",
            [fixture.revision_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("run scope");
    assert_eq!(run_scope.0, created.source_fingerprint());
    assert_eq!(run_scope.1, fixture.scope_id.as_str());
    assert_eq!(run_scope.2, "explicit_tables");
    assert_eq!(run_scope.3, 2);

    let missing = scopes
        .create_explicit(ExplicitBackupScopeSpec::new(
            BackupScopeId::new(),
            fixture.target_id.clone(),
            "Invalid selection",
            fixture.discovery_id.clone(),
            vec!["opaque:missing".to_owned()],
            230,
        ))
        .expect_err("unobserved object must fail closed");
    assert_eq!(missing.code(), "BACKUP_SCOPE_OBJECT_MISSING");
}

#[test]
fn empty_database_inventory_is_a_successful_page_not_missing_evidence() {
    let fixture = Fixture::new();
    fixture.seed_target();
    fixture.seed_discovery_operation();

    fixture
        .store
        .backup_scopes()
        .record_object_discovery(DatabaseObjectDiscoverySpec::new(
            fixture.discovery_id.clone(),
            fixture.target_id.clone(),
            blake3::hash(&[]).to_hex().to_string(),
            Vec::new(),
            200,
        ))
        .expect("record empty inventory");

    let page = fixture
        .store
        .backup_scopes()
        .object_page(&fixture.target_id, None, 50)
        .expect("empty discovery page");
    assert!(page.items().is_empty());
    assert_eq!(page.total_count(), 0);
    assert_eq!(page.eligible_count(), 0);
    assert_eq!(page.discovery_run_id(), &fixture.discovery_id);
}

struct Fixture {
    _temp: TempDir,
    db_path: std::path::PathBuf,
    store: OperationalStore,
    connection_id: ConnectionProfileId,
    target_id: DatabaseTargetId,
    scope_id: BackupScopeId,
    destination_id: DestinationId,
    plan_id: BackupPlanId,
    revision_id: PlanRevisionId,
    discovery_id: OperationRunId,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().expect("temp directory");
        let db_path = temp.path().join("operational.db");
        let store = OperationalStore::open(db_path.clone()).expect("open store");
        Self {
            _temp: temp,
            db_path,
            store,
            connection_id: ConnectionProfileId::new(),
            target_id: DatabaseTargetId::new(),
            scope_id: BackupScopeId::new(),
            destination_id: DestinationId::new(),
            plan_id: BackupPlanId::new(),
            revision_id: PlanRevisionId::new(),
            discovery_id: OperationRunId::new(),
        }
    }

    fn seed_target(&self) {
        let configuration = self.store.configuration();
        configuration
            .insert_connection(dbbackup_catalog::ConnectionProfileSpec::tcp(
                self.connection_id.clone(),
                "primary",
                "postgresql",
                "localhost",
                5432,
                100,
            ))
            .expect("connection");
        configuration
            .insert_target(DatabaseTargetSpec::new(
                self.target_id.clone(),
                self.connection_id.clone(),
                "database:app",
                "app",
                "app",
                "database-fingerprint",
                110,
            ))
            .expect("target");
    }

    fn seed_plan_draft(&self) {
        self.seed_target();
        let configuration = self.store.configuration();
        configuration
            .insert_destination(DestinationSpec::local(
                self.destination_id.clone(),
                "local",
                r#"{"path":"D:\\backup"}"#,
                120,
            ))
            .expect("destination");
        configuration
            .insert_plan_draft(PlanDraftSpec::manual(
                self.plan_id.clone(),
                self.revision_id.clone(),
                "selected tables",
                1,
                "e".repeat(64),
                130,
            ))
            .expect("draft");
    }

    fn seed_discovery_operation(&self) {
        let connection = Connection::open(&self.db_path).expect("direct writer");
        connection
            .execute_batch("PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")
            .expect("writer pragmas");
        connection
            .execute(
                "INSERT INTO operation_runs(
                     id, operation_kind, subject_type, subject_id, request_json,
                     state, created_at_ms, updated_at_ms
                 ) VALUES (?1, 'connection_discovery', 'database_target', ?2,
                           '{\"kind\":\"backup_scope_object_discovery\"}',
                           'running', 190, 190)",
                params![self.discovery_id.as_str(), self.target_id.as_str()],
            )
            .expect("discovery operation");
    }
}
