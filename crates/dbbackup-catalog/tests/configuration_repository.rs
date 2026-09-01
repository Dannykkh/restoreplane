use dbbackup_catalog::{
    rusqlite::Connection, ConnectionProfileSpec, DatabaseTargetSpec, DestinationSpec,
    OperationalStore, PlanDraftSpec,
};
use dbbackup_domain::{
    BackupPlanId, DatabaseTargetId, DestinationId, DestinationRequirement, PlanRevisionId,
};
use tempfile::TempDir;

#[test]
fn connection_target_plan_publish_is_transactional_and_queryable() {
    let fixture = Fixture::new();
    let repository = fixture.store.configuration();

    repository
        .insert_connection(ConnectionProfileSpec::tcp(
            fixture.connection_id.clone(),
            "primary-pg",
            "postgresql",
            "db.internal",
            5432,
            100,
        ))
        .expect("insert connection");
    repository
        .insert_target(DatabaseTargetSpec::new(
            fixture.first_target_id.clone(),
            fixture.connection_id.clone(),
            "oid:100",
            "orders",
            "orders",
            "fingerprint-orders",
            110,
        ))
        .expect("insert first target");
    repository
        .insert_target(DatabaseTargetSpec::new(
            fixture.second_target_id.clone(),
            fixture.connection_id.clone(),
            "oid:101",
            "billing",
            "billing",
            "fingerprint-billing",
            111,
        ))
        .expect("insert second target");
    repository
        .insert_destination(DestinationSpec::local(
            fixture.destination_id.clone(),
            "local-vault",
            r#"{"path":"D:\\backup"}"#,
            120,
        ))
        .expect("insert destination");
    repository
        .insert_plan_draft(PlanDraftSpec::manual(
            fixture.plan_id.clone(),
            fixture.revision_id.clone(),
            "nightly",
            1,
            "a".repeat(64),
            130,
        ))
        .expect("insert plan and draft revision");
    repository
        .add_revision_target(&fixture.revision_id, &fixture.first_target_id, 131)
        .expect("add first target membership");
    repository
        .add_revision_target(&fixture.revision_id, &fixture.second_target_id, 132)
        .expect("add second target membership");
    repository
        .add_revision_destination(
            &fixture.revision_id,
            &fixture.destination_id,
            DestinationRequirement::Required,
            133,
        )
        .expect("add required destination");

    repository
        .publish_revision(&fixture.revision_id, 140)
        .expect("publish valid revision");

    assert_eq!(
        repository
            .target_count(&fixture.connection_id)
            .expect("count selected targets"),
        2
    );
    assert_eq!(
        repository
            .current_revision(&fixture.plan_id)
            .expect("read current revision"),
        Some(fixture.revision_id.clone())
    );
    let destinations = repository
        .backup_destination_bindings(&fixture.revision_id)
        .expect("load immutable backup destinations");
    assert_eq!(destinations.len(), 1);
    assert_eq!(destinations[0].destination_id(), &fixture.destination_id);
    assert_eq!(destinations[0].kind(), "local");
    assert_eq!(
        destinations[0].requirement(),
        DestinationRequirement::Required
    );
}

#[test]
fn application_and_trigger_both_reject_invalid_or_published_revision_changes() {
    let fixture = Fixture::new();
    let repository = fixture.store.configuration();
    fixture.seed_minimum_configuration(&repository);

    repository
        .insert_plan_draft(PlanDraftSpec::manual(
            fixture.plan_id.clone(),
            fixture.revision_id.clone(),
            "best-effort-only",
            1,
            "b".repeat(64),
            200,
        ))
        .expect("insert draft");
    repository
        .add_revision_target(&fixture.revision_id, &fixture.first_target_id, 201)
        .expect("add target");
    repository
        .add_revision_destination(
            &fixture.revision_id,
            &fixture.destination_id,
            DestinationRequirement::BestEffort,
            202,
        )
        .expect("add best-effort destination");

    let application_error = repository
        .publish_revision(&fixture.revision_id, 210)
        .expect_err("application validation must reject best-effort-only plan");
    assert_eq!(
        application_error.code(),
        "PLAN_REVISION_REQUIRED_DESTINATION_REQUIRED"
    );

    let direct = Connection::open(&fixture.db_path).expect("open direct test writer");
    direct
        .execute_batch("PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")
        .expect("configure direct test writer");
    let trigger_error = direct
        .execute(
            "UPDATE backup_plan_revisions
             SET published_at_ms = 211
             WHERE id = ?1",
            [fixture.revision_id.as_str()],
        )
        .expect_err("trigger must independently reject invalid publish");
    assert!(trigger_error
        .to_string()
        .contains("requires at least one required destination"));

    direct
        .execute(
            "UPDATE plan_revision_destinations
             SET requirement = 'required'
             WHERE plan_revision_id = ?1",
            [fixture.revision_id.as_str()],
        )
        .expect("make destination required");
    repository
        .publish_revision(&fixture.revision_id, 220)
        .expect("publish corrected revision");

    let immutable_update = direct
        .execute(
            "UPDATE backup_plan_revisions
             SET timezone = 'Asia/Seoul'
             WHERE id = ?1",
            [fixture.revision_id.as_str()],
        )
        .expect_err("published revision update must fail");
    assert!(immutable_update
        .to_string()
        .contains("published plan revision is immutable"));

    let immutable_membership = direct
        .execute(
            "DELETE FROM plan_revision_targets
             WHERE plan_revision_id = ?1",
            [fixture.revision_id.as_str()],
        )
        .expect_err("published membership delete must fail");
    assert!(immutable_membership
        .to_string()
        .contains("published plan membership is immutable"));
}

#[test]
fn plan_publish_rejects_mysql_level_two_until_a_native_validator_exists() {
    let fixture = Fixture::new();
    let repository = fixture.store.configuration();
    repository
        .insert_connection(ConnectionProfileSpec::tcp(
            fixture.connection_id.clone(),
            "primary-mysql",
            "mysql",
            "mysql.internal",
            3306,
            100,
        ))
        .expect("insert mysql connection");
    repository
        .insert_target(DatabaseTargetSpec::new(
            fixture.first_target_id.clone(),
            fixture.connection_id.clone(),
            "mysql:orders",
            "orders",
            "orders",
            "fingerprint-orders",
            110,
        ))
        .expect("insert mysql target");
    repository
        .insert_destination(DestinationSpec::local(
            fixture.destination_id.clone(),
            "local-vault",
            r#"{"path":"D:\\backup"}"#,
            120,
        ))
        .expect("insert destination");
    repository
        .insert_plan_draft(
            PlanDraftSpec::manual(
                fixture.plan_id.clone(),
                fixture.revision_id.clone(),
                "mysql-l2",
                1,
                "c".repeat(64),
                130,
            )
            .with_policies(
                Some(60_000),
                r#"{"daily":1}"#,
                r#"{"requestedLevel":2}"#,
                "{}",
            ),
        )
        .expect("insert mysql L2 draft");
    repository
        .add_revision_target(&fixture.revision_id, &fixture.first_target_id, 131)
        .expect("add mysql target");
    repository
        .add_revision_destination(
            &fixture.revision_id,
            &fixture.destination_id,
            DestinationRequirement::Required,
            132,
        )
        .expect("add destination");

    let error = repository
        .publish_revision(&fixture.revision_id, 140)
        .expect_err("unsupported mysql L2 plan must remain a draft");

    assert_eq!(error.code(), "PLAN_REVISION_VERIFICATION_UNSUPPORTED");
    assert_eq!(
        repository
            .current_revision(&fixture.plan_id)
            .expect("read current revision"),
        None
    );

    for policy in [r#"{"requestedLevel":"l2"}"#, "{}"] {
        Connection::open(&fixture.db_path)
            .expect("open verification policy writer")
            .execute(
                "UPDATE backup_plan_revisions
                 SET verification_policy_json = ?2
                 WHERE id = ?1",
                [fixture.revision_id.as_str(), policy],
            )
            .expect("replace draft verification policy");
        let error = repository
            .publish_revision(&fixture.revision_id, 141)
            .expect_err("all valid L2 policy shapes must fail closed for mysql");
        assert_eq!(error.code(), "PLAN_REVISION_VERIFICATION_UNSUPPORTED");
    }
}

#[test]
fn production_plan_publish_requires_ready_target_certification_and_destination_probes() {
    let fixture = Fixture::new();
    let repository = fixture.store.configuration();
    fixture.seed_minimum_configuration(&repository);
    repository
        .insert_plan_draft(
            PlanDraftSpec::manual(
                fixture.plan_id.clone(),
                fixture.revision_id.clone(),
                "production-ready-plan",
                1,
                "d".repeat(64),
                130,
            )
            .with_policies(
                Some(60_000),
                r#"{"daily":1}"#,
                r#"{"requestedLevel":1}"#,
                "{}",
            ),
        )
        .expect("insert production draft");
    repository
        .add_revision_target(&fixture.revision_id, &fixture.first_target_id, 131)
        .expect("add target");
    repository
        .add_revision_destination(
            &fixture.revision_id,
            &fixture.destination_id,
            DestinationRequirement::Required,
            132,
        )
        .expect("add destination");

    let unready_target = repository
        .publish_plan_revision(&fixture.plan_id, &fixture.revision_id, 200)
        .expect_err("an unprobed target must remain a draft");
    assert_eq!(unready_target.code(), "PLAN_REVISION_TARGET_NOT_READY");

    fixture.mark_connection_ready(210);
    let missing_certification = repository
        .publish_plan_revision(&fixture.plan_id, &fixture.revision_id, 220)
        .expect_err("an uncertified engine must remain a draft");
    assert_eq!(
        missing_certification.code(),
        "PLAN_REVISION_ENGINE_CAPABILITY_UNAVAILABLE"
    );

    fixture.seed_postgresql_plan_capabilities("limited", 230, 10_000);
    let limited_capability = repository
        .publish_plan_revision(&fixture.plan_id, &fixture.revision_id, 240)
        .expect_err("a limited verification capability is not production authority");
    assert_eq!(
        limited_capability.code(),
        "PLAN_REVISION_ENGINE_CAPABILITY_UNAVAILABLE"
    );

    fixture.seed_postgresql_plan_capabilities("passed", 250, 10_000);
    let unready_destination = repository
        .publish_plan_revision(&fixture.plan_id, &fixture.revision_id, 260)
        .expect_err("writer and reader probes must both be current successes");
    assert_eq!(
        unready_destination.code(),
        "PLAN_REVISION_DESTINATION_NOT_READY"
    );

    fixture.mark_destination_ready_with_role_probes(270);
    fixture.record_newer_reader_probe("failed", 275);
    let stale_reader_success = repository
        .publish_plan_revision(&fixture.plan_id, &fixture.revision_id, 276)
        .expect_err("an older reader success cannot hide the latest failed probe");
    assert_eq!(
        stale_reader_success.code(),
        "PLAN_REVISION_DESTINATION_NOT_READY"
    );
    fixture.record_newer_reader_probe("passed", 277);
    repository
        .publish_plan_revision(&fixture.plan_id, &fixture.revision_id, 280)
        .expect("publish a fully qualified plan");
    assert_eq!(
        repository
            .current_revision(&fixture.plan_id)
            .expect("read current revision"),
        Some(fixture.revision_id.clone())
    );
}

struct Fixture {
    _temp: TempDir,
    db_path: std::path::PathBuf,
    store: OperationalStore,
    connection_id: dbbackup_domain::ConnectionProfileId,
    first_target_id: DatabaseTargetId,
    second_target_id: DatabaseTargetId,
    destination_id: DestinationId,
    plan_id: BackupPlanId,
    revision_id: PlanRevisionId,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().expect("temp dir");
        let db_path = temp.path().join("operational.sqlite");
        let store = OperationalStore::open(db_path.clone()).expect("open operational store");
        Self {
            _temp: temp,
            db_path,
            store,
            connection_id: dbbackup_domain::ConnectionProfileId::new(),
            first_target_id: DatabaseTargetId::new(),
            second_target_id: DatabaseTargetId::new(),
            destination_id: DestinationId::new(),
            plan_id: BackupPlanId::new(),
            revision_id: PlanRevisionId::new(),
        }
    }

    fn seed_minimum_configuration(&self, repository: &dbbackup_catalog::ConfigurationRepository) {
        repository
            .insert_connection(ConnectionProfileSpec::tcp(
                self.connection_id.clone(),
                "primary-pg",
                "postgresql",
                "db.internal",
                5432,
                100,
            ))
            .expect("insert connection");
        repository
            .insert_target(DatabaseTargetSpec::new(
                self.first_target_id.clone(),
                self.connection_id.clone(),
                "oid:100",
                "orders",
                "orders",
                "fingerprint-orders",
                110,
            ))
            .expect("insert target");
        repository
            .insert_destination(DestinationSpec::local(
                self.destination_id.clone(),
                "local-vault",
                r#"{"path":"D:\\backup"}"#,
                120,
            ))
            .expect("insert destination");
    }

    fn write_direct(&self, sql: &str) {
        let connection = Connection::open(&self.db_path).expect("open direct test writer");
        connection
            .execute_batch("PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")
            .expect("configure direct test writer");
        connection
            .execute_batch(sql)
            .expect("apply direct fixture SQL");
    }

    fn mark_connection_ready(&self, observed_at_ms: i64) {
        self.write_direct(&format!(
            "UPDATE connection_profiles
             SET state='ready', last_probe_status='passed',
                 last_probe_at_ms={observed_at_ms}, updated_at_ms={observed_at_ms}
             WHERE id='{}';",
            self.connection_id.as_str()
        ));
    }

    fn seed_postgresql_plan_capabilities(
        &self,
        verification_status: &str,
        certified_at_ms: i64,
        expires_at_ms: i64,
    ) {
        let certification_id = format!("cert-postgresql-{verification_status}");
        self.write_direct(&format!(
            "INSERT INTO engine_certifications(
                 id, engine, server_version_range, tool_name, tool_version_range,
                 operating_system, architecture, dump_format, consistency_grade,
                 status, evidence_locator, evidence_checksum_blake3,
                 certified_at_ms, expires_at_ms, suspended_reason,
                 created_at_ms, updated_at_ms
             ) VALUES (
                 '{certification_id}', 'postgresql', '>=17,<19', 'pg_dump', '>=17,<19',
                 'windows', 'x86_64', 'custom', 'application-consistent',
                 'certified', 'package://provider-certifications.json#/postgresql',
                 '{}', {certified_at_ms}, {expires_at_ms}, NULL,
                 {certified_at_ms}, {certified_at_ms}
             );
             INSERT INTO engine_certification_capabilities(
                 certification_id, capability, support_status,
                 limitation_text, evidence_locator, created_at_ms
             ) VALUES
                 ('{certification_id}', 'backup', 'passed', NULL,
                  'package://provider-certifications.json#/postgresql/backup', {certified_at_ms}),
                 ('{certification_id}', 'verify_l1', '{verification_status}',
                  CASE WHEN '{verification_status}'='limited' THEN 'qualification incomplete' ELSE NULL END,
                  'package://provider-certifications.json#/postgresql/verify_l1', {certified_at_ms});",
            "a".repeat(64)
        ));
    }

    fn mark_destination_ready_with_role_probes(&self, observed_at_ms: i64) {
        self.write_direct(&format!(
            "INSERT INTO destination_probe_runs(
                 id, destination_id, credential_role, status,
                 observed_capabilities_json, free_bytes, latency_ms,
                 failure_class, redacted_diagnostics,
                 started_at_ms, completed_at_ms, created_at_ms
             ) VALUES
                 ('11111111-1111-4111-8111-111111111111', '{}', 'backup_writer', 'passed',
                  '{{}}', 1000000, 1, NULL, NULL, {observed_at_ms}, {observed_at_ms}, {observed_at_ms}),
                 ('22222222-2222-4222-8222-222222222222', '{}', 'restore_reader', 'passed',
                  '{{}}', 1000000, 1, NULL, NULL, {observed_at_ms}, {observed_at_ms}, {observed_at_ms});
             UPDATE destinations
             SET state='ready', last_probe_status='passed',
                 last_probe_at_ms={observed_at_ms}, updated_at_ms={observed_at_ms}
             WHERE id='{}';",
            self.destination_id.as_str(),
            self.destination_id.as_str(),
            self.destination_id.as_str()
        ));
    }

    fn record_newer_reader_probe(&self, status: &str, observed_at_ms: i64) {
        let id = if status == "passed" {
            "44444444-4444-4444-8444-444444444444"
        } else {
            "33333333-3333-4333-8333-333333333333"
        };
        let (failure_class, diagnostics) = if status == "passed" {
            ("NULL", "NULL")
        } else {
            ("'network'", "'bounded test failure'")
        };
        self.write_direct(&format!(
            "INSERT INTO destination_probe_runs(
                 id, destination_id, credential_role, status,
                 observed_capabilities_json, free_bytes, latency_ms,
                 failure_class, redacted_diagnostics,
                 started_at_ms, completed_at_ms, created_at_ms
             ) VALUES (
                 '{id}', '{}', 'restore_reader', '{status}',
                 '{{}}', NULL, 1, {failure_class}, {diagnostics},
                 {observed_at_ms}, {observed_at_ms}, {observed_at_ms}
             );",
            self.destination_id.as_str()
        ));
    }
}
