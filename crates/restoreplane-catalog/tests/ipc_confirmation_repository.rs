use restoreplane_catalog::{
    rusqlite::{params, Connection},
    ConfirmationSpec, ConnectionProfileSpec, DatabaseTargetSpec, DestinationSecretBindingSpec,
    DestinationSecretRoleV2, DestinationSpec, IpcCompletionSpec, IpcRequestSpec, OperationalStore,
    OverwriteRestoreSpec, PlanDraftSpec, RetentionDeleteSpec, RetentionTombstoneSpec,
    SecretRefSpec,
};
use restoreplane_domain::{
    BackupPlanId, ConfirmationTokenId, ConnectionProfileId, DatabaseTargetId, DestinationId,
    DestinationRequirement, IpcRequestId, OperationRunId, PlanOccurrenceId, PlanRevisionId,
    RecoveryPointId, RestoreRunId, SecretRefId, WorkItemId,
};
use restoreplane_pipeline::RetentionDeleteAuthorizationV2;
use tempfile::TempDir;
use uuid::Uuid;

#[test]
fn overwrite_restore_replays_same_payload_and_rejects_conflict_or_token_reuse() {
    let fixture = Fixture::new();
    let repository = fixture.store.ipc();
    let payload_digest = "b".repeat(64);
    let sid_hash = "c".repeat(64);
    let token_id = ConfirmationTokenId::new();
    let recovery_point_id = RecoveryPointId::new();
    fixture.seed_retention_replica(&recovery_point_id);
    let direct = Connection::open(&fixture.db_path).expect("open restore fixture");
    direct
        .execute(
            "UPDATE connection_profiles SET state = 'ready', updated_at_ms = 150
             WHERE id = ?1",
            [fixture.connection_id.as_str()],
        )
        .expect("mark target connection ready");
    let source_replica_id: String = direct
        .query_row(
            "SELECT id FROM catalog_artifact_replicas WHERE recovery_point_id = ?1",
            [recovery_point_id.as_str()],
            |row| row.get(0),
        )
        .expect("source replica");
    let target_fingerprint = "4".repeat(64);
    let preflight_version = "5".repeat(64);

    repository
        .issue_confirmation(ConfirmationSpec::restore_overwrite(
            token_id.clone(),
            "a".repeat(64),
            sid_hash.clone(),
            "restore_target",
            "orders-recovered",
            target_fingerprint.clone(),
            payload_digest.clone(),
            100,
            1_000,
        ))
        .expect("issue bound confirmation");

    let original_work_id = WorkItemId::new();
    let original_restore_id = RestoreRunId::new();
    let first = repository
        .enqueue_overwrite_restore(
            IpcRequestSpec::new(
                IpcRequestId::new(),
                "restore.enqueue_overwrite",
                sid_hash.clone(),
                "restore-request-01",
                payload_digest.clone(),
                200,
                2_000,
            ),
            &token_id,
            &"a".repeat(64),
            overwrite_restore_spec(
                original_restore_id.clone(),
                original_work_id.clone(),
                &recovery_point_id,
                &source_replica_id,
                &fixture.connection_id,
                &target_fingerprint,
                &preflight_version,
                200,
            ),
            overwrite_completion(&original_restore_id, &original_work_id, 200),
        )
        .expect("consume token and enqueue overwrite");
    assert!(first.created());
    assert_eq!(
        first.outcome().result_ref_id(),
        Some(original_restore_id.as_str())
    );

    let replay_restore_id = RestoreRunId::new();
    let replay_work_id = WorkItemId::new();
    let replay = repository
        .enqueue_overwrite_restore(
            IpcRequestSpec::new(
                IpcRequestId::new(),
                "restore.enqueue_overwrite",
                sid_hash.clone(),
                "restore-request-01",
                payload_digest.clone(),
                210,
                2_000,
            ),
            &token_id,
            &"a".repeat(64),
            overwrite_restore_spec(
                replay_restore_id.clone(),
                replay_work_id.clone(),
                &recovery_point_id,
                &source_replica_id,
                &fixture.connection_id,
                &target_fingerprint,
                &preflight_version,
                210,
            ),
            overwrite_completion(&replay_restore_id, &replay_work_id, 210),
        )
        .expect("return original result for replay");
    assert!(!replay.created());
    assert_eq!(
        replay.outcome().result_ref_id(),
        Some(original_restore_id.as_str())
    );

    let conflict_restore_id = RestoreRunId::new();
    let conflict_work_id = WorkItemId::new();
    let conflict = repository
        .enqueue_overwrite_restore(
            IpcRequestSpec::new(
                IpcRequestId::new(),
                "restore.enqueue_overwrite",
                sid_hash.clone(),
                "restore-request-01",
                "d".repeat(64),
                220,
                2_000,
            ),
            &token_id,
            &"a".repeat(64),
            overwrite_restore_spec(
                conflict_restore_id.clone(),
                conflict_work_id.clone(),
                &recovery_point_id,
                &source_replica_id,
                &fixture.connection_id,
                &target_fingerprint,
                &preflight_version,
                220,
            ),
            overwrite_completion(&conflict_restore_id, &conflict_work_id, 220),
        )
        .expect_err("same key with another payload must conflict");
    assert_eq!(conflict.code(), "IPC_IDEMPOTENCY_CONFLICT");

    let reused_restore_id = RestoreRunId::new();
    let reused_work_id = WorkItemId::new();
    let replayed_token = repository
        .enqueue_overwrite_restore(
            IpcRequestSpec::new(
                IpcRequestId::new(),
                "restore.enqueue_overwrite",
                sid_hash,
                "restore-request-02",
                payload_digest,
                230,
                2_000,
            ),
            &token_id,
            &"a".repeat(64),
            overwrite_restore_spec(
                reused_restore_id.clone(),
                reused_work_id.clone(),
                &recovery_point_id,
                &source_replica_id,
                &fixture.connection_id,
                &target_fingerprint,
                &preflight_version,
                230,
            ),
            overwrite_completion(&reused_restore_id, &reused_work_id, 230),
        )
        .expect_err("consumed confirmation must not be reused");
    assert_eq!(
        replayed_token.code(),
        "CONFIRMATION_TOKEN_INVALID_OR_REPLAYED"
    );

    assert_eq!(repository.request_count().expect("request count"), 1);
    assert_eq!(repository.restore_count().expect("restore count"), 1);
    assert_eq!(
        repository
            .confirmation_state(&token_id)
            .expect("confirmation state"),
        "consumed"
    );
}

#[allow(clippy::too_many_arguments)]
fn overwrite_restore_spec(
    restore_run_id: RestoreRunId,
    work_item_id: WorkItemId,
    recovery_point_id: &RecoveryPointId,
    source_replica_id: &str,
    connection_id: &ConnectionProfileId,
    target_fingerprint: &str,
    preflight_version: &str,
    created_at_ms: i64,
) -> OverwriteRestoreSpec {
    OverwriteRestoreSpec::new(
        restore_run_id,
        work_item_id,
        recovery_point_id.clone(),
        source_replica_id,
        connection_id.clone(),
        "orders-recovered",
        target_fingerprint,
        preflight_version,
        2,
        serde_json::json!({
            "eligible": true,
            "mode": "overwrite",
            "preflightVersion": preflight_version,
            "evaluatedAtMs": 100,
            "expiresAtMs": 1_000,
            "targetConnectionId": connection_id.as_str(),
            "targetName": "orders-recovered",
            "targetFingerprint": target_fingerprint,
            "targetExists": true,
            "requiredVerificationLevel": 2,
            "source": {
                "snapshotId": recovery_point_id.as_str(),
                "replicaId": source_replica_id,
            }
        })
        .to_string(),
        created_at_ms,
    )
}

fn overwrite_completion(
    restore_run_id: &RestoreRunId,
    work_item_id: &WorkItemId,
    completed_at_ms: i64,
) -> IpcCompletionSpec {
    IpcCompletionSpec::succeeded(
        serde_json::json!({
            "kind": "success",
            "value": {
                "restoreRunId": restore_run_id.as_str(),
                "workItemId": work_item_id.as_str(),
            }
        })
        .to_string(),
        Some(("restore_run".to_owned(), restore_run_id.as_str().to_owned())),
        completed_at_ms,
    )
}

#[test]
fn confirmation_terminal_binding_and_overwrite_requirement_are_database_invariants() {
    let fixture = Fixture::new();
    let repository = fixture.store.ipc();
    let token_id = ConfirmationTokenId::new();
    repository
        .issue_confirmation(ConfirmationSpec::restore_overwrite(
            token_id.clone(),
            "e".repeat(64),
            "f".repeat(64),
            "restore_target",
            "orders-recovered",
            "target-fingerprint",
            "1".repeat(64),
            100,
            1_000,
        ))
        .expect("issue confirmation");

    let direct = Connection::open(&fixture.db_path).expect("open direct invariant probe");
    direct
        .execute_batch("PRAGMA foreign_keys=ON;")
        .expect("enable foreign keys");
    let overwrite_without_confirmation = direct.execute(
        "INSERT INTO restore_runs(
             id, recovery_point_id, target_connection_profile_id, target_database_name,
             target_fingerprint, purpose, conflict_policy, eligibility_snapshot_json,
             state, created_at_ms, updated_at_ms
         ) VALUES (?1, ?2, ?3, 'orders-recovered', 'target-fingerprint',
                   'incident_recovery', 'overwrite', '{}', 'created', 200, 200)",
        params![
            RestoreRunId::new().as_str(),
            RecoveryPointId::new().as_str(),
            fixture.connection_id.as_str()
        ],
    );
    assert!(
        overwrite_without_confirmation.is_err(),
        "overwrite restore requires confirmation row"
    );

    let request_id = IpcRequestId::new();
    direct
        .execute(
            "INSERT INTO ipc_request_ledger(
                 request_id, command_name, client_sid_hash, idempotency_key,
                 payload_digest_blake3, state, accepted_at_ms, expires_at_ms, created_at_ms
             ) VALUES (?1, 'restore.enqueue', ?2, 'direct', ?3, 'accepted', 200, 2000, 200)",
            params![request_id.as_str(), "f".repeat(64), "1".repeat(64)],
        )
        .expect("insert ledger row");
    direct
        .execute(
            "UPDATE confirmation_tokens
             SET state = 'consumed', consumed_at_ms = 200, consumed_by_request_id = ?2
             WHERE id = ?1",
            params![token_id.as_str(), request_id.as_str()],
        )
        .expect("consume issued token");

    let reverse = direct.execute(
        "UPDATE confirmation_tokens
         SET state = 'issued', consumed_at_ms = NULL, consumed_by_request_id = NULL
         WHERE id = ?1",
        [token_id.as_str()],
    );
    assert!(reverse.is_err(), "terminal state cannot return to issued");
    let mutate_binding = direct.execute(
        "UPDATE confirmation_tokens SET resource_id = 'another-target' WHERE id = ?1",
        [token_id.as_str()],
    );
    assert!(
        mutate_binding.is_err(),
        "terminal confirmation binding is immutable"
    );
}

#[test]
fn retention_tombstone_consumes_a_bound_confirmation_and_enqueues_durable_delete_work() {
    let fixture = Fixture::new();
    let repository = fixture.store.ipc();
    let token_id = ConfirmationTokenId::new();
    let recovery_point_id = RecoveryPointId::new();
    fixture.seed_retention_replica(&recovery_point_id);
    let destination_fingerprint = fixture.destination_fingerprint.clone();
    let token_digest = "8".repeat(64);
    let sid_hash = "9".repeat(64);
    let operation_run_id = OperationRunId::new();
    let work_item_id = WorkItemId::new();
    repository
        .stage_retention_tombstone(RetentionTombstoneSpec::new(
            operation_run_id.clone(),
            recovery_point_id.clone(),
            &destination_fingerprint,
            300,
            200,
        ))
        .expect("stage retention tombstone");
    let authorization = fixture.retention_authorization(&recovery_point_id, 500, 300);
    let confirmation_digest = authorization.digest_blake3().to_owned();
    repository
        .issue_confirmation(ConfirmationSpec::new(
            token_id.clone(),
            token_digest.clone(),
            "retention_delete",
            sid_hash.clone(),
            "artifact_replica",
            recovery_point_id.as_str(),
            &destination_fingerprint,
            confirmation_digest.clone(),
            490,
            1_000,
        ))
        .expect("issue retention confirmation");
    let request_payload_digest = "1".repeat(64);

    let first = repository
        .enqueue_retention_delete(
            IpcRequestSpec::new(
                IpcRequestId::new(),
                "retention.delete",
                sid_hash,
                "retention-request-01",
                request_payload_digest.clone(),
                501,
                2_000,
            ),
            &token_id,
            &token_digest,
            RetentionDeleteSpec::new(
                operation_run_id.clone(),
                work_item_id.clone(),
                authorization,
            ),
        )
        .expect("consume confirmation and enqueue retention delete");
    assert!(first.created());
    assert_eq!(first.work_item_id(), &work_item_id);
    assert_eq!(
        repository
            .confirmation_state(&token_id)
            .expect("confirmation state"),
        "consumed"
    );

    let direct = Connection::open(&fixture.db_path).expect("open retention probe");
    let row: (String, String, String, String, String) = direct
        .query_row(
            "SELECT operation_runs.operation_kind, operation_runs.subject_type,
                    operation_runs.subject_id, operation_runs.request_json, work_items.state
             FROM operation_runs
             JOIN work_items ON work_items.operation_run_id = operation_runs.id
             WHERE operation_runs.id = ?1",
            [operation_run_id.as_str()],
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
        .expect("retention operation");
    assert_eq!(row.0, "retention_delete");
    assert_eq!(row.1, "artifact_replica");
    assert_eq!(row.2, recovery_point_id.as_str());
    assert_eq!(row.4, "queued");
    let request: serde_json::Value = serde_json::from_str(&row.3).expect("request JSON");
    assert_eq!(request["confirmation_token_id"], token_id.as_str());
    assert_eq!(request["schema_version"], 2);
    assert_eq!(request["confirmation_digest_blake3"], confirmation_digest);
    assert_eq!(
        request["request_payload_digest_blake3"],
        request_payload_digest
    );
    assert_eq!(request["tombstoned_at_ms"], 200);
    assert_eq!(
        request["authorization"]["digest_blake3"],
        request["dry_run_digest_blake3"]
    );
    assert_eq!(
        request["authorization"]["plan"]["objects"][0]["role"],
        "set_manifest"
    );
    assert_eq!(
        request["authorization"]["plan"]["objects"][0]["version"],
        "manifest-version"
    );
    let consumed_at_ms: i64 = direct
        .query_row(
            "SELECT consumed_at_ms FROM confirmation_tokens WHERE id = ?1",
            [token_id.as_str()],
            |row| row.get(0),
        )
        .expect("confirmation consumed timestamp");
    assert_eq!(consumed_at_ms, 501);
}

#[test]
fn retention_delete_requires_a_catalog_tombstone_and_real_elapsed_grace() {
    let fixture = Fixture::new();
    let repository = fixture.store.ipc();
    let token_id = ConfirmationTokenId::new();
    let recovery_point_id = RecoveryPointId::new();
    fixture.seed_retention_replica(&recovery_point_id);
    let destination_fingerprint = fixture.destination_fingerprint.clone();
    let token_digest = "4".repeat(64);
    let sid_hash = "5".repeat(64);
    let operation_run_id = OperationRunId::new();
    let staged = repository
        .stage_retention_tombstone(RetentionTombstoneSpec::new(
            operation_run_id.clone(),
            recovery_point_id.clone(),
            &destination_fingerprint,
            300,
            200,
        ))
        .expect("stage catalog-backed tombstone");
    assert!(staged.created());
    assert_eq!(staged.operation_run_id(), &operation_run_id);
    let authorization = fixture.retention_authorization(&recovery_point_id, 500, 300);
    let confirmation_digest = authorization.digest_blake3().to_owned();
    repository
        .issue_confirmation(ConfirmationSpec::new(
            token_id.clone(),
            token_digest.clone(),
            "retention_delete",
            sid_hash.clone(),
            "artifact_replica",
            recovery_point_id.as_str(),
            &destination_fingerprint,
            confirmation_digest,
            490,
            1_000,
        ))
        .expect("issue retention confirmation");

    let unrecorded = repository
        .enqueue_retention_delete(
            IpcRequestSpec::new(
                IpcRequestId::new(),
                "retention.delete",
                sid_hash.clone(),
                "retention-unrecorded",
                "1".repeat(64),
                501,
                2_000,
            ),
            &token_id,
            &token_digest,
            RetentionDeleteSpec::new(
                OperationRunId::new(),
                WorkItemId::new(),
                authorization.clone(),
            ),
        )
        .expect_err("client-provided tombstone must not authorize deletion");
    assert_eq!(unrecorded.code(), "RETENTION_TOMBSTONE_INVALID");

    let early = repository
        .enqueue_retention_delete(
            IpcRequestSpec::new(
                IpcRequestId::new(),
                "retention.delete",
                sid_hash.clone(),
                "retention-too-early",
                "2".repeat(64),
                499,
                2_000,
            ),
            &token_id,
            &token_digest,
            RetentionDeleteSpec::new(
                operation_run_id.clone(),
                WorkItemId::new(),
                authorization.clone(),
            ),
        )
        .expect_err("grace must be measured from the server-accepted tombstone");
    assert_eq!(early.code(), "OPERATIONAL_SECURITY_INPUT_INVALID");
    assert_eq!(
        repository
            .confirmation_state(&token_id)
            .expect("confirmation remains issued"),
        "issued"
    );

    let accepted = repository
        .enqueue_retention_delete(
            IpcRequestSpec::new(
                IpcRequestId::new(),
                "retention.delete",
                sid_hash,
                "retention-after-grace",
                "3".repeat(64),
                501,
                2_000,
            ),
            &token_id,
            &token_digest,
            RetentionDeleteSpec::new(operation_run_id, WorkItemId::new(), authorization),
        )
        .expect("elapsed tombstone can enqueue exact deletion");
    assert!(accepted.created());
}

#[test]
fn retention_delete_rejects_catalog_object_drift_after_tombstone() {
    let fixture = Fixture::new();
    let repository = fixture.store.ipc();
    let token_id = ConfirmationTokenId::new();
    let recovery_point_id = RecoveryPointId::new();
    fixture.seed_retention_replica(&recovery_point_id);
    let destination_fingerprint = fixture.destination_fingerprint.clone();
    let token_digest = "6".repeat(64);
    let sid_hash = "7".repeat(64);
    let operation_run_id = OperationRunId::new();
    repository
        .stage_retention_tombstone(RetentionTombstoneSpec::new(
            operation_run_id.clone(),
            recovery_point_id.clone(),
            &destination_fingerprint,
            300,
            200,
        ))
        .expect("stage exact object set");
    let authorization = fixture.retention_authorization(&recovery_point_id, 500, 300);
    let digest = authorization.digest_blake3().to_owned();
    repository
        .issue_confirmation(ConfirmationSpec::new(
            token_id.clone(),
            token_digest.clone(),
            "retention_delete",
            sid_hash.clone(),
            "artifact_replica",
            recovery_point_id.as_str(),
            &destination_fingerprint,
            digest,
            490,
            1_000,
        ))
        .expect("issue confirmation");

    let direct = Connection::open(&fixture.db_path).expect("open drift mutation");
    let replica_id: String = direct
        .query_row(
            "SELECT id FROM catalog_artifact_replicas
             WHERE recovery_point_id = ?1 AND destination_fingerprint = ?2",
            params![recovery_point_id.as_str(), destination_fingerprint],
            |row| row.get(0),
        )
        .expect("replica id");
    direct
        .execute(
            "INSERT INTO catalog_stored_objects(
                 id, recovery_point_id, artifact_replica_id,
                 object_role, object_key, object_version, checksum_blake3, size_bytes,
                 state, observed_at_ms, metadata_json
             ) VALUES (
                 ?1, ?2, ?3,
                 'verification_evidence', 'evidence/l1.json', 'evidence-version', ?4, 5,
                 'present', 300, '{}'
             )",
            params![
                Uuid::new_v4().to_string(),
                recovery_point_id.as_str(),
                replica_id,
                "d".repeat(64)
            ],
        )
        .expect("insert post-tombstone object drift");
    let error = repository
        .enqueue_retention_delete(
            IpcRequestSpec::new(
                IpcRequestId::new(),
                "retention.delete",
                sid_hash,
                "retention-object-drift",
                "4".repeat(64),
                501,
                2_000,
            ),
            &token_id,
            &token_digest,
            RetentionDeleteSpec::new(operation_run_id, WorkItemId::new(), authorization),
        )
        .expect_err("post-tombstone object drift must invalidate deletion");
    assert_eq!(error.code(), "RETENTION_TOMBSTONE_INVALID");
    assert_eq!(
        repository
            .confirmation_state(&token_id)
            .expect("confirmation remains issued"),
        "issued"
    );
}

#[test]
fn retention_confirmation_binding_mismatch_rolls_back_ledger_and_work() {
    let fixture = Fixture::new();
    let repository = fixture.store.ipc();
    let token_id = ConfirmationTokenId::new();
    let recovery_point_id = RecoveryPointId::new();
    fixture.seed_retention_replica(&recovery_point_id);
    let destination_fingerprint = fixture.destination_fingerprint.clone();
    let token_digest = "b".repeat(64);
    let sid_hash = "c".repeat(64);
    let operation_run_id = OperationRunId::new();
    repository
        .stage_retention_tombstone(RetentionTombstoneSpec::new(
            operation_run_id.clone(),
            recovery_point_id.clone(),
            &destination_fingerprint,
            300,
            200,
        ))
        .expect("stage retention tombstone");
    let authorization = fixture.retention_authorization(&recovery_point_id, 500, 300);
    let confirmation_digest = authorization.digest_blake3().to_owned();
    repository
        .issue_confirmation(ConfirmationSpec::new(
            token_id.clone(),
            token_digest.clone(),
            "retention_delete",
            sid_hash.clone(),
            "artifact_replica",
            recovery_point_id.as_str(),
            "local:vault-a",
            confirmation_digest,
            490,
            1_000,
        ))
        .expect("issue retention confirmation");

    let error = repository
        .enqueue_retention_delete(
            IpcRequestSpec::new(
                IpcRequestId::new(),
                "retention.delete",
                sid_hash,
                "retention-request-mismatch",
                "5".repeat(64),
                501,
                2_000,
            ),
            &token_id,
            &token_digest,
            RetentionDeleteSpec::new(operation_run_id, WorkItemId::new(), authorization),
        )
        .expect_err("destination binding mismatch must reject delete");
    assert_eq!(error.code(), "CONFIRMATION_TOKEN_INVALID_OR_REPLAYED");
    assert_eq!(
        repository
            .confirmation_state(&token_id)
            .expect("confirmation remains usable"),
        "issued"
    );

    let direct = Connection::open(&fixture.db_path).expect("open rollback probe");
    let counts: (i64, i64, i64) = direct
        .query_row(
            "SELECT
                 (SELECT COUNT(*) FROM ipc_request_ledger),
                 (SELECT COUNT(*) FROM operation_runs),
                 (SELECT COUNT(*) FROM work_items)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("query rollback counts");
    assert_eq!(counts, (0, 1, 0));
}

#[test]
fn remote_retention_work_persists_the_confirmed_delete_secret_version() {
    let fixture = Fixture::new_s3();
    let repository = fixture.store.ipc();
    let token_id = ConfirmationTokenId::new();
    let token_digest = "d".repeat(64);
    let sid_hash = "e".repeat(64);
    let recovery_point_id = RecoveryPointId::new();
    let destination_fingerprint = fixture.destination_fingerprint.clone();
    let delete_secret_ref_id = fixture
        .delete_secret_ref_id
        .clone()
        .expect("remote delete secret");
    fixture.seed_retention_replica(&recovery_point_id);
    let operation_run_id = OperationRunId::new();
    repository
        .stage_retention_tombstone(RetentionTombstoneSpec::new(
            operation_run_id.clone(),
            recovery_point_id.clone(),
            &destination_fingerprint,
            300,
            200,
        ))
        .expect("stage remote retention tombstone");
    let authorization = fixture.retention_authorization(&recovery_point_id, 500, 300);
    let confirmation_digest = authorization.digest_blake3().to_owned();
    repository
        .issue_confirmation(ConfirmationSpec::new(
            token_id.clone(),
            token_digest.clone(),
            "retention_delete",
            sid_hash.clone(),
            "artifact_replica",
            recovery_point_id.as_str(),
            &destination_fingerprint,
            confirmation_digest,
            490,
            1_000,
        ))
        .expect("issue remote retention confirmation");
    repository
        .enqueue_retention_delete(
            IpcRequestSpec::new(
                IpcRequestId::new(),
                "retention.delete",
                sid_hash,
                "remote-retention-request",
                "6".repeat(64),
                501,
                2_000,
            ),
            &token_id,
            &token_digest,
            RetentionDeleteSpec::new(operation_run_id.clone(), WorkItemId::new(), authorization),
        )
        .expect("enqueue remote retention");

    let direct = Connection::open(&fixture.db_path).expect("open operation request");
    let request_json: String = direct
        .query_row(
            "SELECT request_json FROM operation_runs WHERE id = ?1",
            [operation_run_id.as_str()],
            |row| row.get(0),
        )
        .expect("remote operation request");
    let request: serde_json::Value = serde_json::from_str(&request_json).expect("request JSON");
    assert_eq!(
        request["delete_secret_ref_id"],
        delete_secret_ref_id.as_str()
    );
    assert_eq!(
        request["authorization"]["plan"]["delete_secret_ref_id"],
        delete_secret_ref_id.as_str()
    );
}

struct Fixture {
    _temp_dir: TempDir,
    db_path: std::path::PathBuf,
    store: OperationalStore,
    connection_id: ConnectionProfileId,
    target_id: DatabaseTargetId,
    revision_id: PlanRevisionId,
    destination_fingerprint: String,
    destination_kind: String,
    delete_secret_ref_id: Option<SecretRefId>,
}

impl Fixture {
    fn new() -> Self {
        Self::with_destination("local")
    }

    fn new_s3() -> Self {
        Self::with_destination("s3")
    }

    fn with_destination(destination_kind: &str) -> Self {
        let temp_dir = TempDir::new().expect("temp directory");
        let db_path = temp_dir.path().join("operational.db");
        let store = OperationalStore::open(db_path.clone()).expect("open operational store");
        let configuration = store.configuration();
        let connection_id = ConnectionProfileId::new();
        let target_id = DatabaseTargetId::new();
        let destination_id = DestinationId::new();
        let destination_fingerprint = format!("{destination_kind}:{}", destination_id.as_str());
        let revision_id = PlanRevisionId::new();
        configuration
            .insert_connection(ConnectionProfileSpec::tcp(
                connection_id.clone(),
                "restore-target",
                "postgresql",
                "db.internal",
                5432,
                50,
            ))
            .expect("insert target connection");
        configuration
            .insert_target(DatabaseTargetSpec::new(
                target_id.clone(),
                connection_id.clone(),
                "orders",
                "orders",
                "orders",
                "source:orders",
                2,
            ))
            .expect("insert retention target");
        let destination = match destination_kind {
            "local" => DestinationSpec::local(
                destination_id.clone(),
                "retention-vault",
                r#"{"path":"C:\\fixture-vault"}"#,
                3,
            ),
            "s3" => DestinationSpec::s3(
                destination_id.clone(),
                "retention-bucket",
                r#"{"bucket":"fixture-vault"}"#,
                3,
            ),
            _ => panic!("unsupported fixture destination"),
        };
        configuration
            .insert_destination(destination)
            .expect("insert retention destination");
        let delete_secret_ref_id = if destination_kind == "s3" {
            let secret_ref_id = SecretRefId::new();
            store
                .secret_refs()
                .create(SecretRefSpec::windows_dpapi(
                    secret_ref_id.clone(),
                    "fixture:retention-delete",
                    "retention_delete",
                    1,
                    "f".repeat(64),
                    None,
                    3,
                ))
                .expect("create delete secret reference");
            configuration
                .bind_destination_secret(DestinationSecretBindingSpec::new(
                    destination_id.clone(),
                    DestinationSecretRoleV2::DeleteOperator,
                    secret_ref_id.clone(),
                    4,
                ))
                .expect("bind delete secret");
            Some(secret_ref_id)
        } else {
            None
        };
        configuration
            .insert_plan_draft(PlanDraftSpec::manual(
                BackupPlanId::new(),
                revision_id.clone(),
                "orders-retention",
                1,
                "a".repeat(64),
                4,
            ))
            .expect("insert retention plan");
        configuration
            .add_revision_target(&revision_id, &target_id, 5)
            .expect("add retention target");
        configuration
            .add_revision_destination(
                &revision_id,
                &destination_id,
                DestinationRequirement::Required,
                6,
            )
            .expect("add retention destination");
        {
            let direct = Connection::open(&db_path).expect("open retention configuration");
            direct
                .execute(
                    "UPDATE backup_plan_revisions
                     SET retention_policy_json = '{\"daily\":1,\"weekly\":0,\"monthly\":0}'
                     WHERE id = ?1",
                    [revision_id.as_str()],
                )
                .expect("set retention policy");
            direct
                .execute(
                    "UPDATE destinations SET state = 'ready' WHERE id = ?1",
                    [destination_id.as_str()],
                )
                .expect("mark destination ready");
        }
        configuration
            .publish_revision(&revision_id, 7)
            .expect("publish retention plan");
        Self {
            _temp_dir: temp_dir,
            db_path,
            store,
            connection_id,
            target_id,
            revision_id,
            destination_fingerprint,
            destination_kind: destination_kind.to_owned(),
            delete_secret_ref_id,
        }
    }

    fn seed_retention_replica(&self, recovery_point_id: &RecoveryPointId) {
        let connection = Connection::open(&self.db_path).expect("open retention seed");
        connection
            .execute_batch("PRAGMA foreign_keys=ON;")
            .expect("enable foreign keys");
        seed_authoritative_recovery_point(
            &connection,
            &self.revision_id,
            &self.target_id,
            &self.destination_fingerprint,
            &self.destination_kind,
            recovery_point_id,
            100,
            true,
        );
        seed_authoritative_recovery_point(
            &connection,
            &self.revision_id,
            &self.target_id,
            &self.destination_fingerprint,
            &self.destination_kind,
            &RecoveryPointId::new(),
            150,
            false,
        );
    }

    fn retention_authorization(
        &self,
        recovery_point_id: &RecoveryPointId,
        evaluated_at_ms: i64,
        grace_period_ms: i64,
    ) -> RetentionDeleteAuthorizationV2 {
        self.store
            .retention_planner()
            .prepare_delete_plan(
                recovery_point_id,
                &self.destination_fingerprint,
                evaluated_at_ms,
                grace_period_ms,
            )
            .expect("authoritative retention plan")
            .authorization()
            .clone()
    }
}

#[allow(clippy::too_many_arguments)]
fn seed_authoritative_recovery_point(
    connection: &Connection,
    revision_id: &PlanRevisionId,
    target_id: &DatabaseTargetId,
    destination_fingerprint: &str,
    destination_kind: &str,
    recovery_point_id: &RecoveryPointId,
    committed_at_ms: i64,
    include_objects: bool,
) {
    let occurrence_id = PlanOccurrenceId::new();
    let backup_run_id = Uuid::new_v4().to_string();
    let replica_id = Uuid::new_v4().to_string();
    connection
        .execute(
            "INSERT INTO plan_occurrences(
                 id, plan_revision_id, origin, requested_at_ms,
                 request_idempotency_key, state, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, 'manual', ?3, ?4, 'succeeded', ?3, ?3)",
            params![
                occurrence_id.as_str(),
                revision_id.as_str(),
                committed_at_ms,
                format!("manual:{}", occurrence_id.as_str())
            ],
        )
        .expect("insert occurrence");
    connection
        .execute(
            "INSERT INTO backup_runs(
                 id, plan_occurrence_id, plan_revision_id, database_target_id,
                 state, consistency_grade, engine, source_fingerprint,
                 started_at_ms, completed_at_ms, recovery_point_id,
                 created_at_ms, updated_at_ms
             ) VALUES (
                 ?1, ?2, ?3, ?4,
                 'succeeded', 'transaction_consistent', 'postgresql', 'source:orders',
                 ?5, ?5, ?6, ?5, ?5
             )",
            params![
                backup_run_id,
                occurrence_id.as_str(),
                revision_id.as_str(),
                target_id.as_str(),
                committed_at_ms,
                recovery_point_id.as_str()
            ],
        )
        .expect("insert backup run");
    connection
        .execute(
            "INSERT INTO catalog_recovery_points(
                 id, origin_instance_id, origin_occurrence_id, origin_backup_run_id,
                 source_target_identity, source_display_name, engine, state,
                 consistency_grade, committed_at_ms, manifest_schema_version,
                 set_manifest_checksum_blake3, signing_key_id, signature_status,
                 required_part_count, present_part_count, engine_metadata_json,
                 first_ingested_at_ms, last_ingested_at_ms
             ) VALUES (
                 ?1, 'fixture-instance', ?2, ?3,
                 'source:orders', 'orders', 'postgresql', 'complete',
                 'transaction_consistent', ?4, 2,
                 ?5, 'fixture-key', 'valid',
                 1, 1, '{}', ?4, ?4
             )",
            params![
                recovery_point_id.as_str(),
                occurrence_id.as_str(),
                backup_run_id,
                committed_at_ms,
                "a".repeat(64)
            ],
        )
        .expect("insert recovery point");
    connection
        .execute(
            "INSERT INTO catalog_artifact_replicas(
                 id, recovery_point_id, destination_fingerprint,
                 destination_kind, requirement, state,
                 immutability_capability, reader_status, committed_at_ms,
                 last_observed_at_ms, metadata_json
             ) VALUES (
                 ?1, ?2, ?3,
                 ?4, 'required', 'committed',
                 'none', 'eligible', ?5, ?5, '{}'
             )",
            params![
                replica_id,
                recovery_point_id.as_str(),
                destination_fingerprint,
                destination_kind,
                committed_at_ms
            ],
        )
        .expect("insert artifact replica");
    connection
        .execute(
            "INSERT INTO catalog_snapshot_projection(
                 recovery_point_id, source_target_identity, source_display_name,
                 engine, committed_at_ms, artifact_completeness, replica_durability,
                 protection_compliance, restore_eligibility, search_text,
                 projection_version, projected_at_ms
             ) VALUES (
                 ?1, 'source:orders', 'orders',
                 'postgresql', ?2, 'complete', 'healthy',
                 'compliant', 'eligible', 'orders',
                 1, ?2
             )",
            params![recovery_point_id.as_str(), committed_at_ms],
        )
        .expect("insert snapshot projection");
    if !include_objects {
        return;
    }
    let part_id = Uuid::new_v4().to_string();
    connection
        .execute(
            "INSERT INTO catalog_artifact_parts(
                 id, recovery_point_id, part_role, logical_name, required,
                 dump_format, compression_format, encryption_format,
                 metadata_json, created_at_ms
             ) VALUES (
                 ?1, ?2, 'database_dump', 'orders', 1,
                 'custom', 'zstd', 'age', '{}', ?3
             )",
            params![part_id, recovery_point_id.as_str(), committed_at_ms],
        )
        .expect("insert artifact part");
    connection
        .execute(
            "INSERT INTO catalog_stored_objects(
                 id, recovery_point_id, artifact_replica_id, artifact_part_id,
                 object_role, object_key, object_version, checksum_blake3, size_bytes,
                 state, observed_at_ms, metadata_json
             ) VALUES (
                 ?1, ?2, ?3, ?4,
                 'payload', 'parts/orders.enc', 'payload-version', ?5, 5,
                 'present', ?6, '{}'
             )",
            params![
                Uuid::new_v4().to_string(),
                recovery_point_id.as_str(),
                replica_id,
                part_id,
                "b".repeat(64),
                committed_at_ms
            ],
        )
        .expect("insert payload object");
    connection
        .execute(
            "INSERT INTO catalog_stored_objects(
                 id, recovery_point_id, artifact_replica_id,
                 object_role, object_key, object_version, checksum_blake3, size_bytes,
                 state, observed_at_ms, metadata_json
             ) VALUES (
                 ?1, ?2, ?3,
                 'set_manifest', 'set-manifest.json', 'manifest-version', ?4, 5,
                 'present', ?5, '{}'
             )",
            params![
                Uuid::new_v4().to_string(),
                recovery_point_id.as_str(),
                replica_id,
                "c".repeat(64),
                committed_at_ms
            ],
        )
        .expect("insert set manifest");
}
