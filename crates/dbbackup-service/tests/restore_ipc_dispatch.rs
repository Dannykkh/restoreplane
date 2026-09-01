use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use dbbackup_application::{
    ApplicationCommand, ApplicationError, ApplicationRequest, ApplicationResponse,
    ApplicationUseCases, AuthoritySnapshot, CutoverMode, ExecutionAuthority,
};
use dbbackup_catalog::{
    rusqlite::{params, Connection},
    CertificationCapability, ConfigCutoverSpec, ConfirmationSpec, DatabaseTargetSpec,
    DestinationSpec, EngineCertificationSpec, G1EvidenceBindingV1, IpcRequestSpec,
    OperationalStore, PlanDraftSpec,
};
use dbbackup_config::{
    CanonicalConnection, CanonicalDestination, CanonicalPlan, CanonicalTarget,
    CanonicalV1ShadowModel,
};
use dbbackup_core::ProviderCapability;
use dbbackup_domain::{
    ArtifactReplicaId, BackupPlanId, ConfirmationTokenId, ConnectionProfileId, DatabaseTargetId,
    DestinationId, DestinationRequirement, IpcRequestId, PlanRevisionId, RecoveryPointId,
};
use dbbackup_service::ipc::{
    authorize_and_decode, encode_request, AuthorizationPolicy, DurableIpcDispatcher,
    RequestEnvelope, TokenIdentity, WindowsSid,
};
use serde_json::json;
use tempfile::TempDir;
use uuid::Uuid;

const SERVICE_SID: &str = "S-1-5-80-1001";
const OWNER_SID: &str = "S-1-5-21-100-200-300-1001";
const OWNER_LOGON_SID: &str = "S-1-5-5-10-20";
const ADMIN_SID: &str = "S-1-5-21-100-200-300-500";
const SOURCE_CHECKSUM: &str = "1111111111111111111111111111111111111111111111111111111111111111";

#[test]
fn manual_backup_dispatch_materializes_durable_work_and_replays_atomically() {
    let temp = TempDir::new().expect("temp directory");
    let database_path = temp.path().join("operational.db");
    let store = OperationalStore::open(database_path.clone()).expect("open operational store");
    commit_cutover(&store);
    let plan_id = seed_two_target_plan(&store, &database_path);
    let dispatcher = DurableIpcDispatcher::new(
        store.ipc(),
        Arc::new(RejectUnexpectedUseCases),
        AuthoritySnapshot::new(
            ExecutionAuthority::Service,
            CutoverMode::Production,
            true,
            true,
        ),
    );
    let request = backup_frame("manual-backup-01", &plan_id);

    let first = dispatcher
        .dispatch(authorized_at(&request, 3_000))
        .expect("enqueue authoritative manual backup");
    assert!(first.is_success());
    assert!(!first.deduplicated());
    assert_eq!(first.body()["body"]["schemaVersion"], 1);
    assert_eq!(first.body()["body"]["planId"], plan_id.as_str());
    assert_eq!(first.body()["body"]["state"], "queued");
    let work_items = first.body()["body"]["workItems"]
        .as_array()
        .expect("target work items");
    assert_eq!(work_items.len(), 2);
    let mut target_names = work_items
        .iter()
        .map(|item| {
            assert_eq!(item["engine"], "postgresql");
            assert_eq!(item["state"], "queued");
            item["targetName"].as_str().expect("target name").to_owned()
        })
        .collect::<Vec<_>>();
    target_names.sort();
    assert_eq!(target_names, ["billing", "orders"]);

    let replay = dispatcher
        .dispatch(authorized_at(&request, 3_001))
        .expect("replay committed manual backup");
    assert!(replay.is_success());
    assert!(replay.deduplicated());
    assert_eq!(replay.body(), first.body());

    let conflict = backup_frame("manual-backup-01", &BackupPlanId::new());
    let conflict_error = dispatcher
        .dispatch(authorized_at(&conflict, 3_002))
        .expect_err("same key with another plan must conflict");
    assert_eq!(conflict_error.code(), "IPC_IDEMPOTENCY_CONFLICT");

    let direct = Connection::open(database_path).expect("open backup assertions");
    let counts: (i64, i64, i64, i64, i64) = direct
        .query_row(
            "SELECT
                 (SELECT COUNT(*) FROM ipc_request_ledger
                  WHERE command_name = 'backup.enqueue'),
                 (SELECT COUNT(*) FROM plan_occurrences
                  WHERE origin = 'manual' AND state = 'enqueued'),
                 (SELECT COUNT(*) FROM backup_runs WHERE state = 'queued'),
                 (SELECT COUNT(*) FROM work_items
                  WHERE kind = 'backup' AND state = 'queued'),
                 (SELECT COUNT(*) FROM activity_events
                  WHERE event_code = 'BACKUP_QUEUED')",
            [],
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
        .expect("query backup atomic counts");
    assert_eq!(counts, (1, 1, 2, 2, 2));
}

#[test]
fn production_restore_dispatch_rechecks_preflight_and_replays_without_reexecution() {
    let temp = TempDir::new().expect("temp directory");
    let database_path = temp.path().join("operational.db");
    let store = OperationalStore::open(database_path.clone()).expect("open operational store");
    commit_cutover(&store);
    let target_connection_id = imported_connection_id(&database_path);
    let recovery_point_id = RecoveryPointId::new();
    let source_replica_id = ArtifactReplicaId::new();
    seed_restore_truth(
        &database_path,
        &target_connection_id,
        &recovery_point_id,
        &source_replica_id,
    );

    let preflight_version = digest("preflight-version");
    let target_fingerprint = digest("target-fingerprint");
    let use_cases = Arc::new(PreflightUseCases {
        calls: AtomicUsize::new(0),
        recovery_point_id: recovery_point_id.clone(),
        source_replica_id: source_replica_id.clone(),
        target_connection_id: target_connection_id.clone(),
        preflight_version: preflight_version.clone(),
        target_fingerprint: target_fingerprint.clone(),
        mode: "create_new",
        purpose: "incident_recovery",
        target_name: "orders_restore",
        target_exists: false,
        evaluated_at_ms: 2_900,
        expires_at_ms: 10_000,
    });
    let dispatcher = DurableIpcDispatcher::new(
        store.ipc(),
        Arc::clone(&use_cases),
        AuthoritySnapshot::new(
            ExecutionAuthority::Service,
            CutoverMode::Production,
            true,
            true,
        ),
    );
    let original = frame(
        "restore-safe-01",
        &recovery_point_id,
        &source_replica_id,
        &target_connection_id,
        "orders_restore",
        &preflight_version,
    );

    let first = dispatcher
        .dispatch(authorized_at(&original, 3_000))
        .expect("dispatch safe restore");
    assert!(first.is_success());
    assert!(!first.deduplicated());
    assert_eq!(first.body()["body"]["state"], "queued");
    assert_eq!(first.body()["body"]["mode"], "create_new");
    assert_eq!(use_cases.calls.load(Ordering::SeqCst), 1);

    let replay = dispatcher
        .dispatch(authorized_at(&original, 3_001))
        .expect("replay committed safe restore");
    assert!(replay.is_success());
    assert!(replay.deduplicated());
    assert_eq!(replay.body(), first.body());
    assert_eq!(
        use_cases.calls.load(Ordering::SeqCst),
        1,
        "a completed request must replay before a new preflight"
    );

    let conflict = frame(
        "restore-safe-01",
        &recovery_point_id,
        &source_replica_id,
        &target_connection_id,
        "orders_restore_conflict",
        &preflight_version,
    );
    let conflict_error = dispatcher
        .dispatch(authorized_at(&conflict, 3_002))
        .expect_err("same key with different payload must conflict");
    assert_eq!(conflict_error.code(), "IPC_IDEMPOTENCY_CONFLICT");
    assert_eq!(use_cases.calls.load(Ordering::SeqCst), 1);

    let direct = Connection::open(database_path).expect("open dispatcher assertions");
    let counts: (i64, i64, i64, i64) = direct
        .query_row(
            "SELECT
                 (SELECT COUNT(*) FROM ipc_request_ledger
                  WHERE command_name = 'restore.enqueue_safe'),
                 (SELECT COUNT(*) FROM restore_runs),
                 (SELECT COUNT(*) FROM work_items WHERE kind = 'restore'),
                 (SELECT COUNT(*) FROM process_instances)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("query production restore counts");
    assert_eq!(counts, (1, 1, 1, 0));
}

#[test]
fn overwrite_confirmation_binds_sid_payload_expiry_and_single_execution_before_process_start() {
    let temp = TempDir::new().expect("temp directory");
    let database_path = temp.path().join("operational.db");
    let store = OperationalStore::open(database_path.clone()).expect("open operational store");
    commit_cutover(&store);
    let target_connection_id = imported_connection_id(&database_path);
    let recovery_point_id = RecoveryPointId::new();
    let source_replica_id = ArtifactReplicaId::new();
    seed_restore_truth(
        &database_path,
        &target_connection_id,
        &recovery_point_id,
        &source_replica_id,
    );
    let preflight_version = digest("overwrite-preflight-version");
    let target_fingerprint = digest("overwrite-target-fingerprint");
    let use_cases = Arc::new(PreflightUseCases {
        calls: AtomicUsize::new(0),
        recovery_point_id: recovery_point_id.clone(),
        source_replica_id: source_replica_id.clone(),
        target_connection_id: target_connection_id.clone(),
        preflight_version: preflight_version.clone(),
        target_fingerprint,
        mode: "overwrite",
        purpose: "incident_recovery",
        target_name: "orders",
        target_exists: true,
        evaluated_at_ms: 2_900,
        expires_at_ms: 10_000,
    });
    let dispatcher = DurableIpcDispatcher::new(
        store.ipc(),
        Arc::clone(&use_cases),
        AuthoritySnapshot::new(
            ExecutionAuthority::Service,
            CutoverMode::Production,
            true,
            true,
        ),
    );
    let prepare_frame = overwrite_prepare_frame(
        "restore-overwrite-prepare-01",
        &recovery_point_id,
        &source_replica_id,
        &target_connection_id,
        &preflight_version,
    );
    let prepared = dispatcher
        .dispatch(authorized_at(&prepare_frame, 3_000))
        .expect("prepare overwrite confirmation");
    assert!(prepared.is_success());
    assert!(!prepared.deduplicated());
    let prepared_body = &prepared.body()["body"];
    assert_eq!(prepared_body["tokenAvailable"], true);
    assert!(prepared_body["confirmationToken"].as_str().is_some());
    assert_eq!(prepared_body["finalSummary"]["target"]["name"], "orders");
    let confirmation_token_id = prepared_body["confirmationTokenId"]
        .as_str()
        .expect("confirmation token id")
        .to_owned();
    let overwrite_request = prepared_body["overwriteRequest"].clone();

    let replayed_prepare = dispatcher
        .dispatch(authorized_at(&prepare_frame, 3_001))
        .expect("replay prepared confirmation");
    assert!(replayed_prepare.is_success());
    assert!(replayed_prepare.deduplicated());
    assert_eq!(replayed_prepare.body()["body"]["tokenAvailable"], false);
    assert!(replayed_prepare.body()["body"]["confirmationToken"].is_null());
    assert!(replayed_prepare.body()["body"]["overwriteRequest"].is_null());

    let direct = Connection::open(&database_path).expect("open overwrite assertions");
    let persisted_prepare: String = direct
        .query_row(
            "SELECT response_json FROM ipc_request_ledger
             WHERE command_name = 'confirmation.prepare'",
            [],
            |row| row.get(0),
        )
        .expect("persisted prepare response");
    assert!(
        !persisted_prepare.contains(
            overwrite_request["confirmationToken"]
                .as_str()
                .expect("raw confirmation token")
        ),
        "raw tokens must never enter the durable IPC ledger"
    );

    let wrong_sid_frame =
        overwrite_execute_frame("restore-overwrite-wrong-sid", overwrite_request.clone());
    let wrong_sid = dispatcher
        .dispatch(authorized_as(
            &wrong_sid_frame,
            3_100,
            &approved_admin_identity(),
        ))
        .expect_err("another SID must not consume the token");
    assert_eq!(wrong_sid.code(), "CONFIRMATION_TOKEN_INVALID_OR_REPLAYED");

    let mut changed_payload = overwrite_request.clone();
    changed_payload["requiredVerificationLevel"] = json!(3);
    let changed_payload_frame =
        overwrite_execute_frame("restore-overwrite-payload-mismatch", changed_payload);
    let changed_payload_error = dispatcher
        .dispatch(authorized_at(&changed_payload_frame, 3_101))
        .expect_err("a changed payload must not consume the token");
    assert_eq!(
        changed_payload_error.code(),
        "CONFIRMATION_TOKEN_INVALID_OR_REPLAYED"
    );

    let expired_frame =
        overwrite_execute_frame("restore-overwrite-expired", overwrite_request.clone());
    let expired = dispatcher
        .dispatch(authorized_at(&expired_frame, 8_000))
        .expect_err("an expired token must not enqueue restore work");
    assert_eq!(expired.code(), "CONFIRMATION_TOKEN_INVALID_OR_REPLAYED");

    let counts_before_success: (i64, i64, i64, i64) = direct
        .query_row(
            "SELECT
                 (SELECT COUNT(*) FROM ipc_request_ledger
                  WHERE command_name IN ('confirmation.prepare', 'restore.enqueue_overwrite')),
                 (SELECT COUNT(*) FROM restore_runs),
                 (SELECT COUNT(*) FROM work_items WHERE kind = 'restore'),
                 (SELECT COUNT(*) FROM process_instances)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("counts before valid overwrite");
    assert_eq!(counts_before_success, (1, 0, 0, 0));

    let valid_frame =
        overwrite_execute_frame("restore-overwrite-execute-01", overwrite_request.clone());
    let accepted = dispatcher
        .dispatch(authorized_at(&valid_frame, 3_200))
        .expect("consume confirmation and enqueue overwrite");
    assert!(accepted.is_success());
    assert!(!accepted.deduplicated());
    assert_eq!(accepted.body()["body"]["mode"], "overwrite");
    assert_eq!(accepted.body()["body"]["state"], "queued");

    let replay = dispatcher
        .dispatch(authorized_at(&valid_frame, 3_201))
        .expect("same request replays the committed result");
    assert!(replay.is_success());
    assert!(replay.deduplicated());
    assert_eq!(replay.body(), accepted.body());
    let calls_after_exact_replay = use_cases.calls.load(Ordering::SeqCst);

    let consumed_replay_frame =
        overwrite_execute_frame("restore-overwrite-consumed-replay", overwrite_request);
    let consumed_replay = dispatcher
        .dispatch(authorized_at(&consumed_replay_frame, 3_202))
        .expect_err("a consumed token cannot authorize a second restore");
    assert_eq!(
        consumed_replay.code(),
        "CONFIRMATION_TOKEN_INVALID_OR_REPLAYED"
    );
    assert_eq!(
        use_cases.calls.load(Ordering::SeqCst),
        calls_after_exact_replay + 1,
        "exact IPC replay skips preflight; a new token-use attempt is rechecked"
    );

    let final_counts: (i64, i64, i64, i64, String) = direct
        .query_row(
            "SELECT
                 (SELECT COUNT(*) FROM ipc_request_ledger
                  WHERE command_name IN ('confirmation.prepare', 'restore.enqueue_overwrite')),
                 (SELECT COUNT(*) FROM restore_runs),
                 (SELECT COUNT(*) FROM work_items WHERE kind = 'restore'),
                 (SELECT COUNT(*) FROM process_instances),
                 (SELECT state FROM confirmation_tokens WHERE id = ?1)",
            [confirmation_token_id],
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
        .expect("final overwrite counts");
    assert_eq!(
        final_counts,
        (2, 1, 1, 0, "consumed".to_owned()),
        "mismatch, expiry, and consumed-token replay must start zero native processes"
    );
}

#[test]
fn restore_drill_dispatch_creates_one_bound_l3_run_without_a_second_work_item() {
    let temp = TempDir::new().expect("temp directory");
    let database_path = temp.path().join("operational.db");
    let store = OperationalStore::open(database_path.clone()).expect("open operational store");
    commit_cutover(&store);
    let target_connection_id = imported_connection_id(&database_path);
    let recovery_point_id = RecoveryPointId::new();
    let source_replica_id = ArtifactReplicaId::new();
    seed_restore_truth(
        &database_path,
        &target_connection_id,
        &recovery_point_id,
        &source_replica_id,
    );
    let preflight_version = digest("drill-preflight-version");
    let use_cases = Arc::new(PreflightUseCases {
        calls: AtomicUsize::new(0),
        recovery_point_id: recovery_point_id.clone(),
        source_replica_id: source_replica_id.clone(),
        target_connection_id: target_connection_id.clone(),
        preflight_version: preflight_version.clone(),
        target_fingerprint: digest("drill-target-fingerprint"),
        mode: "create_new",
        purpose: "restore_drill",
        target_name: "orders_drill",
        target_exists: false,
        evaluated_at_ms: 2_900,
        expires_at_ms: 10_000,
    });
    let dispatcher = DurableIpcDispatcher::new(
        store.ipc(),
        Arc::clone(&use_cases),
        AuthoritySnapshot::new(
            ExecutionAuthority::Service,
            CutoverMode::Production,
            true,
            true,
        ),
    );
    let request = restore_drill_frame(
        "restore-drill-01",
        &recovery_point_id,
        &source_replica_id,
        &target_connection_id,
        "orders_drill",
        &preflight_version,
    );

    let first = dispatcher
        .dispatch(authorized_at(&request, 3_000))
        .expect("enqueue restore drill");
    assert!(first.is_success());
    assert!(!first.deduplicated());
    assert_eq!(first.body()["body"]["purpose"], "restore_drill");
    assert!(first.body()["body"]["verificationRunId"].as_str().is_some());

    let replay = dispatcher
        .dispatch(authorized_at(&request, 3_001))
        .expect("replay restore drill");
    assert!(replay.is_success());
    assert!(replay.deduplicated());
    assert_eq!(replay.body(), first.body());
    assert_eq!(use_cases.calls.load(Ordering::SeqCst), 1);

    let direct = Connection::open(database_path).expect("open drill assertions");
    let row: (i64, i64, i64, i64, String, String, i64, String) = direct
        .query_row(
            "SELECT
                 (SELECT COUNT(*) FROM ipc_request_ledger
                  WHERE command_name = 'restore.enqueue_safe'),
                 (SELECT COUNT(*) FROM restore_runs),
                 (SELECT COUNT(*) FROM verification_runs),
                 (SELECT COUNT(*) FROM work_items),
                 restore.purpose,
                 verification.trigger_kind,
                 verification.requested_level,
                 verification.restore_run_id
             FROM restore_runs AS restore
             JOIN verification_runs AS verification
               ON verification.restore_run_id = restore.id",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )
        .expect("query drill binding");
    assert_eq!(row.0, 1);
    assert_eq!(row.1, 1);
    assert_eq!(row.2, 1);
    assert_eq!(
        row.3, 1,
        "the restore work performs the L3 drill validation"
    );
    assert_eq!(row.4, "restore_drill");
    assert_eq!(row.5, "restore_drill");
    assert_eq!(row.6, 3);
    assert_eq!(
        row.7,
        first.body()["body"]["restoreRunId"]
            .as_str()
            .expect("restore run id")
    );
}

#[test]
fn manual_verification_dispatch_preserves_exact_level_and_replays_atomically() {
    let temp = TempDir::new().expect("temp directory");
    let database_path = temp.path().join("operational.db");
    let store = OperationalStore::open(database_path.clone()).expect("open operational store");
    commit_cutover(&store);
    let target_connection_id = imported_connection_id(&database_path);
    let recovery_point_id = RecoveryPointId::new();
    let source_replica_id = ArtifactReplicaId::new();
    seed_restore_truth(
        &database_path,
        &target_connection_id,
        &recovery_point_id,
        &source_replica_id,
    );
    certify_verification_capabilities(&store);
    let dispatcher = DurableIpcDispatcher::new(
        store.ipc(),
        Arc::new(RejectUnexpectedUseCases),
        AuthoritySnapshot::new(
            ExecutionAuthority::Service,
            CutoverMode::Production,
            true,
            true,
        ),
    );

    let l2_request = verification_frame(
        "manual-verification-l2",
        &recovery_point_id,
        Some(&source_replica_id),
        2,
    );
    let l2 = dispatcher
        .dispatch(authorized_at(&l2_request, 3_000))
        .expect("enqueue exact L2 verification");
    assert!(l2.is_success());
    assert!(!l2.deduplicated());
    assert_eq!(l2.body()["body"]["requestedLevel"], 2);
    assert_eq!(l2.body()["body"]["state"], "queued");
    assert!(l2.body()["body"]["workItemId"].as_str().is_some());

    let replay = dispatcher
        .dispatch(authorized_at(&l2_request, 3_001))
        .expect("replay exact L2 request");
    assert!(replay.is_success());
    assert!(replay.deduplicated());
    assert_eq!(replay.body(), l2.body());

    let changed_level = verification_frame(
        "manual-verification-l2",
        &recovery_point_id,
        Some(&source_replica_id),
        1,
    );
    let conflict = dispatcher
        .dispatch(authorized_at(&changed_level, 3_002))
        .expect_err("same key cannot silently change level");
    assert_eq!(conflict.code(), "IPC_IDEMPOTENCY_CONFLICT");

    let l3_request = verification_frame(
        "manual-verification-l3",
        &recovery_point_id,
        Some(&source_replica_id),
        3,
    );
    let l3 = dispatcher
        .dispatch(authorized_at(&l3_request, 3_003))
        .expect("record explicit unsupported L3");
    assert!(l3.is_success());
    assert_eq!(l3.body()["body"]["requestedLevel"], 3);
    assert_eq!(l3.body()["body"]["state"], "unsupported");
    assert!(l3.body()["body"]["workItemId"].is_null());
    assert_eq!(
        l3.body()["body"]["errorCode"],
        "VERIFICATION_RESTORE_RUN_REQUIRED"
    );

    let direct = Connection::open(database_path).expect("open verification assertions");
    let counts: (i64, i64, i64, i64, i64, i64) = direct
        .query_row(
            "SELECT
                 (SELECT COUNT(*) FROM ipc_request_ledger
                  WHERE command_name = 'verification.enqueue'),
                 (SELECT COUNT(*) FROM verification_runs),
                 (SELECT COUNT(*) FROM verification_runs
                  WHERE requested_level = 2 AND state = 'queued'),
                 (SELECT COUNT(*) FROM verification_runs
                  WHERE requested_level = 3 AND state = 'unsupported'),
                 (SELECT COUNT(*) FROM work_items WHERE kind = 'verification'),
                 (SELECT COUNT(*) FROM process_instances)",
            [],
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
        .expect("verification atomic counts");
    assert_eq!(counts, (2, 2, 1, 1, 1, 0));
}

#[test]
fn replica_repair_dispatch_atomically_binds_catalog_source_target_work_and_activity() {
    let temp = TempDir::new().expect("temp directory");
    let database_path = temp.path().join("operational.db");
    let store = OperationalStore::open(database_path.clone()).expect("open operational store");
    commit_cutover(&store);
    let source_destination_id = DestinationId::new();
    let target_destination_id = DestinationId::new();
    store
        .configuration()
        .insert_destination(DestinationSpec::local(
            source_destination_id.clone(),
            "Repair source",
            r#"{"path":"D:\\repair-source"}"#,
            2_100,
        ))
        .expect("insert source destination");
    store
        .configuration()
        .insert_destination(DestinationSpec::local(
            target_destination_id.clone(),
            "Repair target",
            r#"{"path":"D:\\repair-target"}"#,
            2_101,
        ))
        .expect("insert target destination");
    let direct = Connection::open(&database_path).expect("open repair fixture");
    direct
        .execute(
            "UPDATE destinations SET state = 'ready', updated_at_ms = 2200
             WHERE id IN (?1, ?2)",
            params![
                source_destination_id.as_str(),
                target_destination_id.as_str()
            ],
        )
        .expect("mark repair destinations ready");
    let recovery_point_id = RecoveryPointId::new();
    let source_replica_id = ArtifactReplicaId::new();
    seed_repair_truth(
        &direct,
        &recovery_point_id,
        &source_replica_id,
        &source_destination_id,
    );
    let dispatcher = DurableIpcDispatcher::new(
        store.ipc(),
        Arc::new(RejectUnexpectedUseCases),
        AuthoritySnapshot::new(
            ExecutionAuthority::Service,
            CutoverMode::Production,
            true,
            true,
        ),
    );
    let request = repair_frame(
        "repair-request-01",
        &recovery_point_id,
        &source_replica_id,
        &target_destination_id,
    );
    let first = dispatcher
        .dispatch(authorized_at(&request, 3_000))
        .expect("enqueue authoritative repair");
    assert!(first.is_success());
    assert!(!first.deduplicated());
    assert_eq!(first.body()["body"]["operationKind"], "replica_repair");
    assert_eq!(first.body()["body"]["state"], "queued");

    let replay = dispatcher
        .dispatch(authorized_at(&request, 3_001))
        .expect("replay repair request");
    assert!(replay.is_success());
    assert!(replay.deduplicated());
    assert_eq!(replay.body(), first.body());

    let conflict = repair_frame(
        "repair-request-01",
        &recovery_point_id,
        &source_replica_id,
        &source_destination_id,
    );
    let conflict_error = dispatcher
        .dispatch(authorized_at(&conflict, 3_002))
        .expect_err("same key with changed target must conflict");
    assert_eq!(conflict_error.code(), "IPC_IDEMPOTENCY_CONFLICT");

    let counts: (i64, i64, i64, i64, i64) = direct
        .query_row(
            "SELECT
                 (SELECT COUNT(*) FROM ipc_request_ledger
                  WHERE command_name = 'repair.enqueue'),
                 (SELECT COUNT(*) FROM operation_runs
                  WHERE operation_kind = 'replica_repair'),
                 (SELECT COUNT(*) FROM work_items WHERE kind = 'operation'),
                 (SELECT COUNT(*) FROM process_instances),
                 (SELECT COUNT(*) FROM activity_events
                  WHERE event_code = 'REPLICA_REPAIR_QUEUED')",
            [],
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
        .expect("repair atomic counts");
    assert_eq!(counts, (1, 1, 1, 0, 1));
    let request_json: String = direct
        .query_row(
            "SELECT request_json FROM operation_runs
             WHERE operation_kind = 'replica_repair'",
            [],
            |row| row.get(0),
        )
        .expect("repair request JSON");
    let pinned: serde_json::Value =
        serde_json::from_str(&request_json).expect("parse repair request");
    assert!(pinned["source_reader_secret_ref_id"].is_null());
    assert!(pinned["target_writer_secret_ref_id"].is_null());
    assert!(pinned["target_reader_secret_ref_id"].is_null());
}

struct RejectUnexpectedUseCases;

impl ApplicationUseCases for RejectUnexpectedUseCases {
    fn execute(
        &self,
        request: ApplicationRequest,
    ) -> Result<ApplicationResponse, ApplicationError> {
        panic!("typed repair dispatch must not invoke generic use cases: {request:?}")
    }
}

struct PreflightUseCases {
    calls: AtomicUsize,
    recovery_point_id: RecoveryPointId,
    source_replica_id: ArtifactReplicaId,
    target_connection_id: ConnectionProfileId,
    preflight_version: String,
    target_fingerprint: String,
    mode: &'static str,
    purpose: &'static str,
    target_name: &'static str,
    target_exists: bool,
    evaluated_at_ms: i64,
    expires_at_ms: i64,
}

impl ApplicationUseCases for PreflightUseCases {
    fn execute(
        &self,
        request: ApplicationRequest,
    ) -> Result<ApplicationResponse, ApplicationError> {
        assert_eq!(request.command, ApplicationCommand::RestorePreflight);
        assert_eq!(
            request.payload["snapshotId"],
            self.recovery_point_id.as_str()
        );
        assert_eq!(
            request.payload["sourceReplicaId"],
            self.source_replica_id.as_str()
        );
        assert_eq!(
            request.payload["targetConnectionId"],
            self.target_connection_id.as_str()
        );
        assert_eq!(request.payload["mode"], self.mode);
        assert_eq!(request.payload["purpose"], self.purpose);
        assert_eq!(request.payload["targetName"], self.target_name);
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ApplicationResponse {
            deduplicated: false,
            resource: None,
            body: json!({
                "eligible": true,
                "mode": self.mode,
                "purpose": self.purpose,
                "preflightVersion": self.preflight_version,
                "evaluatedAtMs": self.evaluated_at_ms,
                "expiresAtMs": self.expires_at_ms,
                "targetFingerprint": self.target_fingerprint,
                "targetConnectionId": self.target_connection_id.as_str(),
                "targetName": self.target_name,
                "targetExists": self.target_exists,
                "restoreTestTarget": self.purpose == "restore_drill",
                "targetRole": if self.purpose == "restore_drill" {
                    "restore_test"
                } else {
                    "general"
                },
                "source": {
                    "snapshotId": self.recovery_point_id.as_str(),
                    "replicaId": self.source_replica_id.as_str()
                }
            }),
        })
    }
}

fn frame(
    idempotency_key: &str,
    recovery_point_id: &RecoveryPointId,
    source_replica_id: &ArtifactReplicaId,
    target_connection_id: &ConnectionProfileId,
    target_name: &str,
    preflight_version: &str,
) -> Vec<u8> {
    encode_request(
        &RequestEnvelope::new(
            IpcRequestId::new(),
            ApplicationCommand::RestoreEnqueueSafe,
            Some(idempotency_key.to_owned()),
            2_900,
            10_000,
            json!({
                "snapshotId": recovery_point_id.as_str(),
                "sourceReplicaId": source_replica_id.as_str(),
                "targetConnectionId": target_connection_id.as_str(),
                "targetName": target_name,
                "preflightVersion": preflight_version,
                "requiredVerificationLevel": 2
            }),
        )
        .expect("safe restore request"),
    )
    .expect("encode safe restore request")
}

fn backup_frame(idempotency_key: &str, plan_id: &BackupPlanId) -> Vec<u8> {
    encode_request(
        &RequestEnvelope::new(
            IpcRequestId::new(),
            ApplicationCommand::BackupEnqueue,
            Some(idempotency_key.to_owned()),
            2_900,
            10_000,
            json!({
                "planId": plan_id.as_str(),
            }),
        )
        .expect("manual backup request"),
    )
    .expect("encode manual backup request")
}

fn restore_drill_frame(
    idempotency_key: &str,
    recovery_point_id: &RecoveryPointId,
    source_replica_id: &ArtifactReplicaId,
    target_connection_id: &ConnectionProfileId,
    target_name: &str,
    preflight_version: &str,
) -> Vec<u8> {
    encode_request(
        &RequestEnvelope::new(
            IpcRequestId::new(),
            ApplicationCommand::RestoreEnqueueSafe,
            Some(idempotency_key.to_owned()),
            2_900,
            10_000,
            json!({
                "snapshotId": recovery_point_id.as_str(),
                "sourceReplicaId": source_replica_id.as_str(),
                "targetConnectionId": target_connection_id.as_str(),
                "targetName": target_name,
                "preflightVersion": preflight_version,
                "requiredVerificationLevel": 3,
                "purpose": "restore_drill"
            }),
        )
        .expect("restore drill request"),
    )
    .expect("encode restore drill request")
}

fn overwrite_prepare_frame(
    idempotency_key: &str,
    recovery_point_id: &RecoveryPointId,
    source_replica_id: &ArtifactReplicaId,
    target_connection_id: &ConnectionProfileId,
    preflight_version: &str,
) -> Vec<u8> {
    encode_request(
        &RequestEnvelope::new(
            IpcRequestId::new(),
            ApplicationCommand::ConfirmationPrepare,
            Some(idempotency_key.to_owned()),
            2_900,
            20_000,
            json!({
                "action": "restore_overwrite",
                "snapshotId": recovery_point_id.as_str(),
                "sourceReplicaId": source_replica_id.as_str(),
                "targetConnectionId": target_connection_id.as_str(),
                "targetName": "orders",
                "confirmedTargetName": "orders",
                "preflightVersion": preflight_version,
                "requiredVerificationLevel": 2,
                "ttlMs": 5_000,
            }),
        )
        .expect("overwrite confirmation request"),
    )
    .expect("encode overwrite confirmation request")
}

fn overwrite_execute_frame(idempotency_key: &str, payload: serde_json::Value) -> Vec<u8> {
    encode_request(
        &RequestEnvelope::new(
            IpcRequestId::new(),
            ApplicationCommand::RestoreEnqueueOverwrite,
            Some(idempotency_key.to_owned()),
            2_900,
            20_000,
            payload,
        )
        .expect("overwrite execute request"),
    )
    .expect("encode overwrite execute request")
}

fn repair_frame(
    idempotency_key: &str,
    recovery_point_id: &RecoveryPointId,
    source_replica_id: &ArtifactReplicaId,
    target_destination_id: &DestinationId,
) -> Vec<u8> {
    encode_request(
        &RequestEnvelope::new(
            IpcRequestId::new(),
            ApplicationCommand::RepairEnqueue,
            Some(idempotency_key.to_owned()),
            2_900,
            20_000,
            json!({
                "snapshotId": recovery_point_id.as_str(),
                "sourceReplicaId": source_replica_id.as_str(),
                "targetDestinationId": target_destination_id.as_str(),
            }),
        )
        .expect("repair request"),
    )
    .expect("encode repair request")
}

fn verification_frame(
    idempotency_key: &str,
    recovery_point_id: &RecoveryPointId,
    source_replica_id: Option<&ArtifactReplicaId>,
    level: i64,
) -> Vec<u8> {
    encode_request(
        &RequestEnvelope::new(
            IpcRequestId::new(),
            ApplicationCommand::VerificationEnqueue,
            Some(idempotency_key.to_owned()),
            2_900,
            20_000,
            json!({
                "snapshotId": recovery_point_id.as_str(),
                "sourceReplicaId": source_replica_id.map(ArtifactReplicaId::as_str),
                "level": level,
                "purpose": "manual",
            }),
        )
        .expect("manual verification request"),
    )
    .expect("encode manual verification request")
}

fn authorized_at(frame: &[u8], accepted_at_ms: i64) -> dbbackup_service::ipc::AuthorizedRequest {
    authorize_and_decode(&policy(), &owner_identity(), frame, accepted_at_ms)
        .expect("authorize safe restore request")
}

fn authorized_as(
    frame: &[u8],
    accepted_at_ms: i64,
    identity: &TokenIdentity,
) -> dbbackup_service::ipc::AuthorizedRequest {
    authorize_and_decode(&policy(), identity, frame, accepted_at_ms)
        .expect("authorize restore request")
}

fn policy() -> AuthorizationPolicy {
    AuthorizationPolicy::new(
        WindowsSid::parse(SERVICE_SID).expect("service SID"),
        WindowsSid::parse(OWNER_SID).expect("owner SID"),
        WindowsSid::parse(OWNER_LOGON_SID).expect("owner logon SID"),
        10,
        vec![WindowsSid::parse(ADMIN_SID).expect("approved administrator SID")],
    )
    .expect("authorization policy")
}

fn owner_identity() -> TokenIdentity {
    TokenIdentity::new(
        WindowsSid::parse(OWNER_SID).expect("owner SID"),
        WindowsSid::parse(OWNER_LOGON_SID).expect("owner logon SID"),
        10,
        false,
        true,
    )
}

fn approved_admin_identity() -> TokenIdentity {
    TokenIdentity::new(
        WindowsSid::parse(ADMIN_SID).expect("administrator SID"),
        WindowsSid::parse("S-1-5-5-30-40").expect("administrator logon SID"),
        30,
        true,
        true,
    )
}

fn commit_cutover(store: &OperationalStore) {
    let report = store
        .shadow_import()
        .import_v1(shadow_model(), 1_000)
        .expect("shadow import");
    let cutover = ConfigCutoverSpec::new(
        Uuid::new_v4(),
        Uuid::parse_str(report.import_run_id()).expect("shadow import UUID"),
        SOURCE_CHECKSUM,
        report.relation_digest_blake3(),
        digest("recovery-bundle"),
        digest("scheduler-freeze"),
        digest("service-health"),
        G1EvidenceBindingV1::new(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            digest("source-tree"),
            digest("build"),
            digest("g1-evidence"),
        ),
        2_001,
    );
    let token_id = ConfirmationTokenId::new();
    let token_digest = digest(token_id.as_str());
    let sid_hash = digest("cutover-owner");
    let payload_digest = digest("cutover-payload");
    store
        .ipc()
        .issue_confirmation(ConfirmationSpec::new(
            token_id.clone(),
            token_digest.clone(),
            "config_cutover",
            &sid_hash,
            cutover.confirmation_resource_type(),
            report.import_run_id(),
            cutover.cutover_marker(),
            &payload_digest,
            1_900,
            10_000,
        ))
        .expect("issue cutover confirmation");
    store
        .ipc()
        .commit_config_cutover(
            IpcRequestSpec::new(
                IpcRequestId::new(),
                "config.cutover",
                sid_hash,
                "restore-test-cutover",
                payload_digest,
                2_000,
                10_000,
            ),
            &token_id,
            &token_digest,
            cutover,
        )
        .expect("commit cutover");
}

fn imported_connection_id(database_path: &std::path::Path) -> ConnectionProfileId {
    let connection = Connection::open(database_path).expect("open imported configuration");
    let raw: String = connection
        .query_row(
            "SELECT id FROM connection_profiles WHERE engine = 'postgresql' LIMIT 1",
            [],
            |row| row.get(0),
        )
        .expect("imported PostgreSQL connection");
    ConnectionProfileId::from_str(&raw).expect("connection profile ID")
}

fn seed_two_target_plan(store: &OperationalStore, database_path: &std::path::Path) -> BackupPlanId {
    let connection = Connection::open(database_path).expect("open imported configuration");
    let (connection_id, first_target_id, destination_id): (String, String, String) = connection
        .query_row(
            "SELECT connection.id, target.id, destination.id
             FROM connection_profiles AS connection
             JOIN database_targets AS target
               ON target.connection_profile_id = connection.id
             CROSS JOIN destinations AS destination
             WHERE connection.engine = 'postgresql'
             LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("imported plan dependencies");
    drop(connection);
    let connection_id =
        ConnectionProfileId::from_str(&connection_id).expect("connection profile ID");
    let first_target_id = DatabaseTargetId::from_str(&first_target_id).expect("first target ID");
    let destination_id = DestinationId::from_str(&destination_id).expect("destination ID");
    let second_target_id = DatabaseTargetId::new();
    let plan_id = BackupPlanId::new();
    let revision_id = PlanRevisionId::new();
    let repository = store.configuration();
    repository
        .insert_target(DatabaseTargetSpec::new(
            second_target_id.clone(),
            connection_id,
            "billing",
            "billing",
            "billing",
            digest("billing-source-fingerprint"),
            2_100,
        ))
        .expect("insert second target");
    repository
        .insert_plan_draft_with_membership(
            PlanDraftSpec::manual(
                plan_id.clone(),
                revision_id.clone(),
                "operations-databases",
                1,
                digest("operations-plan-content"),
                2_200,
            ),
            vec![first_target_id, second_target_id],
            vec![(destination_id, DestinationRequirement::Required)],
        )
        .expect("insert two-target plan");
    repository
        .publish_revision(&revision_id, 2_300)
        .expect("publish two-target plan");
    plan_id
}

fn seed_restore_truth(
    database_path: &std::path::Path,
    target_connection_id: &ConnectionProfileId,
    recovery_point_id: &RecoveryPointId,
    source_replica_id: &ArtifactReplicaId,
) {
    let connection = Connection::open(database_path).expect("open restore fixture");
    connection
        .execute_batch("PRAGMA foreign_keys=ON;")
        .expect("enable foreign keys");
    connection
        .execute(
            "UPDATE connection_profiles
             SET state = 'ready', updated_at_ms = 2100
             WHERE id = ?1",
            [target_connection_id.as_str()],
        )
        .expect("mark target ready");
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
                 'postgresql', 'complete', 'application_consistent', 2200,
                 2, ?2, 'signing-key-01', 'valid',
                 1, 1, '{}', 2200, 2200
             )",
            params![recovery_point_id.as_str(), digest("manifest")],
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
                 'none', 'eligible', 2200, 2200, '{}'
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
                 'postgresql', 2200, 'complete', 'healthy',
                 'compliant', 'eligible', 'orders',
                 1, 2200
             )",
            [recovery_point_id.as_str()],
        )
        .expect("insert snapshot projection");
}

fn certify_verification_capabilities(store: &OperationalStore) {
    let mut certification = EngineCertificationSpec::candidate(
        "cert-postgresql-verification-ipc",
        "postgresql",
        ">=17,<18",
        "pg_restore",
        ">=17,<18",
        "windows",
        "x86_64",
        "custom",
        "application-consistent",
        "evidence/verification-ipc.json",
        digest("verification-ipc-evidence"),
        2_200,
    );
    for capability in ProviderCapability::REQUIRED {
        certification = certification.with_capability(CertificationCapability::passed(
            capability.as_str(),
            format!("evidence/{}.json", capability.as_str()),
        ));
    }
    store
        .certification()
        .insert_candidate(certification)
        .expect("insert verification certification");
    store
        .certification()
        .certify("cert-postgresql-verification-ipc", 2_300, 10_000)
        .expect("certify verification capabilities");
}

fn seed_repair_truth(
    connection: &Connection,
    recovery_point_id: &RecoveryPointId,
    source_replica_id: &ArtifactReplicaId,
    source_destination_id: &DestinationId,
) {
    connection
        .execute_batch("PRAGMA foreign_keys=ON;")
        .expect("enable repair foreign keys");
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
                 'postgresql', 'complete', 'application_consistent', 2200,
                 2, ?2, 'signing-key-01', 'valid',
                 1, 1, '{}', 2200, 2200
             )",
            params![recovery_point_id.as_str(), digest("repair-manifest")],
        )
        .expect("insert repair recovery point");
    connection
        .execute(
            "INSERT INTO catalog_artifact_replicas(
                 id, recovery_point_id, destination_fingerprint,
                 destination_kind, requirement, state,
                 immutability_capability, reader_status,
                 committed_at_ms, last_observed_at_ms, metadata_json
             ) VALUES (
                 ?1, ?2, ?3,
                 'local', 'required', 'committed',
                 'none', 'eligible', 2200, 2200, '{}'
             )",
            params![
                source_replica_id.as_str(),
                recovery_point_id.as_str(),
                format!("local:{}", source_destination_id.as_str())
            ],
        )
        .expect("insert repair source replica");
    connection
        .execute(
            "INSERT INTO catalog_snapshot_projection(
                 recovery_point_id, source_target_identity, source_display_name,
                 engine, committed_at_ms, artifact_completeness, replica_durability,
                 protection_compliance, restore_eligibility, search_text,
                 projection_version, projected_at_ms
             ) VALUES (
                 ?1, 'source:orders', 'orders',
                 'postgresql', 2200, 'complete', 'degraded',
                 'non_compliant', 'conditional', 'orders',
                 1, 2200
             )",
            [recovery_point_id.as_str()],
        )
        .expect("insert repair snapshot projection");
}

fn shadow_model() -> CanonicalV1ShadowModel {
    CanonicalV1ShadowModel {
        source_checksum_blake3: SOURCE_CHECKSUM.to_owned(),
        connections: vec![CanonicalConnection {
            key: "postgresql|recovery.internal|5432|backup".to_owned(),
            display_name: "Recovery PostgreSQL".to_owned(),
            engine: "postgresql".to_owned(),
            host: "recovery.internal".to_owned(),
            port: 5432,
            user: "backup".to_owned(),
            source_resource_key: "legacy:recovery.internal".to_owned(),
        }],
        targets: vec![CanonicalTarget {
            key: "postgresql|recovery.internal|5432|orders".to_owned(),
            connection_key: "postgresql|recovery.internal|5432|backup".to_owned(),
            engine_object_id: "orders".to_owned(),
            display_name: "orders".to_owned(),
            normalized_name: "orders".to_owned(),
            source_fingerprint: digest("source-fingerprint"),
        }],
        destinations: vec![CanonicalDestination {
            key: "local|d:/restore-vault".to_owned(),
            display_name: "restore-vault".to_owned(),
            kind: "local".to_owned(),
            endpoint_json: r#"{"path":"D:/restore-vault"}"#.to_owned(),
            declared_capabilities_json: r#"{"write":true}"#.to_owned(),
            immutability_capability: "none".to_owned(),
        }],
        plans: vec![CanonicalPlan {
            key: "orders-nightly".to_owned(),
            revision_key: "orders-nightly|v1".to_owned(),
            display_name: "orders-nightly".to_owned(),
            target_key: "postgresql|recovery.internal|5432|orders".to_owned(),
            destination_keys: vec!["local|d:/restore-vault".to_owned()],
            enabled: true,
            schedule_expression: "0 2 * * *".to_owned(),
            retention_policy_json: r#"{"keepLast":7}"#.to_owned(),
            verification_policy_json: r#"{"level":"l1"}"#.to_owned(),
            execution_policy_json: r#"{"requiredDestinations":"all"}"#.to_owned(),
            content_hash_blake3: digest("plan-content"),
        }],
        secret_refs: Vec::new(),
        secret_bindings: Vec::new(),
    }
}

fn digest(value: &str) -> String {
    blake3::hash(value.as_bytes()).to_hex().to_string()
}
