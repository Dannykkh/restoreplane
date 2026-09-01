use restoreplane_catalog::{
    rusqlite::{params, Connection},
    ConnectionProfileSpec, IpcCompletionSpec, IpcRequestSpec, OperationalStore, SafeRestoreSpec,
};
use restoreplane_domain::{
    ArtifactReplicaId, ConnectionProfileId, IpcRequestId, RecoveryPointId, RestoreRunId, WorkItemId,
};
use serde_json::json;
use tempfile::TempDir;

#[test]
fn safe_restore_request_is_atomic_idempotent_and_conflict_safe() {
    let temp = TempDir::new().expect("temp directory");
    let database_path = temp.path().join("operational.db");
    let store = OperationalStore::open(database_path.clone()).expect("open operational store");
    let target_connection_id = ConnectionProfileId::new();
    store
        .configuration()
        .insert_connection(ConnectionProfileSpec::tcp(
            target_connection_id.clone(),
            "Recovery PostgreSQL",
            "postgresql",
            "recovery.internal",
            5432,
            10,
        ))
        .expect("insert target connection");
    let recovery_point_id = RecoveryPointId::new();
    let source_replica_id = ArtifactReplicaId::new();
    seed_restore_truth(
        &database_path,
        &target_connection_id,
        &recovery_point_id,
        &source_replica_id,
    );

    let repository = store.ipc();
    let sid_hash = "1".repeat(64);
    let payload_digest = "2".repeat(64);
    let target_fingerprint = "3".repeat(64);
    let preflight_version = "4".repeat(64);
    let accepted_at_ms = 100;

    let missing_source_id = RecoveryPointId::new();
    let missing_restore_id = RestoreRunId::new();
    let missing_work_id = WorkItemId::new();
    let failed = repository
        .enqueue_safe_restore_request(
            request(
                &sid_hash,
                "safe-restore-invalid-source",
                &payload_digest,
                accepted_at_ms,
            ),
            restore(
                missing_restore_id.clone(),
                missing_work_id,
                missing_source_id.clone(),
                source_replica_id.as_str(),
                target_connection_id.clone(),
                &target_fingerprint,
                &preflight_version,
                accepted_at_ms,
            ),
            completion(&missing_restore_id, accepted_at_ms),
        )
        .expect_err("invalid source must roll back the ledger and restore rows");
    assert_eq!(failed.code(), "OPERATIONAL_SECURITY_INPUT_INVALID");
    assert_eq!(repository.request_count().expect("request count"), 0);
    assert_eq!(repository.restore_count().expect("restore count"), 0);

    let restore_run_id = RestoreRunId::new();
    let work_item_id = WorkItemId::new();
    let first = repository
        .enqueue_safe_restore_request(
            request(
                &sid_hash,
                "safe-restore-01",
                &payload_digest,
                accepted_at_ms,
            ),
            restore(
                restore_run_id.clone(),
                work_item_id.clone(),
                recovery_point_id.clone(),
                source_replica_id.as_str(),
                target_connection_id.clone(),
                &target_fingerprint,
                &preflight_version,
                accepted_at_ms,
            ),
            completion(&restore_run_id, accepted_at_ms),
        )
        .expect("atomically enqueue safe restore");
    assert!(first.created());
    assert!(first.outcome().succeeded());
    assert_eq!(first.outcome().result_ref_type(), Some("restore_run"));
    assert_eq!(
        first.outcome().result_ref_id(),
        Some(restore_run_id.as_str())
    );

    let replay_restore_id = RestoreRunId::new();
    let replay = repository
        .enqueue_safe_restore_request(
            request(
                &sid_hash,
                "safe-restore-01",
                &payload_digest,
                accepted_at_ms + 1,
            ),
            restore(
                replay_restore_id.clone(),
                WorkItemId::new(),
                recovery_point_id.clone(),
                source_replica_id.as_str(),
                target_connection_id.clone(),
                &target_fingerprint,
                &preflight_version,
                accepted_at_ms + 1,
            ),
            completion(&replay_restore_id, accepted_at_ms + 1),
        )
        .expect("same request must replay the committed result");
    assert!(!replay.created());
    assert_eq!(replay.outcome(), first.outcome());

    let conflict_restore_id = RestoreRunId::new();
    let conflict = repository
        .enqueue_safe_restore_request(
            request(
                &sid_hash,
                "safe-restore-01",
                &"5".repeat(64),
                accepted_at_ms + 2,
            ),
            restore(
                conflict_restore_id.clone(),
                WorkItemId::new(),
                recovery_point_id,
                source_replica_id.as_str(),
                target_connection_id,
                &target_fingerprint,
                &preflight_version,
                accepted_at_ms + 2,
            ),
            completion(&conflict_restore_id, accepted_at_ms + 2),
        )
        .expect_err("same idempotency key with a different digest must conflict");
    assert_eq!(conflict.code(), "IPC_IDEMPOTENCY_CONFLICT");

    let direct = Connection::open(database_path).expect("open atomicity assertions");
    let counts: (i64, i64, i64, i64) = direct
        .query_row(
            "SELECT
                 (SELECT COUNT(*) FROM ipc_request_ledger),
                 (SELECT COUNT(*) FROM restore_runs),
                 (SELECT COUNT(*) FROM work_items WHERE kind = 'restore'),
                 (SELECT COUNT(*) FROM restore_runs
                  WHERE id = ?1 AND source_replica_id = ?2
                    AND conflict_policy = 'create_new' AND state = 'queued')",
            params![restore_run_id.as_str(), source_replica_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("query atomic row counts");
    assert_eq!(counts, (1, 1, 1, 1));
}

fn request(
    sid_hash: &str,
    idempotency_key: &str,
    payload_digest: &str,
    accepted_at_ms: i64,
) -> IpcRequestSpec {
    IpcRequestSpec::new(
        IpcRequestId::new(),
        "restore.enqueue_safe",
        sid_hash,
        idempotency_key,
        payload_digest,
        accepted_at_ms,
        accepted_at_ms + 1_000,
    )
}

#[allow(clippy::too_many_arguments)]
fn restore(
    restore_run_id: RestoreRunId,
    work_item_id: WorkItemId,
    recovery_point_id: RecoveryPointId,
    source_replica_id: &str,
    target_connection_id: ConnectionProfileId,
    target_fingerprint: &str,
    preflight_version: &str,
    created_at_ms: i64,
) -> SafeRestoreSpec {
    let eligibility = json!({
        "eligible": true,
        "mode": "create_new",
        "preflightVersion": preflight_version,
        "targetFingerprint": target_fingerprint,
        "targetConnectionId": target_connection_id.as_str(),
        "targetName": "orders_restore",
        "source": {
            "snapshotId": recovery_point_id.as_str(),
            "replicaId": source_replica_id
        }
    });
    SafeRestoreSpec::new(
        restore_run_id,
        work_item_id,
        recovery_point_id,
        source_replica_id,
        target_connection_id,
        "orders_restore",
        target_fingerprint,
        preflight_version,
        eligibility.to_string(),
        created_at_ms,
    )
}

fn completion(restore_run_id: &RestoreRunId, completed_at_ms: i64) -> IpcCompletionSpec {
    IpcCompletionSpec::succeeded(
        json!({
            "kind": "success",
            "value": {
                "deduplicated": false,
                "resource": {
                    "kind": "restore_run",
                    "id": restore_run_id.as_str()
                },
                "body": {
                    "restoreRunId": restore_run_id.as_str(),
                    "state": "queued"
                }
            }
        })
        .to_string(),
        Some(("restore_run".to_owned(), restore_run_id.to_string())),
        completed_at_ms,
    )
}

fn seed_restore_truth(
    database_path: &std::path::Path,
    target_connection_id: &ConnectionProfileId,
    recovery_point_id: &RecoveryPointId,
    source_replica_id: &ArtifactReplicaId,
) {
    let connection = Connection::open(database_path).expect("open restore truth fixture");
    connection
        .execute_batch("PRAGMA foreign_keys=ON;")
        .expect("enable foreign keys");
    connection
        .execute(
            "UPDATE connection_profiles
             SET state = 'ready', updated_at_ms = 20
             WHERE id = ?1",
            [target_connection_id.as_str()],
        )
        .expect("mark restore connection ready");
    connection
        .execute(
            "INSERT INTO catalog_recovery_points(
                 id, origin_instance_id, source_target_identity, source_display_name,
                 engine, state, consistency_grade, committed_at_ms,
                 manifest_schema_version, set_manifest_checksum_blake3,
                 signing_key_id, signature_status,
                 required_part_count, present_part_count,
                 engine_metadata_json, first_ingested_at_ms, last_ingested_at_ms
             ) VALUES (
                 ?1, 'fixture-instance', 'source:orders', 'orders',
                 'postgresql', 'complete', 'application_consistent', 30,
                 2, ?2, 'signing-key-01', 'valid',
                 1, 1, '{}', 30, 30
             )",
            params![recovery_point_id.as_str(), "6".repeat(64)],
        )
        .expect("insert recovery point");
    connection
        .execute(
            "INSERT INTO catalog_artifact_replicas(
                 id, recovery_point_id, destination_fingerprint,
                 destination_kind, requirement, state,
                 immutability_capability, reader_status,
                 committed_at_ms, last_observed_at_ms, metadata_json
             ) VALUES (
                 ?1, ?2, 'local:fixture', 'local', 'required', 'committed',
                 'none', 'eligible', 30, 30, '{}'
             )",
            params![source_replica_id.as_str(), recovery_point_id.as_str()],
        )
        .expect("insert eligible replica");
    connection
        .execute(
            "INSERT INTO catalog_snapshot_projection(
                 recovery_point_id, source_target_identity, source_display_name,
                 engine, committed_at_ms, artifact_completeness, replica_durability,
                 protection_compliance, restore_eligibility, search_text,
                 projection_version, projected_at_ms
             ) VALUES (
                 ?1, 'source:orders', 'orders',
                 'postgresql', 30, 'complete', 'healthy',
                 'compliant', 'eligible', 'orders',
                 1, 30
             )",
            [recovery_point_id.as_str()],
        )
        .expect("insert snapshot projection");
}
