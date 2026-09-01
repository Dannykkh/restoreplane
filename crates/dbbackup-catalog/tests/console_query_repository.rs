use dbbackup_catalog::{
    ActivityEventSpec, ConnectionProfileSpec, DatabaseTargetSpec, DestinationSpec,
    DiscoveredDatabaseSpec, DiscoverySnapshotSpec, OccurrenceMaterializationSpec, OperationalStore,
    PlanDraftSpec,
};
use dbbackup_domain::{
    BackupPlanId, ConnectionProfileId, DatabaseTargetId, DestinationId, DestinationRequirement,
    OccurrenceOrigin, PlanRevisionId, WorkItemId,
};
use tempfile::TempDir;
use uuid::Uuid;

#[test]
fn console_inventory_queries_are_typed_bounded_and_secret_free() {
    let fixture = Fixture::new();
    fixture.seed_inventory();

    let query = fixture.store.console_query();
    let connections = query
        .connections(None, 25)
        .expect("query connection inventory");
    assert_eq!(connections.items.len(), 1);
    assert_eq!(connections.items[0].display_name, "Primary PostgreSQL");
    assert_eq!(connections.items[0].target_count, 1);
    assert!(connections.items[0].targets.is_empty());
    assert_eq!(connections.items[0].endpoint, "db.internal:5432");
    let targets = query
        .database_targets_in_workspace_at(
            dbbackup_catalog::DEFAULT_WORKSPACE_ID,
            None,
            100,
            Some(fixture.connection_id.as_str()),
            None,
            None,
            10_000,
        )
        .expect("query database target inventory");
    assert_eq!(targets.total_count, 1);
    assert_eq!(targets.items[0].display_name, "orders");

    let destinations = query
        .destinations(None, 25)
        .expect("query destination inventory");
    assert_eq!(destinations.items.len(), 1);
    assert_eq!(destinations.items[0].endpoint, "D:\\backup");
    let serialized = serde_json::to_string(&destinations).expect("serialize destinations");
    assert!(!serialized.contains("password"));
    assert!(!serialized.contains("secret"));

    let plans = query.plans(None, 25).expect("query plan inventory");
    assert_eq!(plans.items.len(), 1);
    assert_eq!(plans.items[0].target_count, 1);
    assert_eq!(plans.items[0].required_destination_count, 1);
}

#[test]
fn activity_query_uses_a_strict_keyset_cursor_and_never_returns_an_unbounded_page() {
    let fixture = Fixture::new();
    let events = (0..125)
        .map(|index| {
            ActivityEventSpec::info(
                format!("event-{index:03}"),
                1_000 + index,
                "BACKUP_PHASE",
                "work_item",
                "work-1",
                format!("phase {index:03}"),
            )
        })
        .collect();
    fixture
        .store
        .activity()
        .append_batch(events)
        .expect("append activity");

    let query = fixture.store.console_query();
    let first = query.activity(None, 50, None).expect("first activity page");
    assert_eq!(first.items.len(), 50);
    assert_eq!(first.total_count, 125);
    let cursor = first.next_cursor.expect("next activity cursor");

    let second = query
        .activity(Some(&cursor), 50, None)
        .expect("second activity page");
    assert_eq!(second.items.len(), 50);
    assert!(
        first.items.last().expect("last first").seq
            > second.items.first().expect("first second").seq
    );

    let error = query
        .activity(None, 501, None)
        .expect_err("unbounded activity pages must be rejected");
    assert_eq!(error.code(), "OPERATIONAL_RUNTIME_STATE_INVALID");
}

#[test]
fn target_list_filters_before_its_immutable_created_id_cursor_and_counts_exactly() {
    let fixture = Fixture::new();
    fixture.seed_inventory();
    let configuration = fixture.store.configuration();
    for (name, created_at_ms) in [("reporting-east", 150), ("reporting-west", 150)] {
        configuration
            .insert_target(DatabaseTargetSpec::new(
                DatabaseTargetId::new(),
                fixture.connection_id.clone(),
                format!("database:{name}"),
                name,
                name,
                format!("fingerprint:{name}"),
                created_at_ms,
            ))
            .expect("insert filtered target");
    }

    let query = fixture.store.console_query();
    let first = query
        .database_targets_in_workspace_at(
            dbbackup_catalog::DEFAULT_WORKSPACE_ID,
            None,
            1,
            Some(fixture.connection_id.as_str()),
            Some("active"),
            Some("REPORTING"),
            10_000,
        )
        .expect("first filtered target page");
    assert_eq!(first.total_count, 2);
    assert_eq!(first.items.len(), 1);
    let cursor = first.next_cursor.expect("created/id lookahead cursor");

    let second = query
        .database_targets_in_workspace_at(
            dbbackup_catalog::DEFAULT_WORKSPACE_ID,
            Some(&cursor),
            1,
            Some(fixture.connection_id.as_str()),
            Some("active"),
            Some("reporting"),
            10_000,
        )
        .expect("second filtered target page");
    assert_eq!(second.total_count, 2);
    assert_eq!(second.items.len(), 1);
    assert_ne!(first.items[0].id, second.items[0].id);
    assert_eq!(second.next_cursor, None);

    let connection = query
        .connection_in_workspace(
            dbbackup_catalog::DEFAULT_WORKSPACE_ID,
            fixture.connection_id.as_str(),
        )
        .expect("connection summary");
    assert_eq!(connection.target_count, 3);
    assert!(connection.targets.is_empty());

    let error = query
        .database_targets_in_workspace_at(
            dbbackup_catalog::DEFAULT_WORKSPACE_ID,
            None,
            101,
            None,
            None,
            None,
            10_000,
        )
        .expect_err("target pages over 100 must fail closed");
    assert_eq!(error.code(), "OPERATIONAL_RUNTIME_STATE_INVALID");
}

#[test]
fn discovery_results_filter_before_cursor_and_keep_filtered_total_count() {
    let fixture = Fixture::new();
    fixture.seed_inventory();
    let discovery_run_id = Uuid::new_v4().to_string();
    fixture
        .store
        .configuration()
        .record_discovery_snapshot(DiscoverySnapshotSpec::successful(
            &discovery_run_id,
            fixture.connection_id.clone(),
            vec![
                DiscoveredDatabaseSpec::eligible(
                    "database:analytics",
                    "analytics",
                    "analytics",
                    "fingerprint:analytics",
                ),
                DiscoveredDatabaseSpec::excluded(
                    "database:archive",
                    "archive",
                    "archive",
                    "system database",
                    "fingerprint:archive",
                ),
                DiscoveredDatabaseSpec::eligible(
                    "database:reporting",
                    "reporting",
                    "reporting",
                    "fingerprint:reporting",
                ),
            ],
            1_000,
            1_010,
        ))
        .expect("record discovery inventory");

    let query = fixture.store.console_query();
    let first = query
        .discovery_results_in_workspace(
            dbbackup_catalog::DEFAULT_WORKSPACE_ID,
            &discovery_run_id,
            None,
            1,
            None,
            Some("eligible"),
        )
        .expect("first eligible discovery page");
    assert_eq!(first.total_count, 2);
    let cursor = first.next_cursor.expect("discovery cursor");
    let second = query
        .discovery_results_in_workspace(
            dbbackup_catalog::DEFAULT_WORKSPACE_ID,
            &discovery_run_id,
            Some(&cursor),
            1,
            None,
            Some("eligible"),
        )
        .expect("second eligible discovery page");
    assert_eq!(second.total_count, 2);
    assert_ne!(
        first.items[0].engine_object_id,
        second.items[0].engine_object_id
    );
    assert_eq!(second.next_cursor, None);

    let searched = query
        .discovery_results_in_workspace(
            dbbackup_catalog::DEFAULT_WORKSPACE_ID,
            &discovery_run_id,
            None,
            100,
            Some("ARCHIVE"),
            Some("excluded"),
        )
        .expect("searched discovery page");
    assert_eq!(searched.total_count, 1);
    assert_eq!(searched.items[0].database_name, "archive");
}

#[test]
fn activity_page_is_byte_bounded_below_the_ipc_frame_limit() {
    let fixture = Fixture::new();
    let events = (0..100)
        .map(|index| {
            ActivityEventSpec::info(
                format!("large-event-{index:03}"),
                10_000 + index,
                "BACKUP_PHASE",
                "work_item",
                "work-1",
                "x".repeat(16 * 1024),
            )
        })
        .collect();
    fixture
        .store
        .activity()
        .append_batch(events)
        .expect("append large activity page");

    let page = fixture
        .store
        .console_query()
        .activity(None, 500, None)
        .expect("byte-bounded activity page");
    assert!(page.items.len() < 100);
    assert!(page.next_cursor.is_some());
    assert!(serde_json::to_vec(&page).expect("serialize page").len() < 1024 * 1024);
}

#[test]
fn backup_work_query_returns_target_context_and_only_measured_progress() {
    let fixture = Fixture::new();
    fixture.seed_inventory();
    let materialized = fixture
        .store
        .runtime()
        .materialize_occurrence(OccurrenceMaterializationSpec::due(
            fixture.revision_id.clone(),
            OccurrenceOrigin::Scheduled,
            200,
            200,
        ))
        .expect("materialize backup occurrence");
    let work_item_id = WorkItemId::from_stable_name(
        Uuid::NAMESPACE_URL,
        format!(
            "backup-work:{}:{}",
            materialized.occurrence_id(),
            fixture.target_id
        )
        .as_bytes(),
    );

    let queued = fixture
        .store
        .console_query()
        .work_item(work_item_id.as_str())
        .expect("query queued backup work");
    let subject = queued.subject.expect("backup subject");
    assert_eq!(subject.plan_id.as_deref(), Some(fixture.plan_id.as_str()));
    assert_eq!(subject.plan_name.as_deref(), Some("Orders nightly"));
    assert_eq!(
        subject.target_id.as_deref(),
        Some(fixture.target_id.as_str())
    );
    assert_eq!(subject.target_name.as_deref(), Some("orders"));
    assert_eq!(subject.engine.as_deref(), Some("postgresql"));
    let queued_progress = queued.progress.expect("queued backup phase");
    assert_eq!(queued_progress.phase, "queued");
    assert_eq!(queued_progress.completed_units, None);
    assert_eq!(queued_progress.total_units, None);
    assert_eq!(queued_progress.unit, "bytes");
    assert_eq!(queued.created_at_ms, 200);
    assert_eq!(queued.updated_at_ms, 200);

    let direct = dbbackup_catalog::rusqlite::Connection::open(&fixture.db_path)
        .expect("open progress fixture");
    direct
        .execute(
            "UPDATE backup_runs
             SET state = 'succeeded', plaintext_bytes = 4096,
                 started_at_ms = 210, completed_at_ms = 220, updated_at_ms = 220
             WHERE id = (SELECT backup_run_id FROM work_items WHERE id = ?1)",
            [work_item_id.as_str()],
        )
        .expect("record measured backup bytes");
    direct
        .execute(
            "UPDATE work_items
             SET state = 'succeeded', terminal_at_ms = 220, updated_at_ms = 220
             WHERE id = ?1",
            [work_item_id.as_str()],
        )
        .expect("complete backup work");
    let completed = fixture
        .store
        .console_query()
        .work_item(work_item_id.as_str())
        .expect("query completed backup work");
    let completed_progress = completed.progress.expect("completed backup progress");
    assert_eq!(completed_progress.phase, "succeeded");
    assert_eq!(completed_progress.completed_units, Some(4096));
    assert_eq!(completed_progress.total_units, Some(4096));
    assert_eq!(completed_progress.updated_at_ms, Some(220));
}

struct Fixture {
    _temp: TempDir,
    db_path: std::path::PathBuf,
    store: OperationalStore,
    connection_id: ConnectionProfileId,
    target_id: DatabaseTargetId,
    destination_id: DestinationId,
    plan_id: BackupPlanId,
    revision_id: PlanRevisionId,
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
            destination_id: DestinationId::new(),
            plan_id: BackupPlanId::new(),
            revision_id: PlanRevisionId::new(),
        }
    }

    fn seed_inventory(&self) {
        let configuration = self.store.configuration();
        configuration
            .insert_connection(ConnectionProfileSpec::tcp(
                self.connection_id.clone(),
                "Primary PostgreSQL",
                "postgresql",
                "db.internal",
                5432,
                100,
            ))
            .expect("insert connection");
        configuration
            .insert_target(DatabaseTargetSpec::new(
                self.target_id.clone(),
                self.connection_id.clone(),
                "oid:42",
                "orders",
                "orders",
                "source-fingerprint",
                110,
            ))
            .expect("insert target");
        configuration
            .insert_destination(DestinationSpec::local(
                self.destination_id.clone(),
                "Local Vault",
                r#"{"path":"D:\\backup","password":"must-not-leak"}"#,
                120,
            ))
            .expect("insert destination");
        configuration
            .insert_plan_draft(PlanDraftSpec::manual(
                self.plan_id.clone(),
                self.revision_id.clone(),
                "Orders nightly",
                1,
                "a".repeat(64),
                130,
            ))
            .expect("insert plan");
        configuration
            .add_revision_target(&self.revision_id, &self.target_id, 131)
            .expect("add plan target");
        configuration
            .add_revision_destination(
                &self.revision_id,
                &self.destination_id,
                DestinationRequirement::Required,
                132,
            )
            .expect("add plan destination");
        configuration
            .publish_revision(&self.revision_id, 140)
            .expect("publish plan");
    }
}
