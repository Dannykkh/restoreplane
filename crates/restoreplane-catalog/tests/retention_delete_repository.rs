use restoreplane_catalog::{
    rusqlite::{params, Connection},
    ClaimRequest, ClaimTerminalOutcome, OperationWorkSpec, OperationalStore,
    RetentionDeleteSettlementOutcome, RetentionDeleteSettlementSpec, RetryDisposition,
};
use restoreplane_domain::{OperationRunId, RecoveryPointId, WorkItemId};
use restoreplane_pipeline::{
    DeleteObjectDispositionV2, DeleteObjectObservationV2, RetentionDeleteReportV2,
    RetentionObjectRoleV2,
};
use tempfile::TempDir;
use uuid::Uuid;

#[test]
fn partial_delete_projects_successes_and_atomically_requeues_with_attention() {
    let fixture = Fixture::new();
    let claim = fixture.enqueue_and_start(1_000);
    let repository = fixture.store.runtime();

    let outcome = repository
        .settle_retention_delete(
            &claim,
            RetentionDeleteSettlementSpec::new(
                fixture.recovery_point_id.clone(),
                fixture.destination_fingerprint.clone(),
                report(
                    DeleteObjectDispositionV2::Deleted,
                    DeleteObjectDispositionV2::RetryRequired,
                ),
                1_010,
                1_100,
            ),
        )
        .expect("settle partial delete");

    assert_eq!(
        outcome,
        RetentionDeleteSettlementOutcome::Retry(RetryDisposition::Requeued)
    );
    let direct = fixture.direct();
    let states = object_states(&direct, &fixture.replica_id);
    assert_eq!(
        states,
        vec![
            ("payload/object.bin".to_owned(), "present".to_owned()),
            ("set/manifest.json".to_owned(), "deleted".to_owned()),
        ]
    );
    let runtime: (String, String, String, Option<String>) = direct
        .query_row(
            "SELECT work_items.state, work_attempts.state, operation_runs.state,
                    operation_runs.result_json
             FROM work_items
             JOIN work_attempts ON work_attempts.work_item_id = work_items.id
             JOIN operation_runs ON operation_runs.id = work_items.operation_run_id
             WHERE work_items.id = ?1",
            [fixture.work_item_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("query runtime state");
    assert_eq!(runtime.0, "queued");
    assert_eq!(runtime.1, "failed");
    assert_eq!(runtime.2, "running");
    let result: serde_json::Value =
        serde_json::from_str(runtime.3.as_deref().expect("attempt result")).expect("result JSON");
    assert_eq!(result["deleted_count"], 1);
    assert_eq!(result["retry_required_count"], 1);

    let projection: (String, String, String, String, String, String) = direct
        .query_row(
            "SELECT replicas.state, replicas.reader_status,
                    snapshots.artifact_completeness, snapshots.replica_durability,
                    snapshots.protection_compliance, snapshots.restore_eligibility
             FROM catalog_artifact_replicas AS replicas
             JOIN catalog_snapshot_projection AS snapshots
               ON snapshots.recovery_point_id = replicas.recovery_point_id
             WHERE replicas.id = ?1",
            [fixture.replica_id.as_str()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .expect("query projected truth");
    assert_eq!(
        projection,
        (
            "degraded".to_owned(),
            "unavailable".to_owned(),
            "incomplete".to_owned(),
            "unavailable".to_owned(),
            "non_compliant".to_owned(),
            "blocked".to_owned(),
        )
    );
    let evidence: (i64, i64) = direct
        .query_row(
            "SELECT
                 (SELECT COUNT(*) FROM activity_events
                  WHERE event_code = 'RETENTION_DELETE_INCOMPLETE'),
                 (SELECT COUNT(*) FROM attention_states
                  WHERE entity_id = ?1 AND state = 'open')",
            [fixture.replica_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("query retry evidence");
    assert_eq!(evidence, (1, 1));
}

#[test]
fn marker_retry_requeues_without_inventing_payload_retry_evidence() {
    let fixture = Fixture::new();
    let claim = fixture.enqueue_and_start(1_500);

    let outcome = fixture
        .store
        .runtime()
        .settle_retention_delete(
            &claim,
            RetentionDeleteSettlementSpec::new(
                fixture.recovery_point_id.clone(),
                fixture.destination_fingerprint.clone(),
                report(
                    DeleteObjectDispositionV2::RetryRequired,
                    DeleteObjectDispositionV2::NotAttemptedMarkerBlocked,
                ),
                1_510,
                1_600,
            ),
        )
        .expect("settle marker retry");

    assert_eq!(
        outcome,
        RetentionDeleteSettlementOutcome::Retry(RetryDisposition::Requeued)
    );
    let direct = fixture.direct();
    assert_eq!(
        object_states(&direct, &fixture.replica_id),
        vec![
            ("payload/object.bin".to_owned(), "present".to_owned()),
            ("set/manifest.json".to_owned(), "present".to_owned()),
        ]
    );
    let result_json: String = direct
        .query_row(
            "SELECT operation_runs.result_json
             FROM work_items
             JOIN operation_runs ON operation_runs.id = work_items.operation_run_id
             WHERE work_items.id = ?1",
            [fixture.work_item_id.as_str()],
            |row| row.get(0),
        )
        .expect("query marker retry result");
    let result: serde_json::Value =
        serde_json::from_str(&result_json).expect("marker retry result JSON");
    assert_eq!(result["retry_required_count"], 1);
    assert_eq!(result["blocked_count"], 1);
    assert_eq!(result["observations"][0]["disposition"], "retry_required");
    assert_eq!(
        result["observations"][1]["disposition"],
        "not_attempted_marker_blocked"
    );
}

#[test]
fn marker_lock_and_terminal_failure_keep_distinct_settlement_semantics() {
    let cases = [
        (
            DeleteObjectDispositionV2::RetainedByLock,
            ClaimTerminalOutcome::PartiallySucceeded,
        ),
        (
            DeleteObjectDispositionV2::FailedTerminal,
            ClaimTerminalOutcome::Failed,
        ),
    ];

    for (marker_disposition, expected_outcome) in cases {
        let fixture = Fixture::new();
        let claim = fixture.enqueue_and_start(1_700);
        let outcome = fixture
            .store
            .runtime()
            .settle_retention_delete(
                &claim,
                RetentionDeleteSettlementSpec::new(
                    fixture.recovery_point_id.clone(),
                    fixture.destination_fingerprint.clone(),
                    report(
                        marker_disposition,
                        DeleteObjectDispositionV2::NotAttemptedMarkerBlocked,
                    ),
                    1_710,
                    1_800,
                ),
            )
            .expect("settle blocked marker failure");

        assert_eq!(
            outcome,
            RetentionDeleteSettlementOutcome::Terminal(expected_outcome)
        );
        assert_eq!(
            object_states(&fixture.direct(), &fixture.replica_id),
            vec![
                ("payload/object.bin".to_owned(), "present".to_owned()),
                ("set/manifest.json".to_owned(), "present".to_owned()),
            ]
        );
    }
}

#[test]
fn marker_failure_cannot_claim_that_a_payload_delete_was_attempted() {
    let fixture = Fixture::new();
    let claim = fixture.enqueue_and_start(1_900);

    let error = fixture
        .store
        .runtime()
        .settle_retention_delete(
            &claim,
            RetentionDeleteSettlementSpec::new(
                fixture.recovery_point_id.clone(),
                fixture.destination_fingerprint.clone(),
                report(
                    DeleteObjectDispositionV2::RetryRequired,
                    DeleteObjectDispositionV2::Deleted,
                ),
                1_910,
                2_000,
            ),
        )
        .expect_err("marker failure must make every payload not-attempted");

    assert_eq!(error.code(), "OPERATIONAL_RUNTIME_STATE_INVALID");
    assert_eq!(
        object_states(&fixture.direct(), &fixture.replica_id),
        vec![
            ("payload/object.bin".to_owned(), "present".to_owned()),
            ("set/manifest.json".to_owned(), "present".to_owned()),
        ]
    );
}

#[test]
fn retry_completion_deletes_remaining_objects_and_resolves_attention() {
    let fixture = Fixture::new();
    let first = fixture.enqueue_and_start(2_000);
    let repository = fixture.store.runtime();
    repository
        .settle_retention_delete(
            &first,
            RetentionDeleteSettlementSpec::new(
                fixture.recovery_point_id.clone(),
                fixture.destination_fingerprint.clone(),
                report(
                    DeleteObjectDispositionV2::Deleted,
                    DeleteObjectDispositionV2::RetryRequired,
                ),
                2_010,
                2_100,
            ),
        )
        .expect("schedule retry");

    let second = repository
        .claim_next(ClaimRequest::new(
            "retention-worker",
            "boot-b",
            2_100,
            1_000,
        ))
        .expect("claim retry")
        .expect("retry work");
    repository.start_claim(&second, 2_101).expect("start retry");
    let outcome = repository
        .settle_retention_delete(
            &second,
            RetentionDeleteSettlementSpec::new(
                fixture.recovery_point_id.clone(),
                fixture.destination_fingerprint.clone(),
                report(
                    DeleteObjectDispositionV2::Deleted,
                    DeleteObjectDispositionV2::Deleted,
                ),
                2_110,
                2_200,
            ),
        )
        .expect("complete retry");
    assert_eq!(
        outcome,
        RetentionDeleteSettlementOutcome::Terminal(ClaimTerminalOutcome::Succeeded)
    );

    let direct = fixture.direct();
    assert_eq!(
        object_states(&direct, &fixture.replica_id),
        vec![
            ("payload/object.bin".to_owned(), "deleted".to_owned()),
            ("set/manifest.json".to_owned(), "deleted".to_owned()),
        ]
    );
    let runtime: (String, String, String, String) = direct
        .query_row(
            "SELECT work_items.state, latest_attempt.state, operation_runs.state,
                    attention_states.state
             FROM work_items
             JOIN operation_runs ON operation_runs.id = work_items.operation_run_id
             JOIN work_attempts AS latest_attempt
               ON latest_attempt.work_item_id = work_items.id
              AND latest_attempt.attempt_no = 2
             JOIN attention_states
               ON attention_states.entity_id = ?2
             WHERE work_items.id = ?1",
            params![fixture.work_item_id.as_str(), fixture.replica_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("query completed runtime");
    assert_eq!(
        runtime,
        (
            "succeeded".to_owned(),
            "succeeded".to_owned(),
            "succeeded".to_owned(),
            "resolved".to_owned(),
        )
    );
    let replica: (String, String) = direct
        .query_row(
            "SELECT state, reader_status
             FROM catalog_artifact_replicas
             WHERE id = ?1",
            [fixture.replica_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("query deleted replica");
    assert_eq!(replica, ("missing".to_owned(), "unavailable".to_owned()));
}

#[test]
fn object_lock_is_preserved_as_partial_success_with_attention() {
    let fixture = Fixture::new();
    let claim = fixture.enqueue_and_start(3_000);
    let repository = fixture.store.runtime();

    let outcome = repository
        .settle_retention_delete(
            &claim,
            RetentionDeleteSettlementSpec::new(
                fixture.recovery_point_id.clone(),
                fixture.destination_fingerprint.clone(),
                report(
                    DeleteObjectDispositionV2::Deleted,
                    DeleteObjectDispositionV2::RetainedByLock,
                ),
                3_010,
                3_100,
            ),
        )
        .expect("settle retained object");
    assert_eq!(
        outcome,
        RetentionDeleteSettlementOutcome::Terminal(ClaimTerminalOutcome::PartiallySucceeded)
    );

    let direct = fixture.direct();
    let states = object_states(&direct, &fixture.replica_id);
    assert_eq!(
        states,
        vec![
            ("payload/object.bin".to_owned(), "present".to_owned()),
            ("set/manifest.json".to_owned(), "deleted".to_owned()),
        ]
    );
    let evidence: (String, i64) = direct
        .query_row(
            "SELECT operation_runs.state,
                    (SELECT COUNT(*) FROM attention_states
                     WHERE entity_id = ?2 AND state = 'open')
             FROM operation_runs
             WHERE id = ?1",
            params![
                fixture.operation_run_id.as_str(),
                fixture.replica_id.as_str()
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("query lock evidence");
    assert_eq!(evidence, ("partially_succeeded".to_owned(), 1));
}

#[test]
fn stale_claim_cannot_project_retention_deletion() {
    let fixture = Fixture::new();
    let stale = fixture.enqueue_and_start(4_000);
    let repository = fixture.store.runtime();
    repository
        .retry_claim(&stale, 4_010, 4_100, "TRANSIENT_DELETE_FAILURE", None)
        .expect("release first claim");
    let current = repository
        .claim_next(ClaimRequest::new(
            "retention-worker",
            "boot-b",
            4_100,
            1_000,
        ))
        .expect("claim retry")
        .expect("retry work");
    repository
        .start_claim(&current, 4_101)
        .expect("start current claim");

    let error = repository
        .settle_retention_delete(
            &stale,
            RetentionDeleteSettlementSpec::new(
                fixture.recovery_point_id.clone(),
                fixture.destination_fingerprint.clone(),
                report(
                    DeleteObjectDispositionV2::Deleted,
                    DeleteObjectDispositionV2::Deleted,
                ),
                4_110,
                4_200,
            ),
        )
        .expect_err("stale fence must not mutate catalog truth");
    assert_eq!(error.code(), "WORK_ITEM_FENCE_STALE");
    assert_eq!(
        object_states(&fixture.direct(), &fixture.replica_id),
        vec![
            ("payload/object.bin".to_owned(), "present".to_owned()),
            ("set/manifest.json".to_owned(), "present".to_owned()),
        ]
    );
}

#[test]
fn versioned_objects_with_the_same_key_are_settled_independently() {
    let fixture = Fixture::new();
    let direct = fixture.direct();
    direct
        .execute(
            "UPDATE catalog_stored_objects
             SET object_version = 'version-a'
             WHERE artifact_replica_id = ?1
               AND object_key = 'payload/object.bin'",
            [fixture.replica_id.as_str()],
        )
        .expect("version original payload");
    direct
        .execute(
            "INSERT INTO catalog_stored_objects(
                 id, recovery_point_id, artifact_replica_id, artifact_part_id,
                 object_role, object_key, object_version, checksum_blake3, size_bytes,
                 state, observed_at_ms, metadata_json
             )
             SELECT ?1, recovery_point_id, artifact_replica_id, artifact_part_id,
                    object_role, object_key, 'version-b', ?2, size_bytes,
                    state, observed_at_ms, metadata_json
             FROM catalog_stored_objects
             WHERE artifact_replica_id = ?3
               AND object_key = 'payload/object.bin'
               AND object_version = 'version-a'",
            params![
                Uuid::new_v4().to_string(),
                "e".repeat(64),
                fixture.replica_id.as_str()
            ],
        )
        .expect("insert second payload version");
    drop(direct);

    let claim = fixture.enqueue_and_start(5_000);
    let outcome = fixture
        .store
        .runtime()
        .settle_retention_delete(
            &claim,
            RetentionDeleteSettlementSpec::new(
                fixture.recovery_point_id.clone(),
                fixture.destination_fingerprint.clone(),
                RetentionDeleteReportV2 {
                    observations: vec![
                        DeleteObjectObservationV2 {
                            object_key: "set/manifest.json".to_owned(),
                            object_version: None,
                            role: RetentionObjectRoleV2::SetManifest,
                            disposition: DeleteObjectDispositionV2::Deleted,
                            diagnostic: None,
                        },
                        DeleteObjectObservationV2 {
                            object_key: "payload/object.bin".to_owned(),
                            object_version: Some("version-a".to_owned()),
                            role: RetentionObjectRoleV2::Payload,
                            disposition: DeleteObjectDispositionV2::Deleted,
                            diagnostic: None,
                        },
                        DeleteObjectObservationV2 {
                            object_key: "payload/object.bin".to_owned(),
                            object_version: Some("version-b".to_owned()),
                            role: RetentionObjectRoleV2::Payload,
                            disposition: DeleteObjectDispositionV2::RetryRequired,
                            diagnostic: Some("destination temporarily unavailable".to_owned()),
                        },
                    ],
                },
                5_010,
                5_100,
            ),
        )
        .expect("settle version-aware delete");
    assert_eq!(
        outcome,
        RetentionDeleteSettlementOutcome::Retry(RetryDisposition::Requeued)
    );

    let direct = fixture.direct();
    let mut statement = direct
        .prepare(
            "SELECT object_key, object_version, state
             FROM catalog_stored_objects
             WHERE artifact_replica_id = ?1
             ORDER BY object_key, object_version",
        )
        .expect("prepare versioned object states");
    let states = statement
        .query_map([fixture.replica_id.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .expect("query versioned object states")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect versioned object states");
    assert_eq!(
        states,
        vec![
            (
                "payload/object.bin".to_owned(),
                Some("version-a".to_owned()),
                "deleted".to_owned(),
            ),
            (
                "payload/object.bin".to_owned(),
                Some("version-b".to_owned()),
                "present".to_owned(),
            ),
            ("set/manifest.json".to_owned(), None, "deleted".to_owned(),),
        ]
    );
}

fn report(
    manifest: DeleteObjectDispositionV2,
    payload: DeleteObjectDispositionV2,
) -> RetentionDeleteReportV2 {
    RetentionDeleteReportV2 {
        observations: vec![
            DeleteObjectObservationV2 {
                object_key: "set/manifest.json".to_owned(),
                object_version: None,
                role: RetentionObjectRoleV2::SetManifest,
                disposition: manifest,
                diagnostic: (manifest != DeleteObjectDispositionV2::Deleted)
                    .then(|| "retention_until_ms=Some(9000)".to_owned()),
            },
            DeleteObjectObservationV2 {
                object_key: "payload/object.bin".to_owned(),
                object_version: None,
                role: RetentionObjectRoleV2::Payload,
                disposition: payload,
                diagnostic: (payload != DeleteObjectDispositionV2::Deleted)
                    .then(|| "destination temporarily unavailable".to_owned()),
            },
        ],
    }
}

fn object_states(connection: &Connection, replica_id: &str) -> Vec<(String, String)> {
    let mut statement = connection
        .prepare(
            "SELECT object_key, state
             FROM catalog_stored_objects
             WHERE artifact_replica_id = ?1
             ORDER BY object_key",
        )
        .expect("prepare object state query");
    statement
        .query_map([replica_id], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query object states")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect object states")
}

struct Fixture {
    _temp_dir: TempDir,
    db_path: std::path::PathBuf,
    store: OperationalStore,
    recovery_point_id: RecoveryPointId,
    replica_id: String,
    destination_fingerprint: String,
    operation_run_id: OperationRunId,
    work_item_id: WorkItemId,
}

impl Fixture {
    fn new() -> Self {
        let temp_dir = TempDir::new().expect("temp directory");
        let db_path = temp_dir.path().join("operational.db");
        let store = OperationalStore::open(db_path.clone()).expect("open operational store");
        let recovery_point_id = RecoveryPointId::new();
        let part_id = Uuid::new_v4().to_string();
        let replica_id = Uuid::new_v4().to_string();
        let destination_fingerprint = "local:retention-fixture".to_owned();
        let connection = Connection::open(&db_path).expect("open projection seed");
        connection
            .execute_batch("PRAGMA foreign_keys=ON;")
            .expect("enable foreign keys");
        connection
            .execute(
                "INSERT INTO catalog_recovery_points(
                     id, origin_instance_id, source_target_identity, source_display_name,
                     engine, state, consistency_grade, committed_at_ms,
                     manifest_schema_version, set_manifest_checksum_blake3,
                     signing_key_id, signature_status, required_part_count,
                     present_part_count, engine_metadata_json,
                     first_ingested_at_ms, last_ingested_at_ms
                 ) VALUES (
                     ?1, 'fixture-instance', 'target:orders', 'orders',
                     'postgresql', 'complete', 'transactionally_consistent', 100,
                     2, ?2,
                     'fixture-key', 'valid', 1,
                     1, '{}',
                     100, 100
                 )",
                params![recovery_point_id.as_str(), "a".repeat(64)],
            )
            .expect("insert recovery point");
        connection
            .execute(
                "INSERT INTO catalog_artifact_parts(
                     id, recovery_point_id, part_role, logical_name, required,
                     dump_format, compression_format, encryption_format,
                     metadata_json, created_at_ms
                 ) VALUES (
                     ?1, ?2, 'database_dump', 'orders', 1,
                     'custom', 'zstd', 'age', '{}', 100
                 )",
                params![part_id, recovery_point_id.as_str()],
            )
            .expect("insert artifact part");
        connection
            .execute(
                "INSERT INTO catalog_artifact_replicas(
                     id, recovery_point_id, destination_fingerprint,
                     destination_kind, requirement, state,
                     immutability_capability, reader_status, committed_at_ms,
                     last_observed_at_ms, metadata_json
                 ) VALUES (
                     ?1, ?2, ?3,
                     'local', 'required', 'committed',
                     'none', 'eligible', 100,
                     100, '{}'
                 )",
                params![
                    replica_id,
                    recovery_point_id.as_str(),
                    destination_fingerprint
                ],
            )
            .expect("insert artifact replica");
        connection
            .execute(
                "INSERT INTO catalog_stored_objects(
                     id, recovery_point_id, artifact_replica_id, artifact_part_id,
                     object_role, object_key, checksum_blake3, size_bytes,
                     state, observed_at_ms, metadata_json
                 ) VALUES (
                     ?1, ?2, ?3, ?4,
                     'payload', 'payload/object.bin', ?5, 5,
                     'present', 100, '{}'
                 )",
                params![
                    Uuid::new_v4().to_string(),
                    recovery_point_id.as_str(),
                    replica_id,
                    part_id,
                    "b".repeat(64)
                ],
            )
            .expect("insert payload object");
        connection
            .execute(
                "INSERT INTO catalog_stored_objects(
                     id, recovery_point_id, artifact_replica_id,
                     object_role, object_key, checksum_blake3, size_bytes,
                     state, observed_at_ms, metadata_json
                 ) VALUES (
                     ?1, ?2, ?3,
                     'set_manifest', 'set/manifest.json', ?4, 5,
                     'present', 100, '{}'
                 )",
                params![
                    Uuid::new_v4().to_string(),
                    recovery_point_id.as_str(),
                    replica_id,
                    "c".repeat(64)
                ],
            )
            .expect("insert set manifest object");
        connection
            .execute(
                "INSERT INTO catalog_snapshot_projection(
                     recovery_point_id, source_target_identity, source_display_name,
                     engine, committed_at_ms, artifact_completeness,
                     replica_durability, protection_compliance, restore_eligibility,
                     search_text, projection_version, projected_at_ms
                 ) VALUES (
                     ?1, 'target:orders', 'orders',
                     'postgresql', 100, 'complete',
                     'healthy', 'compliant', 'eligible',
                     'orders target:orders', 1, 100
                 )",
                [recovery_point_id.as_str()],
            )
            .expect("insert snapshot projection");

        Self {
            _temp_dir: temp_dir,
            db_path,
            store,
            recovery_point_id,
            replica_id,
            destination_fingerprint,
            operation_run_id: OperationRunId::new(),
            work_item_id: WorkItemId::new(),
        }
    }

    fn enqueue_and_start(&self, at_ms: i64) -> restoreplane_catalog::ClaimedWork {
        let request_json = serde_json::json!({
            "authorized_at_ms": at_ms,
            "confirmation_digest_blake3": "c".repeat(64),
            "destination_fingerprint": self.destination_fingerprint,
            "dry_run_digest_blake3": "d".repeat(64),
            "recovery_point_id": self.recovery_point_id.as_str(),
            "request_payload_digest_blake3": "e".repeat(64),
            "schema_version": 2,
            "tombstoned_at_ms": at_ms,
        })
        .to_string();
        let repository = self.store.runtime();
        repository
            .enqueue_operation_work(OperationWorkSpec::new(
                self.operation_run_id.clone(),
                self.work_item_id.clone(),
                "retention_delete",
                "artifact_replica",
                self.recovery_point_id.as_str(),
                request_json,
                at_ms,
            ))
            .expect("enqueue retention operation");
        let claim = repository
            .claim_next(ClaimRequest::new(
                "retention-worker",
                "boot-a",
                at_ms,
                1_000,
            ))
            .expect("claim retention operation")
            .expect("retention work");
        repository
            .start_claim(&claim, at_ms + 1)
            .expect("start retention operation");
        claim
    }

    fn direct(&self) -> Connection {
        Connection::open(&self.db_path).expect("open direct query")
    }
}
