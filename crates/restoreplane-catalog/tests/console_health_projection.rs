use std::str::FromStr;

use restoreplane_catalog::{
    ConnectionProfileSpec, DatabaseObjectDiscoverySpec, DatabaseTargetSpec, DestinationSpec,
    DiscoveredDatabaseObjectSpec, ExplicitBackupScopeSpec, OccurrenceMaterializationSpec,
    OperationalStore, PlanDraftSpec, SchedulerCursorAdvanceSpec,
};
use restoreplane_domain::{
    BackupPlanId, BackupScopeId, ConnectionProfileId, DatabaseTargetId, DestinationId,
    DestinationRequirement, OccurrenceOrigin, OperationRunId, PlanRevisionId,
};
use rusqlite::{params, Connection};
use tempfile::TempDir;

#[test]
fn scope_health_is_a_database_projection_of_inventory_policy_and_recovery_evidence() {
    let fixture = Fixture::new();
    fixture.seed();

    let scopes = fixture
        .store
        .console_query()
        .backup_scopes_in_workspace(
            restoreplane_catalog::DEFAULT_WORKSPACE_ID,
            Some(fixture.target_id.as_str()),
            10_000,
        )
        .expect("scope health projection");
    let scope = scopes
        .iter()
        .find(|scope| scope.id == fixture.scope_id.as_str())
        .expect("explicit scope");

    assert_eq!(scope.database_name, "orders");
    assert_eq!(scope.connection_name, "Primary PostgreSQL");
    assert_eq!(scope.engine, "postgresql");
    assert_eq!(scope.selected_object_count, Some(2));
    assert_eq!(scope.eligible_object_count, Some(2));
    assert_eq!(scope.new_object_count, Some(1));
    assert_eq!(scope.missing_object_count, Some(1));
    assert_eq!(scope.changed_object_count, Some(1));
    assert_eq!(scope.drift_state, "drifted");
    assert!(!scope.source_drift);
    assert_eq!(scope.plan_count, 1);
    assert_eq!(scope.retention_policy_count, 1);
    assert_eq!(scope.rpo_ms, Some(3_600_000));
    assert_eq!(scope.latest_backup_state.as_deref(), Some("succeeded"));
    assert_eq!(
        scope.latest_recovery_point_id.as_deref(),
        Some("20000000-0000-4000-8000-000000000001")
    );
    assert_eq!(scope.latest_success_at_ms, Some(9_000));
    assert_eq!(scope.artifact_completeness, "complete");
    assert_eq!(scope.replica_durability, "healthy");
    assert_eq!(scope.protection_compliance, "compliant");
    assert_eq!(scope.restore_eligibility, "eligible");
    assert_eq!(scope.latest_verified_level, Some(2));
    assert_eq!(scope.recovery_evidence_at_ms, Some(9_100));
}

#[test]
fn scope_health_page_applies_filters_before_the_created_id_cursor() {
    let fixture = Fixture::new();
    fixture.seed();
    let query = fixture.store.console_query();

    let filtered = query
        .backup_scopes_page_in_workspace(
            restoreplane_catalog::DEFAULT_WORKSPACE_ID,
            Some(fixture.target_id.as_str()),
            None,
            1,
            Some("active"),
            Some("core tables"),
            10_000,
        )
        .expect("filtered scope page");
    assert_eq!(filtered.total_count, 1);
    assert_eq!(filtered.items[0].id, fixture.scope_id.as_str());
    assert_eq!(filtered.next_cursor, None);

    let first = query
        .backup_scopes_page_in_workspace(
            restoreplane_catalog::DEFAULT_WORKSPACE_ID,
            Some(fixture.target_id.as_str()),
            None,
            1,
            None,
            None,
            10_000,
        )
        .expect("first scope page");
    let second = query
        .backup_scopes_page_in_workspace(
            restoreplane_catalog::DEFAULT_WORKSPACE_ID,
            Some(fixture.target_id.as_str()),
            first.next_cursor.as_deref(),
            1,
            None,
            None,
            10_000,
        )
        .expect("second scope page");
    assert_eq!(first.total_count, 2);
    assert_eq!(second.total_count, 2);
    assert_ne!(first.items[0].id, second.items[0].id);
    assert_eq!(second.next_cursor, None);
}

#[test]
fn database_target_rolls_up_live_partial_scope_recovery_instead_of_stale_projection_rows() {
    let fixture = Fixture::new();
    fixture.seed();

    let page = fixture
        .store
        .console_query()
        .database_targets_in_workspace_at(
            restoreplane_catalog::DEFAULT_WORKSPACE_ID,
            None,
            100,
            Some(fixture.connection_id.as_str()),
            None,
            None,
            10_000,
        )
        .expect("connection target health projection");
    let target = &page.items[0];

    assert_eq!(target.protection_scope, "partial");
    assert_eq!(
        target.latest_recovery_point_id.as_deref(),
        Some("20000000-0000-4000-8000-000000000001")
    );
    assert_eq!(target.latest_success_at_ms, Some(9_000));
    assert_eq!(target.artifact_completeness, "complete");
    assert_eq!(target.replica_durability, "healthy");
    assert_eq!(target.protection_compliance, "partial");
    assert!(!target.rpo_violation);
    assert!(!target.verification_overdue);
    assert_eq!(target.verification_state, "current");
    assert_eq!(target.evidence_at_ms, Some(9_100));
}

#[test]
fn weekly_verification_becomes_overdue_only_after_its_durable_policy_window() {
    let fixture = Fixture::new();
    fixture.seed_with_verification_policy(r#"{"requestedLevel":3,"schedule":"weekly"}"#);

    let week_ms = 7 * 24 * 60 * 60 * 1_000;
    let before_due = fixture
        .store
        .console_query()
        .backup_scopes_in_workspace(
            restoreplane_catalog::DEFAULT_WORKSPACE_ID,
            Some(fixture.target_id.as_str()),
            week_ms - 1,
        )
        .expect("scope health before weekly deadline");
    assert!(!before_due[0].verification_overdue);

    let after_due_at = week_ms + 10_000;
    let after_due = fixture
        .store
        .console_query()
        .backup_scopes_in_workspace(
            restoreplane_catalog::DEFAULT_WORKSPACE_ID,
            Some(fixture.target_id.as_str()),
            after_due_at,
        )
        .expect("scope health after weekly deadline");
    let scope = after_due
        .iter()
        .find(|scope| scope.id == fixture.scope_id.as_str())
        .expect("explicit scope");
    assert_eq!(scope.verification_requested_level, Some(3));
    assert_eq!(scope.verification_state, "pending");
    assert!(scope.verification_overdue);

    let targets = fixture
        .store
        .console_query()
        .database_targets_in_workspace_at(
            restoreplane_catalog::DEFAULT_WORKSPACE_ID,
            None,
            100,
            Some(fixture.connection_id.as_str()),
            None,
            None,
            after_due_at,
        )
        .expect("target health after weekly deadline");
    assert!(targets.items[0].verification_overdue);
}

#[test]
fn weekly_verification_does_not_combine_an_unproved_level_with_a_fresh_summary_time() {
    let fixture = Fixture::new();
    fixture.seed_with_verification_policy(r#"{"requestedLevel":3,"schedule":"weekly"}"#);
    let week_ms = 7 * 24 * 60 * 60 * 1_000;
    let now_ms = week_ms + 10_000;
    Connection::open(&fixture.db_path)
        .expect("open projection writer")
        .execute(
            "UPDATE catalog_snapshot_projection
             SET highest_verified_level = 3, last_verified_at_ms = ?1",
            [now_ms - 1],
        )
        .expect("overstate independent summary axes");

    let scopes = fixture
        .store
        .console_query()
        .backup_scopes_in_workspace(
            restoreplane_catalog::DEFAULT_WORKSPACE_ID,
            Some(fixture.target_id.as_str()),
            now_ms,
        )
        .expect("scope health");

    let scope = scopes
        .iter()
        .find(|scope| scope.id == fixture.scope_id.as_str())
        .expect("explicit scope");
    assert_eq!(scope.verification_state, "pending");
    assert!(scope.verification_overdue);
}

#[test]
fn every_run_verification_requires_the_highest_level_across_active_policies() {
    let fixture = Fixture::new();
    fixture.seed();
    fixture.seed_additional_verification_policy(r#"{"requestedLevel":3,"schedule":"every_run"}"#);

    let scopes = fixture
        .store
        .console_query()
        .backup_scopes_in_workspace(
            restoreplane_catalog::DEFAULT_WORKSPACE_ID,
            Some(fixture.target_id.as_str()),
            10_000,
        )
        .expect("scope health");
    let scope = scopes
        .iter()
        .find(|scope| scope.id == fixture.scope_id.as_str())
        .expect("explicit scope");

    assert_eq!(scope.verification_requested_level, Some(3));
    assert_eq!(scope.verification_state, "pending");
    assert!(scope.verification_overdue);
}

#[test]
fn connection_health_discards_a_scope_rollup_that_exceeds_the_hard_cap() {
    let fixture = Fixture::new();
    fixture.seed();
    fixture.seed_scope_health_overflow();

    let page = fixture
        .store
        .console_query()
        .database_targets_in_workspace_at(
            restoreplane_catalog::DEFAULT_WORKSPACE_ID,
            None,
            100,
            Some(fixture.connection_id.as_str()),
            None,
            None,
            10_000,
        )
        .expect("bounded connection health");
    let target = &page.items[0];

    assert_eq!(target.protection_scope, "unknown");
    assert_eq!(target.protection_compliance, "unknown");
    assert_eq!(target.verification_state, "unknown");
    assert!(!target.verification_overdue);
}

#[test]
fn connection_health_scope_cap_only_counts_current_connection_page_ids() {
    let fixture = Fixture::new();
    fixture.seed();
    let overflow_connection_id =
        ConnectionProfileId::from_str("ffffffff-ffff-4fff-bfff-ffffffffffff")
            .expect("overflow connection id");
    let overflow_target_id = DatabaseTargetId::from_str("eeeeeeee-eeee-4eee-beee-eeeeeeeeeeee")
        .expect("overflow target id");
    let configuration = fixture.store.configuration();
    configuration
        .insert_connection(ConnectionProfileSpec::tcp(
            overflow_connection_id.clone(),
            "Overflow PostgreSQL",
            "postgresql",
            "overflow.internal",
            5432,
            600,
        ))
        .expect("overflow connection");
    configuration
        .insert_target(DatabaseTargetSpec::new(
            overflow_target_id.clone(),
            overflow_connection_id,
            "database:overflow",
            "overflow",
            "overflow",
            "overflow-fingerprint",
            601,
        ))
        .expect("overflow target");
    fixture.seed_extra_scopes(&overflow_target_id, 1_000, "other");

    let page = fixture
        .store
        .console_query()
        .database_targets_in_workspace_at(
            restoreplane_catalog::DEFAULT_WORKSPACE_ID,
            None,
            1,
            Some(fixture.connection_id.as_str()),
            None,
            None,
            10_000,
        )
        .expect("first connection page");

    assert_eq!(page.items[0].connection_id, fixture.connection_id.as_str());
    assert_eq!(page.items[0].verification_state, "current");
}

#[test]
fn scheduler_and_retention_health_report_only_durable_store_evidence() {
    let fixture = Fixture::new();
    fixture.seed();
    let query = fixture.store.console_query();

    let scheduler = query.scheduler_health().expect("scheduler health");
    assert_eq!(scheduler.state, "observed");
    assert_eq!(scheduler.configured_plan_count, 1);
    assert_eq!(scheduler.observed_cursor_count, 1);
    assert_eq!(scheduler.last_tick_at_ms, Some(9_500));
    assert_eq!(scheduler.last_materialized_for_ms, Some(9_000));
    assert_eq!(scheduler.error_plan_count, 0);
    assert_eq!(scheduler.last_error_code, None);

    let before_scan = query
        .retention_health()
        .expect("retention before first scan");
    assert_eq!(before_scan.state, "awaiting_first_scan");
    assert_eq!(before_scan.configured_policy_count, 1);
    assert_eq!(before_scan.last_scan_at_ms, None);

    fixture.seed_retention_runtime_evidence();
    let retention = query.retention_health().expect("retention health");
    assert_eq!(retention.state, "observed");
    assert_eq!(retention.last_scan_at_ms, Some(9_600));
    assert_eq!(retention.active_tombstone_count, 1);
    assert_eq!(retention.queued_delete_count, 0);
}

#[test]
fn retention_health_prioritizes_failed_delete_evidence_over_an_observed_scan() {
    let fixture = Fixture::new();
    fixture.seed();
    fixture.seed_retention_runtime_evidence();
    fixture.seed_failed_retention_delete();

    let retention = fixture
        .store
        .console_query()
        .retention_health()
        .expect("retention health");

    assert_eq!(retention.state, "attention");
    assert_eq!(retention.failed_operation_count, 1);
    assert_eq!(retention.last_error_code.as_deref(), Some("DELETE_DENIED"));
    assert_eq!(retention.last_error_at_ms, Some(9_700));
    assert_eq!(retention.evidence_at_ms, Some(9_700));
}

#[test]
fn new_scope_objects_exclude_tables_deliberately_omitted_at_the_baseline() {
    let fixture = Fixture::new();
    fixture.seed_scope_with_deliberately_excluded_baseline_table();

    let scopes = fixture
        .store
        .console_query()
        .backup_scopes_in_workspace(
            restoreplane_catalog::DEFAULT_WORKSPACE_ID,
            Some(fixture.target_id.as_str()),
            1_000,
        )
        .expect("scope health projection");
    let scope = scopes
        .iter()
        .find(|scope| scope.id == fixture.scope_id.as_str())
        .expect("explicit scope");

    assert_eq!(scope.selected_object_count, Some(2));
    assert_eq!(scope.eligible_object_count, Some(2));
    assert_eq!(scope.new_object_count, Some(1));
    assert_eq!(scope.missing_object_count, Some(2));
    assert_eq!(scope.changed_object_count, Some(0));
    assert_eq!(scope.drift_state, "drifted");
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
    first_discovery_id: OperationRunId,
    second_discovery_id: OperationRunId,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().expect("temporary directory");
        let db_path = temp.path().join("operational.db");
        let store = OperationalStore::open(db_path.clone()).expect("operational store");
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
            first_discovery_id: OperationRunId::new(),
            second_discovery_id: OperationRunId::new(),
        }
    }

    fn seed(&self) {
        self.seed_with_verification_policy(r#"{"requestedLevel":2}"#);
    }

    fn seed_with_verification_policy(&self, verification_policy_json: &str) {
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
            .expect("connection");
        configuration
            .insert_target(DatabaseTargetSpec::new(
                self.target_id.clone(),
                self.connection_id.clone(),
                "database:orders",
                "orders",
                "orders",
                "database-fingerprint",
                110,
            ))
            .expect("target");
        configuration
            .insert_destination(DestinationSpec::local(
                self.destination_id.clone(),
                "Vault",
                r#"{"path":"D:\\backup"}"#,
                120,
            ))
            .expect("destination");
        configuration
            .insert_plan_draft(
                PlanDraftSpec::cron(
                    self.plan_id.clone(),
                    self.revision_id.clone(),
                    "Hourly selected tables",
                    1,
                    "0 * * * *",
                    "UTC",
                    "a".repeat(64),
                    130,
                )
                .with_policies(
                    Some(3_600_000),
                    r#"{"daily":7,"weekly":4,"monthly":3}"#,
                    verification_policy_json,
                    "{}",
                ),
            )
            .expect("plan draft");

        self.seed_discovery_operation(&self.first_discovery_id, 200);
        self.store
            .backup_scopes()
            .record_object_discovery(DatabaseObjectDiscoverySpec::new(
                self.first_discovery_id.clone(),
                self.target_id.clone(),
                "b".repeat(64),
                vec![
                    DiscoveredDatabaseObjectSpec::eligible_table(
                        "opaque:orders",
                        "public",
                        "orders",
                        "c".repeat(64),
                    ),
                    DiscoveredDatabaseObjectSpec::eligible_table(
                        "opaque:invoices",
                        "public",
                        "invoices",
                        "d".repeat(64),
                    ),
                ],
                210,
            ))
            .expect("first discovery");
        self.finish_discovery(&self.first_discovery_id);
        self.store
            .backup_scopes()
            .create_explicit(ExplicitBackupScopeSpec::new(
                self.scope_id.clone(),
                self.target_id.clone(),
                "Core tables",
                self.first_discovery_id.clone(),
                vec!["opaque:orders".to_owned(), "opaque:invoices".to_owned()],
                220,
            ))
            .expect("explicit scope");
        configuration
            .add_revision_target(&self.revision_id, &self.target_id, 230)
            .expect("target binding");
        self.store
            .backup_scopes()
            .bind_revision_scope(&self.revision_id, &self.scope_id, 231)
            .expect("scope binding");
        configuration
            .add_revision_destination(
                &self.revision_id,
                &self.destination_id,
                DestinationRequirement::Required,
                232,
            )
            .expect("destination binding");
        configuration
            .publish_revision(&self.revision_id, 240)
            .expect("publish plan");

        self.seed_discovery_operation(&self.second_discovery_id, 300);
        self.store
            .backup_scopes()
            .record_object_discovery(DatabaseObjectDiscoverySpec::new(
                self.second_discovery_id.clone(),
                self.target_id.clone(),
                "e".repeat(64),
                vec![
                    DiscoveredDatabaseObjectSpec::eligible_table(
                        "opaque:orders",
                        "public",
                        "orders",
                        "f".repeat(64),
                    ),
                    DiscoveredDatabaseObjectSpec::eligible_table(
                        "opaque:payments",
                        "public",
                        "payments",
                        "1".repeat(64),
                    ),
                ],
                310,
            ))
            .expect("latest discovery");
        self.finish_discovery(&self.second_discovery_id);

        let occurrence = self
            .store
            .runtime()
            .materialize_occurrence(OccurrenceMaterializationSpec::due(
                self.revision_id.clone(),
                OccurrenceOrigin::Scheduled,
                8_000,
                8_000,
            ))
            .expect("backup occurrence");
        self.seed_recovery(occurrence.occurrence_id().as_str());
        self.store
            .scheduler()
            .advance_cursor(SchedulerCursorAdvanceSpec::success(
                self.revision_id.clone(),
                9_500,
                Some(9_000),
            ))
            .expect("scheduler cursor");
    }

    fn seed_additional_verification_policy(&self, verification_policy_json: &str) {
        let plan_id = BackupPlanId::new();
        let revision_id = PlanRevisionId::new();
        let configuration = self.store.configuration();
        configuration
            .insert_plan_draft(
                PlanDraftSpec::cron(
                    plan_id,
                    revision_id.clone(),
                    "Higher verification policy",
                    1,
                    "30 * * * *",
                    "UTC",
                    "9".repeat(64),
                    500,
                )
                .with_policies(
                    Some(3_600_000),
                    r#"{"daily":7}"#,
                    verification_policy_json,
                    "{}",
                ),
            )
            .expect("additional plan draft");
        configuration
            .add_revision_target(&revision_id, &self.target_id, 501)
            .expect("additional target binding");
        self.store
            .backup_scopes()
            .bind_revision_scope(&revision_id, &self.scope_id, 502)
            .expect("additional scope binding");
        configuration
            .add_revision_destination(
                &revision_id,
                &self.destination_id,
                DestinationRequirement::Required,
                503,
            )
            .expect("additional destination binding");
        configuration
            .publish_revision(&revision_id, 504)
            .expect("publish additional plan");
    }

    fn seed_scope_health_overflow(&self) {
        self.seed_extra_scopes(&self.target_id, 999, "overflow");
    }

    fn seed_extra_scopes(&self, target_id: &DatabaseTargetId, count: usize, prefix: &str) {
        let mut connection = Connection::open(&self.db_path).expect("scope overflow writer");
        let transaction = connection
            .transaction()
            .expect("scope overflow transaction");
        for index in 0..count {
            let id = format!("{prefix}-scope-{index:04}");
            let display_name = format!("{prefix} scope {index:04}");
            let selector =
                format!(r#"{{"mode":"explicit_tables","objects":["{prefix}:{index:04}"]}}"#);
            transaction
                .execute(
                    "INSERT INTO backup_scopes(
                         id, database_target_id, display_name, normalized_name,
                         mode, state, selector_json, selector_hash_blake3,
                         source_fingerprint, database_source_fingerprint,
                         resolved_object_count, last_resolved_at_ms,
                         created_at_ms, updated_at_ms
                     ) VALUES (
                         ?1, ?2, ?3, lower(?3), 'explicit_tables', 'active', ?4, ?5,
                         'database-fingerprint', 'database-fingerprint',
                         1, ?6, ?6, ?6
                     )",
                    params![
                        id,
                        target_id.as_str(),
                        display_name,
                        selector,
                        "8".repeat(64),
                        1_000 + i64::try_from(index).expect("scope index fits i64"),
                    ],
                )
                .expect("insert overflow scope");
        }
        transaction.commit().expect("commit scope overflow");
    }

    fn seed_scope_with_deliberately_excluded_baseline_table(&self) {
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
            .expect("connection");
        configuration
            .insert_target(DatabaseTargetSpec::new(
                self.target_id.clone(),
                self.connection_id.clone(),
                "database:orders",
                "orders",
                "orders",
                "database-fingerprint",
                110,
            ))
            .expect("target");

        self.seed_discovery_operation(&self.first_discovery_id, 200);
        self.store
            .backup_scopes()
            .record_object_discovery(DatabaseObjectDiscoverySpec::new(
                self.first_discovery_id.clone(),
                self.target_id.clone(),
                "b".repeat(64),
                vec![
                    DiscoveredDatabaseObjectSpec::eligible_table(
                        "opaque:orders",
                        "public",
                        "orders",
                        "c".repeat(64),
                    ),
                    DiscoveredDatabaseObjectSpec::eligible_table(
                        "opaque:invoices",
                        "public",
                        "invoices",
                        "d".repeat(64),
                    ),
                    DiscoveredDatabaseObjectSpec::eligible_table(
                        "opaque:audit",
                        "public",
                        "audit",
                        "e".repeat(64),
                    ),
                ],
                210,
            ))
            .expect("baseline discovery");
        self.finish_discovery(&self.first_discovery_id);
        self.store
            .backup_scopes()
            .create_explicit(ExplicitBackupScopeSpec::new(
                self.scope_id.clone(),
                self.target_id.clone(),
                "Core tables",
                self.first_discovery_id.clone(),
                vec!["opaque:orders".to_owned(), "opaque:invoices".to_owned()],
                220,
            ))
            .expect("explicit scope");

        self.seed_discovery_operation(&self.second_discovery_id, 300);
        self.store
            .backup_scopes()
            .record_object_discovery(DatabaseObjectDiscoverySpec::new(
                self.second_discovery_id.clone(),
                self.target_id.clone(),
                "f".repeat(64),
                vec![
                    DiscoveredDatabaseObjectSpec::eligible_table(
                        "opaque:audit",
                        "public",
                        "audit",
                        "e".repeat(64),
                    ),
                    DiscoveredDatabaseObjectSpec::eligible_table(
                        "opaque:payments",
                        "public",
                        "payments",
                        "1".repeat(64),
                    ),
                ],
                310,
            ))
            .expect("latest discovery");
        self.finish_discovery(&self.second_discovery_id);
    }

    fn finish_discovery(&self, operation_id: &OperationRunId) {
        Connection::open(&self.db_path)
            .unwrap()
            .execute(
                "UPDATE operation_runs SET state='succeeded' WHERE id=?1",
                [operation_id.as_str()],
            )
            .unwrap();
    }

    fn seed_discovery_operation(&self, operation_id: &OperationRunId, at_ms: i64) {
        let connection = Connection::open(&self.db_path).expect("discovery writer");
        connection
            .execute(
                "INSERT INTO operation_runs(
                     id, operation_kind, subject_type, subject_id, request_json,
                     state, created_at_ms, updated_at_ms
                 ) VALUES (?1, 'connection_discovery', 'database_target', ?2,
                           '{}', 'running', ?3, ?3)",
                params![operation_id.as_str(), self.target_id.as_str(), at_ms],
            )
            .expect("discovery operation");
    }

    fn seed_recovery(&self, occurrence_id: &str) {
        let connection = Connection::open(&self.db_path).expect("recovery writer");
        let (backup_run_id, source_fingerprint): (String, String) = connection
            .query_row(
                "SELECT run.id, run.source_fingerprint
                 FROM backup_runs AS run
                 JOIN backup_run_scopes AS scope ON scope.backup_run_id = run.id
                 WHERE run.plan_occurrence_id = ?1 AND scope.backup_scope_id = ?2",
                params![occurrence_id, self.scope_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("backup run");
        let recovery_id = "20000000-0000-4000-8000-000000000001";
        connection
            .execute(
                "INSERT INTO catalog_recovery_points(
                     id, origin_instance_id, origin_occurrence_id, origin_backup_run_id,
                     source_target_identity, source_display_name, engine, state,
                     consistency_grade, committed_at_ms, manifest_schema_version,
                     set_manifest_checksum_blake3, signing_key_id, signature_status,
                     required_part_count, present_part_count, engine_metadata_json,
                     first_ingested_at_ms, last_ingested_at_ms
                 ) VALUES (?1, 'instance-test', ?2, ?3, ?4, 'Core tables', 'postgresql',
                           'complete', 'transactional', 9000, 2, ?5, 'key-test', 'valid',
                           1, 1, '{}', 9100, 9100)",
                params![
                    recovery_id,
                    occurrence_id,
                    backup_run_id,
                    source_fingerprint,
                    "2".repeat(64)
                ],
            )
            .expect("recovery point");
        connection
            .execute(
                "INSERT INTO catalog_snapshot_projection(
                     recovery_point_id, source_target_identity, source_display_name,
                     engine, committed_at_ms, artifact_completeness, replica_durability,
                     protection_compliance, restore_eligibility, highest_verified_level,
                     last_verified_at_ms, search_text, projection_version, projected_at_ms
                 ) VALUES (?1, ?2, 'Core tables', 'postgresql', 9000, 'complete', 'healthy',
                           'compliant', 'eligible', 2, 9050, 'core tables', 1, 9100)",
                params![recovery_id, source_fingerprint],
            )
            .expect("snapshot projection");
        connection
            .execute(
                "INSERT INTO catalog_verification_projection(
                     id, recovery_point_id, origin_verification_run_id,
                     verify_level, result, verified_at_ms, tool_name, tool_version,
                     evidence_checksum_blake3, evidence_locator, evidence_json, ingested_at_ms
                 ) VALUES (
                     '20000000-0000-4000-8000-000000000002', ?1, NULL,
                     2, 'passed', 9050, NULL, NULL,
                     ?2, NULL, '{}', 9100
                 )",
                params![recovery_id, blake3::hash(b"{}").to_hex().to_string()],
            )
            .expect("verification projection");
        connection
            .execute(
                "UPDATE backup_runs
                 SET state = 'succeeded', recovery_point_id = ?2, started_at_ms = 8500,
                     completed_at_ms = 9000, updated_at_ms = 9000
                 WHERE id = ?1",
                params![backup_run_id, recovery_id],
            )
            .expect("complete backup run");
    }

    fn seed_retention_runtime_evidence(&self) {
        let connection = Connection::open(&self.db_path).expect("retention writer");
        connection
            .execute(
                "INSERT INTO app_meta(key, value_text, updated_at_ms)
                 VALUES ('retention.runtime.cursor.v1',
                         '{\"schema_version\":1,\"committed_at_ms\":9000,\"recovery_point_id\":\"20000000-0000-4000-8000-000000000001\",\"destination_fingerprint\":\"local:test\"}',
                         9600)",
                [],
            )
            .expect("retention cursor");
        connection
            .execute(
                "INSERT INTO operation_runs(
                     id, operation_kind, subject_type, subject_id, request_json,
                     state, created_at_ms, updated_at_ms
                 ) VALUES ('30000000-0000-4000-8000-000000000001', 'retention_delete',
                           'artifact_replica', '20000000-0000-4000-8000-000000000001',
                           '{\"phase\":\"tombstoned\"}', 'created', 9500, 9500)",
                [],
            )
            .expect("retention tombstone");
    }

    fn seed_failed_retention_delete(&self) {
        let connection = Connection::open(&self.db_path).expect("retention failure writer");
        connection
            .execute(
                "INSERT INTO operation_runs(
                     id, operation_kind, subject_type, subject_id, request_json,
                     state, error_code, created_at_ms, updated_at_ms
                 ) VALUES ('30000000-0000-4000-8000-000000000002', 'retention_delete',
                           'artifact_replica', '20000000-0000-4000-8000-000000000001',
                           '{}', 'failed', 'DELETE_DENIED', 9700, 9700)",
                [],
            )
            .expect("failed retention delete");
    }
}
