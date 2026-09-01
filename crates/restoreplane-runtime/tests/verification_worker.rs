use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use restoreplane_catalog::{
    rusqlite::{params, Connection},
    OperationalStore, VerificationWorkSpec,
};
use restoreplane_domain::{RecoveryPointId, VerificationRunId, WorkItemId};
use restoreplane_runtime::{
    RuntimeClock, VerificationExecutionError, VerificationExecutionEvidence, VerificationExecutor,
    VerificationWorker, VerificationWorkerConfig, VerificationWorkerOutcome,
};
use tempfile::TempDir;
use uuid::Uuid;

#[tokio::test]
async fn queued_verification_reaches_passed_with_exact_level_and_evidence() {
    let fixture = Fixture::new();
    let clock = Arc::new(ManualClock::new(1_100));
    let worker = fixture.worker(Arc::new(PassingExecutor), clock);

    let outcome = worker.run_once().await.expect("execute verification");

    assert_eq!(
        outcome,
        VerificationWorkerOutcome::Passed {
            verification_run_id: fixture.verification_run_id.to_string(),
            requested_level: 2,
        }
    );
    assert_eq!(
        fixture.verification_row(),
        (
            "passed".to_owned(),
            Some(2),
            Some(r#"{"nativeValidator":"pg_restore --list","result":"passed"}"#.to_owned()),
            None,
        )
    );
    assert_eq!(fixture.work_state(), "succeeded");
    let projection = fixture.verification_projection_row();
    assert_eq!(projection.0, fixture.verification_run_id.as_str());
    assert_eq!(projection.1, 2);
    assert_eq!(projection.2, "passed");
    assert_eq!(projection.3, 1_100);
    assert_eq!(
        projection.4,
        blake3::hash(r#"{"nativeValidator":"pg_restore --list","result":"passed"}"#.as_bytes(),)
            .to_hex()
            .to_string()
    );
    assert_eq!(fixture.snapshot_verification_row(), (Some(2), Some(1_100)));
}

#[tokio::test]
async fn unsupported_native_level_terminalizes_once_without_requeue() {
    let fixture = Fixture::new();
    let clock = Arc::new(ManualClock::new(1_100));
    let worker = fixture.worker(Arc::new(UnsupportedExecutor), clock);

    let outcome = worker
        .run_once()
        .await
        .expect("reject unsupported verification");

    assert_eq!(
        outcome,
        VerificationWorkerOutcome::Failed {
            verification_run_id: fixture.verification_run_id.to_string(),
            error_code: "VERIFY_L2_NATIVE_UNSUPPORTED".to_owned(),
        }
    );
    assert_eq!(fixture.verification_row().0, "failed");
    assert_eq!(fixture.work_state(), "failed_terminal");
    assert_eq!(
        worker
            .run_once()
            .await
            .expect("no repeated unsupported work"),
        VerificationWorkerOutcome::Idle
    );
}

#[tokio::test]
async fn retry_budget_is_bounded_and_never_creates_a_lower_level_run() {
    let fixture = Fixture::new();
    let clock = Arc::new(ManualClock::new(1_100));
    let worker = fixture.worker(Arc::new(RetryingExecutor), clock.clone());

    for expected_available in [1_110, 1_120] {
        let outcome = worker.run_once().await.expect("retry verification");
        assert!(matches!(
            outcome,
            VerificationWorkerOutcome::RetryScheduled {
                available_at_ms,
                ..
            } if available_at_ms == expected_available
        ));
        clock.set(expected_available);
    }
    let outcome = worker.run_once().await.expect("exhaust verification");

    assert_eq!(
        outcome,
        VerificationWorkerOutcome::Failed {
            verification_run_id: fixture.verification_run_id.to_string(),
            error_code: "RETRY_BUDGET_EXHAUSTED".to_owned(),
        }
    );
    assert_eq!(
        fixture.verification_row(),
        (
            "failed".to_owned(),
            None,
            None,
            Some("RETRY_BUDGET_EXHAUSTED".to_owned()),
        )
    );
    assert_eq!(fixture.verification_count_at_level(1), 0);
    assert_eq!(fixture.attempt_count(), 3);
    assert_eq!(
        fixture.verification_projection_row().2,
        "failed",
        "terminal failure evidence remains queryable"
    );
    assert_eq!(
        fixture.snapshot_verification_row(),
        (None, None),
        "failed verification must not upgrade snapshot truth"
    );
}

#[tokio::test]
async fn conflicting_projection_identity_aborts_terminal_settlement() {
    let fixture = Fixture::new();
    fixture.insert_conflicting_verification_projection();
    let clock = Arc::new(ManualClock::new(1_100));
    let worker = fixture.worker(Arc::new(PassingExecutor), clock);

    worker
        .run_once()
        .await
        .expect_err("divergent durable evidence must abort settlement");

    assert_eq!(
        fixture.verification_projection_row(),
        (
            fixture.verification_run_id.to_string(),
            1,
            "failed".to_owned(),
            999,
            "b".repeat(64),
        ),
        "the pre-existing evidence identity must not be overwritten"
    );
    assert_eq!(
        fixture.snapshot_verification_row(),
        (None, None),
        "an aborted settlement must not upgrade snapshot truth"
    );
}

struct PassingExecutor;

struct UnsupportedExecutor;

#[async_trait]
impl VerificationExecutor for UnsupportedExecutor {
    async fn admit(
        &self,
        _claim: &restoreplane_catalog::ClaimedWork,
        _context: &restoreplane_catalog::VerificationClaimContext,
    ) -> Result<restoreplane_runtime::VerificationAdmission, VerificationExecutionError> {
        Err(
            VerificationExecutionError::terminal("VERIFY_L2_NATIVE_UNSUPPORTED")
                .expect("valid terminal error"),
        )
    }

    async fn execute(
        &self,
        _claim: &restoreplane_catalog::ClaimedWork,
        _context: &restoreplane_catalog::VerificationClaimContext,
    ) -> Result<VerificationExecutionEvidence, VerificationExecutionError> {
        panic!("parked verification must not execute")
    }
}

#[async_trait]
impl VerificationExecutor for PassingExecutor {
    async fn execute(
        &self,
        _claim: &restoreplane_catalog::ClaimedWork,
        context: &restoreplane_catalog::VerificationClaimContext,
    ) -> Result<VerificationExecutionEvidence, VerificationExecutionError> {
        assert_eq!(context.requested_level(), 2);
        VerificationExecutionEvidence::new(
            r#"{"nativeValidator":"pg_restore --list","result":"passed"}"#,
        )
        .map_err(|_| VerificationExecutionError::terminal("VERIFY_EVIDENCE_INVALID").unwrap())
    }
}

struct RetryingExecutor;

#[async_trait]
impl VerificationExecutor for RetryingExecutor {
    async fn execute(
        &self,
        _claim: &restoreplane_catalog::ClaimedWork,
        _context: &restoreplane_catalog::VerificationClaimContext,
    ) -> Result<VerificationExecutionEvidence, VerificationExecutionError> {
        Err(VerificationExecutionError::retryable("VERIFY_TOOL_BUSY").expect("valid error"))
    }
}

struct ManualClock(AtomicI64);

impl ManualClock {
    fn new(now_ms: i64) -> Self {
        Self(AtomicI64::new(now_ms))
    }

    fn set(&self, now_ms: i64) {
        self.0.store(now_ms, Ordering::SeqCst);
    }
}

impl RuntimeClock for ManualClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct Fixture {
    _temp: TempDir,
    db_path: std::path::PathBuf,
    store: OperationalStore,
    verification_run_id: VerificationRunId,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().expect("temp directory");
        let db_path = temp.path().join("operational.db");
        let store = OperationalStore::open(db_path.clone()).expect("open store");
        let recovery_point_id = RecoveryPointId::new();
        let verification_run_id = VerificationRunId::new();
        Connection::open(&db_path)
            .expect("open fixture connection")
            .execute_batch("PRAGMA foreign_keys=ON;")
            .expect("enable foreign keys");
        let connection = Connection::open(&db_path).expect("open fixture connection");
        connection
            .execute(
                "INSERT INTO catalog_recovery_points(
                     id, origin_instance_id, source_target_identity, source_display_name,
                     engine, state, consistency_grade, committed_at_ms,
                     manifest_schema_version, set_manifest_checksum_blake3, signing_key_id,
                     signature_status, required_part_count, present_part_count,
                     engine_metadata_json, first_ingested_at_ms, last_ingested_at_ms
                 ) VALUES (
                     ?1, 'instance-a', 'target:orders', 'orders',
                     'postgresql', 'complete', 'application_consistent', 900,
                     1, ?2, 'signing-key-01',
                     'valid', 1, 1,
                     '{}', 900, 900
                 )",
                params![recovery_point_id.as_str(), "a".repeat(64)],
            )
            .expect("insert recovery point");
        connection
            .execute(
                "INSERT INTO catalog_snapshot_projection(
                     recovery_point_id, source_target_identity, source_display_name,
                     engine, committed_at_ms, artifact_completeness, replica_durability,
                     protection_compliance, restore_eligibility, search_text,
                     projection_version, projected_at_ms
                 ) VALUES (
                     ?1, 'target:orders', 'orders', 'postgresql', 900,
                     'complete', 'healthy', 'compliant', 'eligible', 'orders', 1, 900
                 )",
                [recovery_point_id.as_str()],
            )
            .expect("insert snapshot projection");
        seed_required_replica(&connection, &recovery_point_id);
        store
            .verification()
            .enqueue(VerificationWorkSpec::post_backup(
                verification_run_id.clone(),
                WorkItemId::new(),
                recovery_point_id,
                2,
                1_000,
            ))
            .expect("enqueue verification");
        Self {
            _temp: temp,
            db_path,
            store,
            verification_run_id,
        }
    }

    fn worker(
        &self,
        executor: Arc<dyn VerificationExecutor>,
        clock: Arc<dyn RuntimeClock>,
    ) -> VerificationWorker {
        VerificationWorker::new(
            self.store.runtime(),
            executor,
            clock,
            VerificationWorkerConfig::new("verification-worker", "boot-a", 1_000, 10)
                .expect("worker config"),
        )
    }

    fn verification_row(&self) -> (String, Option<i64>, Option<String>, Option<String>) {
        Connection::open(&self.db_path)
            .expect("open query connection")
            .query_row(
                "SELECT state, achieved_level, evidence_json, error_code
                 FROM verification_runs WHERE id = ?1",
                [self.verification_run_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("verification row")
    }

    fn work_state(&self) -> String {
        Connection::open(&self.db_path)
            .expect("open work query")
            .query_row(
                "SELECT state FROM work_items WHERE verification_run_id = ?1",
                [self.verification_run_id.as_str()],
                |row| row.get(0),
            )
            .expect("work state")
    }

    fn verification_count_at_level(&self, level: i64) -> i64 {
        Connection::open(&self.db_path)
            .expect("open level query")
            .query_row(
                "SELECT COUNT(*) FROM verification_runs WHERE requested_level = ?1",
                [level],
                |row| row.get(0),
            )
            .expect("level count")
    }

    fn attempt_count(&self) -> i64 {
        Connection::open(&self.db_path)
            .expect("open attempt query")
            .query_row("SELECT COUNT(*) FROM work_attempts", [], |row| row.get(0))
            .expect("attempt count")
    }

    fn verification_projection_row(&self) -> (String, i64, String, i64, String) {
        Connection::open(&self.db_path)
            .expect("open projection query")
            .query_row(
                "SELECT origin_verification_run_id, verify_level, result,
                        verified_at_ms, evidence_checksum_blake3
                 FROM catalog_verification_projection
                 WHERE origin_verification_run_id = ?1",
                [self.verification_run_id.as_str()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .expect("verification projection")
    }

    fn insert_conflicting_verification_projection(&self) {
        let connection = Connection::open(&self.db_path).expect("open projection fixture");
        let recovery_point_id: String = connection
            .query_row(
                "SELECT recovery_point_id FROM verification_runs WHERE id = ?1",
                [self.verification_run_id.as_str()],
                |row| row.get(0),
            )
            .expect("recovery point id");
        let projection_id = Uuid::new_v5(
            &Uuid::NAMESPACE_URL,
            format!(
                "catalog-verification-projection\0{}",
                self.verification_run_id.as_str()
            )
            .as_bytes(),
        )
        .to_string();
        connection
            .execute(
                "INSERT INTO catalog_verification_projection(
                     id, recovery_point_id, origin_verification_run_id,
                     verify_level, result, verified_at_ms,
                     evidence_checksum_blake3, evidence_json, ingested_at_ms
                 ) VALUES (?1, ?2, ?3, 1, 'failed', 999, ?4, '{\"forged\":true}', 999)",
                params![
                    projection_id,
                    recovery_point_id,
                    self.verification_run_id.as_str(),
                    "b".repeat(64),
                ],
            )
            .expect("insert conflicting projection");
    }

    fn snapshot_verification_row(&self) -> (Option<i64>, Option<i64>) {
        Connection::open(&self.db_path)
            .expect("open snapshot verification query")
            .query_row(
                "SELECT snapshot.highest_verified_level, snapshot.last_verified_at_ms
                 FROM catalog_snapshot_projection AS snapshot
                 JOIN verification_runs AS verification
                   ON verification.recovery_point_id = snapshot.recovery_point_id
                 WHERE verification.id = ?1",
                [self.verification_run_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("snapshot verification row")
    }
}

fn seed_required_replica(connection: &Connection, recovery_point_id: &RecoveryPointId) {
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
                 ?3, 128, 96, '{}', 900
             )",
            params![part_id, recovery_point_id.as_str(), "c".repeat(64)],
        )
        .expect("insert required verification part");
    connection
        .execute(
            "INSERT INTO catalog_artifact_replicas(
                 id, recovery_point_id, destination_fingerprint, destination_kind,
                 requirement, state, immutability_capability, reader_status,
                 committed_at_ms, last_observed_at_ms, metadata_json
             ) VALUES (
                 ?1, ?2, ?3, 'local', 'required', 'committed', 'none', 'eligible',
                 900, 900, '{}'
             )",
            params![
                replica_id,
                recovery_point_id.as_str(),
                format!("local:{replica_id}")
            ],
        )
        .expect("insert required verification replica");
    connection
        .execute(
            "INSERT INTO catalog_stored_objects(
                 id, recovery_point_id, artifact_replica_id, artifact_part_id,
                 object_role, object_key, checksum_blake3, size_bytes,
                 state, observed_at_ms, metadata_json
             ) VALUES (
                 ?1, ?2, ?3, NULL, 'set_manifest', 'fixture/set-manifest.json',
                 ?4, 512, 'present', 900, '{}'
             )",
            params![
                Uuid::new_v4().to_string(),
                recovery_point_id.as_str(),
                replica_id,
                "d".repeat(64)
            ],
        )
        .expect("insert verification manifest object");
    connection
        .execute(
            "INSERT INTO catalog_stored_objects(
                 id, recovery_point_id, artifact_replica_id, artifact_part_id,
                 object_role, object_key, checksum_blake3, size_bytes,
                 state, observed_at_ms, metadata_json
             ) VALUES (
                 ?1, ?2, ?3, ?4, 'payload', 'fixture/database.dump.enc',
                 ?5, 96, 'present', 900, '{}'
             )",
            params![
                Uuid::new_v4().to_string(),
                recovery_point_id.as_str(),
                replica_id,
                part_id,
                "e".repeat(64)
            ],
        )
        .expect("insert verification payload object");
}
