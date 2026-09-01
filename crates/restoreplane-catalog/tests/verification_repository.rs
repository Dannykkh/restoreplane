use restoreplane_catalog::{
    rusqlite::{params, Connection, OptionalExtension},
    BackupWorkSpec, CancelRequestOutcome, CertificationCapability, ClaimRequest,
    ConnectionProfileSpec, DatabaseTargetSpec, DestinationSpec, EngineCertificationSpec,
    NotificationChannelSpec, NotificationRuleSpec, OccurrenceSpec, OperationalStore, PlanDraftSpec,
    PostBackupVerificationDisposition, UnsupportedVerificationSpec, VerificationWorkSpec,
};
use restoreplane_core::ProviderCapability;
use restoreplane_domain::{
    BackupPlanId, BackupRunId, ConnectionProfileId, DatabaseTargetId, DestinationId,
    DestinationRequirement, OccurrenceIdentity, OccurrenceOrigin, PlanRevisionId, RecoveryPointId,
    VerificationRunId, WorkItemId, WorkItemKind,
};
use tempfile::TempDir;
use uuid::Uuid;

#[test]
fn l2_post_backup_verification_is_durable_and_idempotent() {
    let fixture = Fixture::new();
    let run_id = VerificationRunId::new();
    let work_id = WorkItemId::new();
    let spec = VerificationWorkSpec::post_backup(
        run_id.clone(),
        work_id.clone(),
        fixture.recovery_point_id.clone(),
        2,
        1_100,
    );

    let first = fixture
        .store
        .verification()
        .enqueue(spec.clone())
        .expect("enqueue verification");
    let replay = fixture
        .store
        .verification()
        .enqueue(spec)
        .expect("idempotent replay");

    assert!(first.created());
    assert!(!replay.created());
    assert_eq!(first.verification_run_id(), &run_id);
    assert_eq!(first.work_item_id(), Some(&work_id));
    assert!(verification_source_replica(&fixture.db_path, &run_id).is_some());
    assert_eq!(
        fixture.verification_row(&run_id),
        (2, "post_backup".to_owned(), "queued".to_owned(), None)
    );
    assert_eq!(
        fixture.work_row(&work_id),
        (
            "verification".to_owned(),
            run_id.to_string(),
            "queued".to_owned()
        )
    );
}

#[test]
fn unsupported_level_is_explicit_and_never_enqueues_a_lower_level() {
    let fixture = Fixture::new();
    let run_id = VerificationRunId::new();

    let outcome = fixture
        .store
        .verification()
        .record_unsupported(UnsupportedVerificationSpec::post_backup(
            run_id.clone(),
            fixture.recovery_point_id.clone(),
            3,
            "VERIFY_LEVEL_UNSUPPORTED",
            1_100,
        ))
        .expect("record unsupported");

    assert!(outcome.created());
    assert_eq!(outcome.work_item_id(), None);
    assert_eq!(
        fixture.verification_row(&run_id),
        (
            3,
            "post_backup".to_owned(),
            "unsupported".to_owned(),
            Some("VERIFY_LEVEL_UNSUPPORTED".to_owned())
        )
    );
    assert_eq!(fixture.verification_count_at_level(1), 0);
    assert_eq!(fixture.verification_count_at_level(2), 0);
    assert_eq!(fixture.work_count(), 0);
    assert_eq!(
        fixture.verification_projection(&run_id),
        Some((3, "unsupported".to_owned(), 1_100)),
        "unsupported terminal truth remains queryable without a worker"
    );
}

#[test]
fn cancelling_queued_verification_projects_terminal_history_atomically() {
    let fixture = Fixture::new();
    let run_id = VerificationRunId::new();
    let work_id = WorkItemId::new();
    fixture
        .store
        .verification()
        .enqueue(VerificationWorkSpec::post_backup(
            run_id.clone(),
            work_id.clone(),
            fixture.recovery_point_id.clone(),
            2,
            1_100,
        ))
        .expect("enqueue verification");

    let outcome = fixture
        .store
        .runtime()
        .request_cancel(&work_id, 0, 1_101)
        .expect("cancel queued verification");

    assert_eq!(outcome, CancelRequestOutcome::Requested);
    assert_eq!(
        fixture.verification_projection(&run_id),
        Some((2, "cancelled".to_owned(), 1_101))
    );
}

#[test]
fn l3_without_restore_run_binding_is_rejected_without_l1_downgrade() {
    let fixture = Fixture::new();
    let error = fixture
        .store
        .verification()
        .enqueue(VerificationWorkSpec::post_backup(
            VerificationRunId::new(),
            WorkItemId::new(),
            fixture.recovery_point_id.clone(),
            3,
            1_100,
        ))
        .expect_err("L3 must bind a Restore Run");

    assert_eq!(error.code(), "VERIFICATION_RESTORE_RUN_REQUIRED");
    assert_eq!(fixture.verification_count_at_level(1), 0);
    assert_eq!(fixture.verification_count_at_level(2), 0);
    assert_eq!(fixture.verification_count_at_level(3), 0);
    assert_eq!(fixture.work_count(), 0);
}

#[test]
fn post_backup_policy_creates_verification_activity_and_outbox_in_one_schedule() {
    let fixture = ScheduledFixture::new(r#"{"level":"l2"}"#, "verification.queued");

    let first = fixture
        .store
        .verification()
        .schedule_post_backup(&fixture.recovery_point_id, 1_100)
        .expect("schedule post-backup verification");
    let replay = fixture
        .store
        .verification()
        .schedule_post_backup(&fixture.recovery_point_id, 1_100)
        .expect("idempotent schedule replay");

    assert_eq!(
        first.disposition(),
        PostBackupVerificationDisposition::Queued
    );
    assert_eq!(first.requested_level(), Some(2));
    assert!(first.verification_run_id().is_some());
    assert!(first.work_item_id().is_some());
    assert!(verification_source_replica(
        &fixture.db_path,
        first
            .verification_run_id()
            .expect("scheduled verification run")
    )
    .is_some());
    assert_eq!(first.outbox_intents_created(), 1);
    assert_eq!(replay.outbox_intents_created(), 0);
    assert_eq!(fixture.count("verification_runs"), 1);
    assert_eq!(fixture.count("work_items"), 2);
    assert_eq!(fixture.count("activity_events"), 1);
    assert_eq!(fixture.count("notification_outbox"), 1);
}

#[test]
fn post_backup_l3_without_restore_binding_records_unsupported_at_l3_only() {
    let fixture = ScheduledFixture::new(r#"{"level":"l3"}"#, "verification.unsupported");

    let outcome = fixture
        .store
        .verification()
        .schedule_post_backup(&fixture.recovery_point_id, 1_100)
        .expect("record unsupported L3");

    assert_eq!(
        outcome.disposition(),
        PostBackupVerificationDisposition::Unsupported
    );
    assert_eq!(outcome.requested_level(), Some(3));
    assert_eq!(
        outcome.reason_code(),
        Some("VERIFICATION_RESTORE_RUN_REQUIRED")
    );
    assert!(outcome.work_item_id().is_none());
    assert_eq!(
        fixture.verification_state(3),
        Some("unsupported".to_owned())
    );
    assert_eq!(fixture.verification_state(1), None);
    assert_eq!(fixture.verification_state(2), None);
    assert_eq!(fixture.count_verification_work(), 0);
    assert_eq!(fixture.count("notification_outbox"), 1);
}

#[test]
fn mysql_post_backup_l2_is_recorded_unsupported_even_when_legacy_certification_claims_passed() {
    let fixture = ScheduledFixture::new(r#"{"level":"l2"}"#, "verification.unsupported");
    Connection::open(&fixture.db_path)
        .expect("open engine override connection")
        .execute_batch(&format!(
            "UPDATE backup_runs
             SET engine = 'mysql'
             WHERE id = (SELECT origin_backup_run_id FROM catalog_recovery_points WHERE id = '{id}');
             UPDATE catalog_recovery_points SET engine = 'mysql' WHERE id = '{id}';",
            id = fixture.recovery_point_id.as_str()
        ))
        .expect("represent legacy mysql backup and recovery evidence");
    install_mysql_l2_certification(&fixture.store);

    let outcome = fixture
        .store
        .verification()
        .schedule_post_backup(&fixture.recovery_point_id, 1_100)
        .expect("settle unsupported mysql L2");

    assert_eq!(
        outcome.disposition(),
        PostBackupVerificationDisposition::Unsupported
    );
    assert_eq!(outcome.reason_code(), Some("VERIFY_L2_NATIVE_UNSUPPORTED"));
    assert!(outcome.work_item_id().is_none());
    assert_eq!(fixture.count_verification_work(), 0);
}

#[test]
fn kind_filtered_claim_does_not_steal_verification_work() {
    let fixture = ScheduledFixture::new(r#"{"level":"l2"}"#, "verification.queued");
    fixture
        .store
        .verification()
        .schedule_post_backup(&fixture.recovery_point_id, 1_100)
        .expect("schedule verification");

    let runtime = fixture.store.runtime();
    assert!(runtime
        .claim_next(
            ClaimRequest::new("operation-worker", "boot-a", 1_101, 100)
                .for_kind(WorkItemKind::Operation)
        )
        .expect("filtered operation claim")
        .is_none());
    let claim = runtime
        .claim_next(
            ClaimRequest::new("verification-worker", "boot-b", 1_101, 100)
                .for_kind(WorkItemKind::Verification),
        )
        .expect("filtered verification claim")
        .expect("verification work");
    assert_eq!(claim.kind(), WorkItemKind::Verification);
}

struct Fixture {
    _temp: TempDir,
    db_path: std::path::PathBuf,
    store: OperationalStore,
    recovery_point_id: RecoveryPointId,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().expect("temp directory");
        let db_path = temp.path().join("operational.db");
        let store = OperationalStore::open(db_path.clone()).expect("open store");
        let recovery_point_id = RecoveryPointId::new();
        Connection::open(&db_path)
            .expect("open fixture connection")
            .execute(
                "INSERT INTO catalog_recovery_points(
                     id, origin_instance_id, source_target_identity, source_display_name,
                     engine, state, consistency_grade, committed_at_ms,
                     manifest_schema_version, set_manifest_checksum_blake3, signing_key_id,
                     signature_status, required_part_count, present_part_count,
                     engine_metadata_json, first_ingested_at_ms, last_ingested_at_ms
                 ) VALUES (
                     ?1, 'instance-a', 'target:orders', 'orders',
                     'postgresql', 'complete', 'application_consistent', 1000,
                     1, ?2, 'signing-key-01',
                     'valid', 1, 1,
                     '{}', 1000, 1000
                 )",
                params![recovery_point_id.as_str(), "a".repeat(64)],
            )
            .expect("insert complete recovery point");
        seed_required_replica(&db_path, &recovery_point_id);
        Self {
            _temp: temp,
            db_path,
            store,
            recovery_point_id,
        }
    }

    fn verification_row(
        &self,
        run_id: &VerificationRunId,
    ) -> (i64, String, String, Option<String>) {
        Connection::open(&self.db_path)
            .expect("open query connection")
            .query_row(
                "SELECT requested_level, trigger_kind, state, error_code
                 FROM verification_runs WHERE id = ?1",
                [run_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("verification row")
    }

    fn work_row(&self, work_id: &WorkItemId) -> (String, String, String) {
        Connection::open(&self.db_path)
            .expect("open query connection")
            .query_row(
                "SELECT kind, verification_run_id, state
                 FROM work_items WHERE id = ?1",
                [work_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("work row")
    }

    fn verification_count_at_level(&self, level: i64) -> i64 {
        Connection::open(&self.db_path)
            .expect("open query connection")
            .query_row(
                "SELECT COUNT(*) FROM verification_runs WHERE requested_level = ?1",
                [level],
                |row| row.get(0),
            )
            .expect("verification count")
    }

    fn work_count(&self) -> i64 {
        Connection::open(&self.db_path)
            .expect("open query connection")
            .query_row("SELECT COUNT(*) FROM work_items", [], |row| row.get(0))
            .expect("work count")
    }

    fn verification_projection(&self, run_id: &VerificationRunId) -> Option<(i64, String, i64)> {
        Connection::open(&self.db_path)
            .expect("open projection query")
            .query_row(
                "SELECT verify_level, result, verified_at_ms
                 FROM catalog_verification_projection
                 WHERE origin_verification_run_id = ?1",
                [run_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .expect("verification projection query")
    }
}

struct ScheduledFixture {
    _temp: TempDir,
    db_path: std::path::PathBuf,
    store: OperationalStore,
    recovery_point_id: RecoveryPointId,
}

impl ScheduledFixture {
    fn new(policy_json: &str, notification_event: &str) -> Self {
        let temp = TempDir::new().expect("temp directory");
        let db_path = temp.path().join("operational.db");
        let store = OperationalStore::open(db_path.clone()).expect("open store");
        let connection_id = ConnectionProfileId::new();
        let target_id = DatabaseTargetId::new();
        let destination_id = DestinationId::new();
        let plan_id = BackupPlanId::new();
        let revision_id = PlanRevisionId::new();
        let occurrence_id =
            OccurrenceIdentity::scheduled(revision_id.clone(), OccurrenceOrigin::Scheduled, 900)
                .expect("occurrence identity")
                .deterministic_id(Uuid::NAMESPACE_URL);
        let backup_run_id = BackupRunId::new();
        let recovery_point_id = RecoveryPointId::new();

        let configuration = store.configuration();
        configuration
            .insert_connection(ConnectionProfileSpec::tcp(
                connection_id.clone(),
                "primary-pg",
                "postgresql",
                "db.internal",
                5432,
                100,
            ))
            .expect("insert connection");
        configuration
            .insert_target(DatabaseTargetSpec::new(
                target_id.clone(),
                connection_id,
                "oid:100",
                "orders",
                "orders",
                "source-fingerprint",
                110,
            ))
            .expect("insert target");
        configuration
            .insert_destination(DestinationSpec::local(
                destination_id.clone(),
                "local-vault",
                r#"{"path":"D:\\backup"}"#,
                120,
            ))
            .expect("insert destination");
        configuration
            .insert_plan_draft(PlanDraftSpec::manual(
                plan_id.clone(),
                revision_id.clone(),
                "nightly",
                1,
                "a".repeat(64),
                130,
            ))
            .expect("insert plan");
        configuration
            .add_revision_target(&revision_id, &target_id, 131)
            .expect("add target");
        configuration
            .add_revision_destination(
                &revision_id,
                &destination_id,
                DestinationRequirement::Required,
                132,
            )
            .expect("add destination");
        Connection::open(&db_path)
            .expect("open policy connection")
            .execute(
                "UPDATE backup_plan_revisions
                 SET verification_policy_json = ?2
                 WHERE id = ?1",
                params![revision_id.as_str(), policy_json],
            )
            .expect("set verification policy");
        configuration
            .publish_revision(&revision_id, 140)
            .expect("publish plan");

        let runtime = store.runtime();
        runtime
            .upsert_occurrence(OccurrenceSpec::scheduled(
                occurrence_id.clone(),
                revision_id.clone(),
                OccurrenceOrigin::Scheduled,
                900,
                900,
            ))
            .expect("insert occurrence");
        runtime
            .enqueue_backup_work(BackupWorkSpec::new(
                backup_run_id.clone(),
                WorkItemId::new(),
                occurrence_id.clone(),
                revision_id,
                target_id,
                "postgresql",
                "source-fingerprint",
                950,
            ))
            .expect("enqueue backup");
        Connection::open(&db_path)
            .expect("open recovery connection")
            .execute(
                "INSERT INTO catalog_recovery_points(
                     id, origin_instance_id, origin_occurrence_id, origin_backup_run_id,
                     source_target_identity, source_display_name,
                     engine, state, consistency_grade, committed_at_ms,
                     manifest_schema_version, set_manifest_checksum_blake3, signing_key_id,
                     signature_status, required_part_count, present_part_count,
                     engine_metadata_json, first_ingested_at_ms, last_ingested_at_ms
                 ) VALUES (
                     ?1, 'instance-a', ?2, ?3,
                     'target:orders', 'orders',
                     'postgresql', 'complete', 'application_consistent', 1000,
                     1, ?4, 'signing-key-01',
                     'valid', 1, 1,
                     '{}', 1000, 1000
                 )",
                params![
                    recovery_point_id.as_str(),
                    occurrence_id.as_str(),
                    backup_run_id.as_str(),
                    "b".repeat(64)
                ],
            )
            .expect("insert recovery point");
        seed_required_replica(&db_path, &recovery_point_id);

        install_certification(&store);
        let notifications = store.notification();
        notifications
            .insert_channel(NotificationChannelSpec::webhook(
                "channel-verification",
                "Verification webhook",
                r#"{"endpointRef":"secret://notification"}"#,
                500,
            ))
            .expect("insert channel");
        notifications
            .mark_channel_ready("channel-verification", 510)
            .expect("probe channel");
        notifications
            .insert_rule(NotificationRuleSpec::new(
                "rule-verification",
                "Verification events",
                "channel-verification",
                format!(r#"{{"events":["{notification_event}"]}}"#),
                "info",
                0,
                520,
            ))
            .expect("insert rule");
        notifications
            .attach_rule_to_plan(&plan_id, "rule-verification", 530)
            .expect("attach rule");

        Self {
            _temp: temp,
            db_path,
            store,
            recovery_point_id,
        }
    }

    fn count(&self, table: &str) -> i64 {
        assert!(
            matches!(
                table,
                "verification_runs" | "work_items" | "activity_events" | "notification_outbox"
            ),
            "test table allowlist"
        );
        Connection::open(&self.db_path)
            .expect("open count connection")
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("count rows")
    }

    fn verification_state(&self, level: i64) -> Option<String> {
        Connection::open(&self.db_path)
            .expect("open verification query")
            .query_row(
                "SELECT state FROM verification_runs WHERE requested_level = ?1",
                [level],
                |row| row.get(0),
            )
            .ok()
    }

    fn count_verification_work(&self) -> i64 {
        Connection::open(&self.db_path)
            .expect("open work query")
            .query_row(
                "SELECT COUNT(*) FROM work_items WHERE kind = 'verification'",
                [],
                |row| row.get(0),
            )
            .expect("count verification work")
    }
}

fn seed_required_replica(
    database_path: &std::path::Path,
    recovery_point_id: &RecoveryPointId,
) -> String {
    let connection = Connection::open(database_path).expect("open replica fixture writer");
    let part_id = Uuid::new_v4().to_string();
    let replica_id = Uuid::new_v4().to_string();
    connection
        .execute(
            "INSERT INTO catalog_artifact_parts(
                 id, recovery_point_id, part_role, logical_name, required,
                 dump_format, compression_format, encryption_format,
                 plaintext_checksum_blake3, plaintext_bytes, cipher_bytes,
                 metadata_json, created_at_ms
             ) VALUES (
                 ?1, ?2, 'database_dump', 'database.dump', 1,
                 'custom', 'zstd', 'chacha20poly1305_stream',
                 ?3, 128, 96,
                 '{}', 1000
             )",
            params![part_id, recovery_point_id.as_str(), "c".repeat(64)],
        )
        .expect("insert required part");
    connection
        .execute(
            "INSERT INTO catalog_artifact_replicas(
                 id, recovery_point_id, destination_fingerprint, destination_kind,
                 requirement, state, immutability_capability, reader_status,
                 committed_at_ms, last_observed_at_ms, metadata_json
             ) VALUES (
                 ?1, ?2, ?3, 'local',
                 'required', 'committed', 'none', 'eligible',
                 1000, 1000, '{}'
             )",
            params![
                replica_id,
                recovery_point_id.as_str(),
                format!("local:{replica_id}")
            ],
        )
        .expect("insert required replica");
    connection
        .execute(
            "INSERT INTO catalog_stored_objects(
                 id, recovery_point_id, artifact_replica_id, artifact_part_id,
                 object_role, object_key, checksum_blake3, size_bytes,
                 state, observed_at_ms, metadata_json
             ) VALUES (
                 ?1, ?2, ?3, NULL,
                 'set_manifest', 'fixture/set-manifest.json', ?4, 512,
                 'present', 1000, '{}'
             )",
            params![
                Uuid::new_v4().to_string(),
                recovery_point_id.as_str(),
                replica_id,
                "d".repeat(64)
            ],
        )
        .expect("insert manifest object");
    connection
        .execute(
            "INSERT INTO catalog_stored_objects(
                 id, recovery_point_id, artifact_replica_id, artifact_part_id,
                 object_role, object_key, checksum_blake3, size_bytes,
                 state, observed_at_ms, metadata_json
             ) VALUES (
                 ?1, ?2, ?3, ?4,
                 'payload', 'fixture/database.dump.enc', ?5, 96,
                 'present', 1000, '{}'
             )",
            params![
                Uuid::new_v4().to_string(),
                recovery_point_id.as_str(),
                replica_id,
                part_id,
                "e".repeat(64)
            ],
        )
        .expect("insert payload object");
    replica_id
}

fn verification_source_replica(
    database_path: &std::path::Path,
    verification_run_id: &VerificationRunId,
) -> Option<String> {
    Connection::open(database_path)
        .expect("open verification source query")
        .query_row(
            "SELECT source_replica_id FROM verification_runs WHERE id = ?1",
            [verification_run_id.as_str()],
            |row| row.get(0),
        )
        .expect("query verification source")
}

fn install_certification(store: &OperationalStore) {
    let mut candidate = EngineCertificationSpec::candidate(
        "cert-pg-current",
        "postgresql",
        "14-18",
        "pg_dump",
        "14-18",
        "linux",
        "amd64",
        "custom",
        "application_consistent",
        "artifacts/g2/postgresql.json",
        "c".repeat(64),
        500,
    );
    for capability in [
        "probe",
        "discovery",
        "tool_preflight",
        "backup",
        "verify_l1",
        "verify_l2",
        "restore_create_new",
        "validation",
        "restore_l3",
    ] {
        candidate = candidate.with_capability(CertificationCapability::passed(
            capability,
            format!("artifacts/g2/postgresql.json#{capability}"),
        ));
    }
    let certifications = store.certification();
    certifications
        .insert_candidate(candidate)
        .expect("insert certification");
    certifications
        .certify("cert-pg-current", 600, 5_000)
        .expect("certify engine");
}

fn install_mysql_l2_certification(store: &OperationalStore) {
    let mut candidate = EngineCertificationSpec::candidate(
        "cert-mysql-legacy",
        "mysql",
        ">=8,<9",
        "mysqldump",
        ">=8,<9",
        "windows",
        "x86_64",
        "plain",
        "transaction-consistent",
        "artifacts/g2/mysql.json",
        "d".repeat(64),
        500,
    );
    for capability in ProviderCapability::REQUIRED {
        candidate = candidate.with_capability(CertificationCapability::passed(
            capability.as_str(),
            format!("artifacts/g2/mysql.json#{}", capability.as_str()),
        ));
    }
    let certifications = store.certification();
    certifications
        .insert_candidate(candidate)
        .expect("insert legacy mysql certification");
    certifications
        .certify("cert-mysql-legacy", 600, 5_000)
        .expect("certify legacy mysql capability");
}
