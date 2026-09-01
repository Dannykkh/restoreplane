use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use dbbackup_application::{
    ApplicationCommand, AuthoritySnapshot, CutoverMode, ExecutionAuthority,
};
use dbbackup_catalog::{
    rusqlite::{params, Connection},
    ConnectionProfileSpec, DatabaseTargetSpec, DestinationSpec, OperationalStore, PlanDraftSpec,
    RetentionSafetyControlSpec, WorkspaceSpec,
};
use dbbackup_domain::{
    BackupPlanId, ConnectionProfileId, DatabaseTargetId, DestinationId, DestinationRequirement,
    IpcRequestId, PlanOccurrenceId, PlanRevisionId, RecoveryPointId, WorkspaceId,
};
use dbbackup_pipeline::{RetentionDispositionV2, RetentionReasonV2};
use dbbackup_service::application::ServiceApplication;
use dbbackup_service::host::{HostConfig, HostRuntime, ServiceHost};
use dbbackup_service::ipc::{
    authorize_and_decode, encode_request, AuthorizationPolicy, DurableIpcDispatcher,
    RawConfirmationToken, RequestEnvelope, TokenIdentity, WindowsSid,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use uuid::Uuid;

const SERVICE_SID: &str = "S-1-5-80-1001";
const OWNER_SID: &str = "S-1-5-21-100-200-300-1001";
const OWNER_LOGON_SID: &str = "S-1-5-5-10-20";
const OLD_COMMITTED_AT_MS: i64 = 86_400_000;
const NEW_COMMITTED_AT_MS: i64 = 172_800_000;
const NOW_MS: i64 = 259_200_000;

#[test]
fn authorized_frame_dry_run_uses_catalog_authority_without_ledger_mutation() {
    let fixture = Fixture::new();
    let application = Arc::new(ServiceApplication::with_clock(
        fixture.host.health_probe(),
        fixture.host.operational_store(),
        fixed_now_ms,
    ));
    let dispatcher =
        DurableIpcDispatcher::new(fixture.store.ipc(), application, rehearsal_authority());
    let payload = json!({
        "recoveryPointId": fixture.old_recovery_point_id.as_str(),
        "destinationFingerprint": fixture.destination_fingerprint,
        "gracePeriodMs": 10_000,
    });
    let response = dispatcher
        .dispatch(authorized(&frame(payload.clone())))
        .expect("dispatch retention dry run");

    assert!(
        response.is_success(),
        "unexpected response {}: {}",
        response.code(),
        response.body()
    );
    let body = &response.body()["body"];
    assert!(!response.deduplicated());
    assert_eq!(body["schemaVersion"], 1);
    assert_eq!(body["evaluatedAtMs"], NOW_MS);
    assert_eq!(body["disposition"], "tombstone_required");
    assert_eq!(body["reasons"], json!([]));
    assert_eq!(body["wouldBreakCompliance"], false);
    assert_eq!(body["authorization"]["plan"]["policy_expired"], true);
    assert_eq!(
        body["authorization"]["plan"]["objects"],
        json!([
            {
                "key": "set-manifest.json",
                "role": "set_manifest",
                "version": "manifest-version"
            },
            {
                "key": "parts/orders.enc",
                "role": "payload",
                "version": "payload-version"
            }
        ])
    );
    assert_eq!(
        body["digestBlake3"].as_str().expect("dry-run digest").len(),
        64
    );
    assert_eq!(
        fixture.store.ipc().request_count().expect("ledger count"),
        0,
        "read-only dry run must not create an idempotency ledger row"
    );

    let mut forged = payload;
    forged
        .as_object_mut()
        .expect("payload object")
        .insert("pinned".to_owned(), Value::Bool(true));
    let rejected = dispatcher
        .dispatch(authorized(&frame(forged)))
        .expect("application failure is returned as an IPC response");
    assert!(!rejected.is_success());
    assert_eq!(rejected.code(), "APPLICATION_PAYLOAD_INVALID");
    assert_eq!(
        fixture.store.ipc().request_count().expect("ledger count"),
        0
    );
}

#[test]
fn control_set_replays_and_rolls_back_control_with_ledger_completion() {
    let fixture = Fixture::new();
    let application = Arc::new(ServiceApplication::with_clock(
        fixture.host.health_probe(),
        fixture.host.operational_store(),
        fixed_now_ms,
    ));
    let dispatcher =
        DurableIpcDispatcher::new(fixture.store.ipc(), application, rehearsal_authority());
    let payload = json!({
        "recoveryPointId": fixture.old_recovery_point_id.as_str(),
        "destinationFingerprint": fixture.destination_fingerprint,
        "pinned": true,
        "legalHold": true,
    });
    let original = mutation_frame(
        ApplicationCommand::RetentionControlSet,
        "retention-control-01",
        NOW_MS - 1_000,
        payload.clone(),
    );

    let first = dispatcher
        .dispatch(authorized_at(&original, NOW_MS))
        .expect("set retention control");
    assert!(first.is_success());
    assert!(!first.deduplicated());
    assert_eq!(
        first.body()["body"],
        json!({
            "schemaVersion": 1,
            "recoveryPointId": fixture.old_recovery_point_id.as_str(),
            "destinationFingerprint": fixture.destination_fingerprint,
            "pinned": true,
            "legalHold": true,
            "updatedAtMs": NOW_MS,
        })
    );
    assert_eq!(
        fixture.store.ipc().request_count().expect("ledger count"),
        1
    );
    let preserved = fixture
        .store
        .retention_planner()
        .prepare_delete_plan(
            &fixture.old_recovery_point_id,
            &fixture.destination_fingerprint,
            NOW_MS,
            10_000,
        )
        .expect("controlled dry run");
    assert_eq!(
        preserved.decision().disposition,
        RetentionDispositionV2::Preserve
    );
    assert!(preserved
        .decision()
        .reasons
        .contains(&RetentionReasonV2::Pinned));
    assert!(preserved
        .decision()
        .reasons
        .contains(&RetentionReasonV2::LegalHold));

    let replay = dispatcher
        .dispatch(authorized_at(&original, NOW_MS + 1))
        .expect("replay retention control");
    assert!(replay.is_success());
    assert!(replay.deduplicated());
    assert_eq!(replay.body(), first.body());
    assert_eq!(
        fixture.store.ipc().request_count().expect("ledger count"),
        1
    );

    let conflict = mutation_frame(
        ApplicationCommand::RetentionControlSet,
        "retention-control-01",
        NOW_MS - 900,
        json!({
            "recoveryPointId": fixture.old_recovery_point_id.as_str(),
            "destinationFingerprint": fixture.destination_fingerprint,
            "pinned": false,
            "legalHold": true,
        }),
    );
    let conflict = dispatcher
        .dispatch(authorized_at(&conflict, NOW_MS + 1))
        .expect_err("same key cannot mutate a different control payload");
    assert_eq!(conflict.code(), "IPC_IDEMPOTENCY_CONFLICT");
    assert_eq!(
        fixture.store.ipc().request_count().expect("ledger count"),
        1
    );

    let forged = mutation_frame(
        ApplicationCommand::RetentionControlSet,
        "retention-control-forged",
        NOW_MS - 800,
        json!({
            "recoveryPointId": fixture.old_recovery_point_id.as_str(),
            "destinationFingerprint": fixture.destination_fingerprint,
            "pinned": false,
            "legalHold": false,
            "updatedAtMs": 0,
        }),
    );
    let rejected = dispatcher
        .dispatch(authorized_at(&forged, NOW_MS + 1))
        .expect("invalid payload is returned as an application response");
    assert!(!rejected.is_success());
    assert_eq!(rejected.code(), "APPLICATION_PAYLOAD_INVALID");
    assert_eq!(
        fixture.store.ipc().request_count().expect("ledger count"),
        1
    );

    direct(&fixture.db_path)
        .execute_batch(
            "CREATE TRIGGER fail_retention_control_completion
             BEFORE UPDATE OF state ON ipc_request_ledger
             WHEN NEW.command_name = 'retention.control_set'
              AND NEW.state = 'succeeded'
             BEGIN
                 SELECT RAISE(ABORT, 'injected retention control completion failure');
             END;",
        )
        .expect("install completion failure injection");
    let rollback_frame = mutation_frame(
        ApplicationCommand::RetentionControlSet,
        "retention-control-rollback",
        NOW_MS - 700,
        json!({
            "recoveryPointId": fixture.old_recovery_point_id.as_str(),
            "destinationFingerprint": fixture.destination_fingerprint,
            "pinned": false,
            "legalHold": false,
        }),
    );
    let injected = dispatcher
        .dispatch(authorized_at(&rollback_frame, NOW_MS + 2))
        .expect_err("ledger completion failure must abort the typed transaction");
    assert_eq!(injected.code(), "OPERATIONAL_REPOSITORY_SQLITE");
    assert_eq!(
        fixture.store.ipc().request_count().expect("ledger count"),
        1,
        "failed completion must roll back the new ledger row"
    );
    let still_preserved = fixture
        .store
        .retention_planner()
        .prepare_delete_plan(
            &fixture.old_recovery_point_id,
            &fixture.destination_fingerprint,
            NOW_MS,
            10_000,
        )
        .expect("rolled-back control dry run");
    assert!(
        still_preserved
            .decision()
            .reasons
            .contains(&RetentionReasonV2::Pinned),
        "control document must roll back with ledger completion"
    );

    direct(&fixture.db_path)
        .execute_batch("DROP TRIGGER fail_retention_control_completion;")
        .expect("remove completion failure injection");
    let retried = dispatcher
        .dispatch(authorized_at(&rollback_frame, NOW_MS + 2))
        .expect("retry fully rolled-back request");
    assert!(retried.is_success());
    assert!(!retried.deduplicated());
    assert_eq!(
        fixture.store.ipc().request_count().expect("ledger count"),
        2
    );
    let released = fixture
        .store
        .retention_planner()
        .prepare_delete_plan(
            &fixture.old_recovery_point_id,
            &fixture.destination_fingerprint,
            NOW_MS,
            10_000,
        )
        .expect("released dry run");
    assert_eq!(
        released.decision().disposition,
        RetentionDispositionV2::TombstoneRequired
    );
    assert!(released.decision().reasons.is_empty());
}

#[test]
fn retention_mutation_rejects_a_recovery_point_from_another_workspace_before_ledger_write() {
    let fixture = Fixture::new();
    let other_workspace_id = WorkspaceId::new();
    fixture
        .store
        .workspace()
        .create(WorkspaceSpec::new(
            other_workspace_id.clone(),
            "Other Workspace",
            NOW_MS,
        ))
        .expect("create other workspace");
    let application = Arc::new(ServiceApplication::with_clock(
        fixture.host.health_probe(),
        fixture.host.operational_store(),
        fixed_now_ms,
    ));
    let dispatcher =
        DurableIpcDispatcher::new(fixture.store.ipc(), application, rehearsal_authority());
    let request = mutation_frame(
        ApplicationCommand::RetentionControlSet,
        "retention-cross-workspace-01",
        NOW_MS - 1_000,
        json!({
            "workspaceId": other_workspace_id.as_str(),
            "recoveryPointId": fixture.old_recovery_point_id.as_str(),
            "destinationFingerprint": fixture.destination_fingerprint,
            "pinned": true,
            "legalHold": false,
        }),
    );

    let error = dispatcher
        .dispatch(authorized_at(&request, NOW_MS))
        .expect_err("cross-workspace retention mutation must fail");
    assert_eq!(error.code(), "WORKSPACE_SCOPE_MISMATCH");
    assert_eq!(
        fixture.store.ipc().request_count().expect("ledger count"),
        0
    );
}

#[test]
fn tombstone_uses_server_time_current_authority_and_atomic_ledger_completion() {
    let fixture = Fixture::new();
    let application = Arc::new(ServiceApplication::with_clock(
        fixture.host.health_probe(),
        fixture.host.operational_store(),
        fixed_now_ms,
    ));
    let dispatcher =
        DurableIpcDispatcher::new(fixture.store.ipc(), application, rehearsal_authority());
    fixture
        .store
        .retention_planner()
        .set_safety_control(RetentionSafetyControlSpec::new(
            fixture.old_recovery_point_id.clone(),
            fixture.destination_fingerprint.clone(),
            true,
            false,
            NOW_MS - 1,
        ))
        .expect("pin candidate");
    let payload = json!({
        "recoveryPointId": fixture.old_recovery_point_id.as_str(),
        "destinationFingerprint": fixture.destination_fingerprint,
        "gracePeriodMs": 10_000,
    });
    let frame = mutation_frame(
        ApplicationCommand::RetentionTombstone,
        "retention-tombstone-01",
        NOW_MS - 100,
        payload.clone(),
    );

    let protected = dispatcher
        .dispatch(authorized_at(&frame, NOW_MS))
        .expect_err("a currently pinned candidate cannot be tombstoned");
    assert_eq!(protected.code(), "RETENTION_TOMBSTONE_INVALID");
    assert_eq!(
        fixture.store.ipc().request_count().expect("ledger count"),
        0
    );
    assert_eq!(operation_count(&fixture.db_path), 0);

    fixture
        .store
        .retention_planner()
        .set_safety_control(RetentionSafetyControlSpec::new(
            fixture.old_recovery_point_id.clone(),
            fixture.destination_fingerprint.clone(),
            false,
            false,
            NOW_MS + 1,
        ))
        .expect("release candidate");
    direct(&fixture.db_path)
        .execute_batch(
            "CREATE TRIGGER fail_retention_tombstone_completion
             BEFORE UPDATE OF state ON ipc_request_ledger
             WHEN NEW.command_name = 'retention.tombstone'
              AND NEW.state = 'succeeded'
             BEGIN
                 SELECT RAISE(ABORT, 'injected retention tombstone completion failure');
             END;",
        )
        .expect("install tombstone completion failure injection");
    let injected = dispatcher
        .dispatch(authorized_at(&frame, NOW_MS + 2))
        .expect_err("ledger completion failure must abort tombstone staging");
    assert_eq!(injected.code(), "OPERATIONAL_REPOSITORY_SQLITE");
    assert_eq!(
        fixture.store.ipc().request_count().expect("ledger count"),
        0
    );
    assert_eq!(
        operation_count(&fixture.db_path),
        0,
        "operation and ledger must roll back together"
    );

    direct(&fixture.db_path)
        .execute_batch("DROP TRIGGER fail_retention_tombstone_completion;")
        .expect("remove tombstone completion failure injection");
    let first = dispatcher
        .dispatch(authorized_at(&frame, NOW_MS + 2))
        .expect("stage tombstone");
    assert!(first.is_success());
    assert!(!first.deduplicated());
    let body = &first.body()["body"];
    let operation_run_id = body["operationRunId"].as_str().expect("operation run ID");
    Uuid::parse_str(operation_run_id).expect("canonical operation run UUID");
    assert_eq!(body["schemaVersion"], 1);
    assert_eq!(
        body["recoveryPointId"],
        fixture.old_recovery_point_id.as_str()
    );
    assert_eq!(
        body["destinationFingerprint"],
        fixture.destination_fingerprint
    );
    assert_eq!(body["gracePeriodMs"], 10_000);
    assert_eq!(
        body["tombstonedAtMs"],
        NOW_MS + 2,
        "grace must start at server acceptance, not client issuedAt"
    );
    assert_eq!(body["eligibleAfterMs"], NOW_MS + 10_002);
    assert_eq!(
        fixture.store.ipc().request_count().expect("ledger count"),
        1
    );
    assert_eq!(operation_count(&fixture.db_path), 1);

    let request_json: String = direct(&fixture.db_path)
        .query_row(
            "SELECT request_json FROM operation_runs WHERE id = ?1",
            [operation_run_id],
            |row| row.get(0),
        )
        .expect("stored tombstone");
    let tombstone: Value = serde_json::from_str(&request_json).expect("tombstone JSON");
    assert_eq!(tombstone["tombstoned_at_ms"], NOW_MS + 2);
    assert_eq!(
        tombstone["objects"],
        json!([
            {
                "key": "set-manifest.json",
                "role": "set_manifest",
                "version": "manifest-version"
            },
            {
                "key": "parts/orders.enc",
                "role": "payload",
                "version": "payload-version"
            }
        ])
    );

    let replay = dispatcher
        .dispatch(authorized_at(&frame, NOW_MS + 3))
        .expect("replay tombstone request");
    assert!(replay.is_success());
    assert!(replay.deduplicated());
    assert_eq!(replay.body(), first.body());
    assert_eq!(operation_count(&fixture.db_path), 1);

    let conflict = mutation_frame(
        ApplicationCommand::RetentionTombstone,
        "retention-tombstone-01",
        NOW_MS,
        json!({
            "recoveryPointId": fixture.old_recovery_point_id.as_str(),
            "destinationFingerprint": fixture.destination_fingerprint,
            "gracePeriodMs": 20_000,
        }),
    );
    let conflict = dispatcher
        .dispatch(authorized_at(&conflict, NOW_MS + 3))
        .expect_err("same key cannot change grace");
    assert_eq!(conflict.code(), "IPC_IDEMPOTENCY_CONFLICT");

    let forged = mutation_frame(
        ApplicationCommand::RetentionTombstone,
        "retention-tombstone-forged",
        NOW_MS,
        json!({
            "recoveryPointId": fixture.old_recovery_point_id.as_str(),
            "destinationFingerprint": fixture.destination_fingerprint,
            "gracePeriodMs": 10_000,
            "tombstonedAtMs": 0,
        }),
    );
    let rejected = dispatcher
        .dispatch(authorized_at(&forged, NOW_MS + 3))
        .expect("invalid payload is returned as an application response");
    assert!(!rejected.is_success());
    assert_eq!(rejected.code(), "APPLICATION_PAYLOAD_INVALID");
    assert_eq!(
        fixture.store.ipc().request_count().expect("ledger count"),
        1
    );
    assert_eq!(operation_count(&fixture.db_path), 1);

    let waiting = fixture
        .store
        .retention_planner()
        .prepare_delete_plan(
            &fixture.old_recovery_point_id,
            &fixture.destination_fingerprint,
            NOW_MS + 10_001,
            10_000,
        )
        .expect("grace still active");
    assert_eq!(
        waiting.decision().disposition,
        RetentionDispositionV2::WaitForGrace
    );
    let eligible = fixture
        .store
        .retention_planner()
        .prepare_delete_plan(
            &fixture.old_recovery_point_id,
            &fixture.destination_fingerprint,
            NOW_MS + 10_002,
            10_000,
        )
        .expect("grace elapsed");
    assert_eq!(
        eligible.decision().disposition,
        RetentionDispositionV2::DeleteEligible
    );
}

#[test]
fn confirmation_prepare_is_atomic_and_never_persists_or_replays_the_raw_token() {
    let fixture = Fixture::new();
    let application = Arc::new(ServiceApplication::with_clock(
        fixture.host.health_probe(),
        fixture.host.operational_store(),
        fixed_now_ms,
    ));
    let dispatcher =
        DurableIpcDispatcher::new(fixture.store.ipc(), application, rehearsal_authority());
    let tombstone_payload = json!({
        "recoveryPointId": fixture.old_recovery_point_id.as_str(),
        "destinationFingerprint": fixture.destination_fingerprint,
        "gracePeriodMs": 10_000,
    });
    let tombstone_frame = mutation_frame(
        ApplicationCommand::RetentionTombstone,
        "retention-tombstone-for-confirmation",
        NOW_MS - 100,
        tombstone_payload,
    );
    let tombstone = dispatcher
        .dispatch(authorized_at(&tombstone_frame, NOW_MS))
        .expect("stage prerequisite tombstone");
    let operation_run_id = tombstone.body()["body"]["operationRunId"]
        .as_str()
        .expect("tombstone operation ID")
        .to_owned();
    let eligible_at_ms = NOW_MS + 10_000;
    let eligible = fixture
        .store
        .retention_planner()
        .prepare_delete_plan(
            &fixture.old_recovery_point_id,
            &fixture.destination_fingerprint,
            eligible_at_ms,
            10_000,
        )
        .expect("eligible authorization");
    assert_eq!(
        eligible.decision().disposition,
        RetentionDispositionV2::DeleteEligible
    );
    let payload = json!({
        "action": "retention_delete",
        "recoveryPointId": fixture.old_recovery_point_id.as_str(),
        "destinationFingerprint": fixture.destination_fingerprint,
        "gracePeriodMs": 10_000,
        "ttlMs": 60_000,
    });
    let frame = mutation_frame(
        ApplicationCommand::ConfirmationPrepare,
        "retention-confirmation-01",
        eligible_at_ms - 100,
        payload.clone(),
    );

    let premature = mutation_frame(
        ApplicationCommand::ConfirmationPrepare,
        "retention-confirmation-premature",
        eligible_at_ms - 200,
        payload.clone(),
    );
    let premature = dispatcher
        .dispatch(authorized_at(&premature, eligible_at_ms - 1))
        .expect_err("confirmation requires a currently eligible authoritative plan");
    assert_eq!(premature.code(), "RETENTION_TOMBSTONE_INVALID");
    assert_eq!(
        fixture.store.ipc().request_count().expect("ledger count"),
        1
    );
    assert_eq!(confirmation_count(&fixture.db_path), 0);

    direct(&fixture.db_path)
        .execute_batch(
            "CREATE TRIGGER fail_confirmation_prepare_completion
             BEFORE UPDATE OF state ON ipc_request_ledger
             WHEN NEW.command_name = 'confirmation.prepare'
              AND NEW.state = 'succeeded'
             BEGIN
                 SELECT RAISE(ABORT, 'injected confirmation completion failure');
             END;",
        )
        .expect("install confirmation completion failure injection");
    let injected = dispatcher
        .dispatch(authorized_at(&frame, eligible_at_ms))
        .expect_err("ledger completion failure must abort token issuance");
    assert_eq!(injected.code(), "OPERATIONAL_REPOSITORY_SQLITE");
    assert_eq!(
        fixture.store.ipc().request_count().expect("ledger count"),
        1
    );
    assert_eq!(confirmation_count(&fixture.db_path), 0);

    direct(&fixture.db_path)
        .execute_batch("DROP TRIGGER fail_confirmation_prepare_completion;")
        .expect("remove confirmation completion failure injection");
    let first = dispatcher
        .dispatch(authorized_at(&frame, eligible_at_ms))
        .expect("prepare confirmation");
    assert!(first.is_success());
    assert!(!first.deduplicated());
    let body = &first.body()["body"];
    let confirmation_token_id = body["confirmationTokenId"]
        .as_str()
        .expect("confirmation token ID");
    Uuid::parse_str(confirmation_token_id).expect("canonical confirmation UUID");
    let encoded_token = body["confirmationToken"]
        .as_str()
        .expect("one-time raw confirmation token");
    let raw_token = RawConfirmationToken::from_encoded(encoded_token).expect("raw token");
    assert_eq!(raw_token.expose().len(), 32);
    assert_eq!(body["schemaVersion"], 1);
    assert_eq!(body["action"], "retention_delete");
    assert_eq!(body["issuedAtMs"], eligible_at_ms);
    assert_eq!(body["expiresAtMs"], eligible_at_ms + 60_000);
    assert_eq!(body["tokenAvailable"], true);
    assert_eq!(body["payloadDigestBlake3"], eligible.digest_blake3());
    assert_eq!(
        body["authorization"]["digest_blake3"],
        eligible.digest_blake3()
    );
    assert_eq!(
        fixture.store.ipc().request_count().expect("ledger count"),
        2
    );
    assert_eq!(confirmation_count(&fixture.db_path), 1);

    let record = fixture
        .store
        .ipc()
        .confirmation(
            &confirmation_token_id
                .parse()
                .expect("typed confirmation token ID"),
        )
        .expect("confirmation record");
    assert_eq!(record.state(), "issued");
    assert_eq!(record.action(), "retention_delete");
    assert_eq!(record.resource_type(), "artifact_replica");
    assert_eq!(record.resource_id(), fixture.old_recovery_point_id.as_str());
    assert_eq!(
        record.resource_fingerprint(),
        fixture.destination_fingerprint
    );
    assert_eq!(
        record.payload_digest_blake3(),
        body["payloadDigestBlake3"]
            .as_str()
            .expect("response payload digest")
    );
    assert_eq!(
        record.token_digest_blake3(),
        blake3::hash(raw_token.expose()).to_hex().as_str()
    );

    let stored_response: String = direct(&fixture.db_path)
        .query_row(
            "SELECT response_json
             FROM ipc_request_ledger
             WHERE command_name = 'confirmation.prepare'",
            [],
            |row| row.get(0),
        )
        .expect("stored redacted confirmation response");
    assert!(!stored_response.contains(encoded_token));
    assert!(stored_response.contains("\"token_available\":false"));
    assert!(!stored_response.contains("\"confirmation_token\""));
    for path in database_files(&fixture.db_path) {
        let bytes = fs::read(&path).expect("read operational store file");
        assert!(!contains(&bytes, encoded_token.as_bytes()));
        assert!(!contains(&bytes, raw_token.expose()));
    }

    let replay = dispatcher
        .dispatch(authorized_at(&frame, eligible_at_ms + 1))
        .expect("replay confirmation prepare");
    assert!(replay.is_success());
    assert!(replay.deduplicated());
    assert_eq!(
        replay.body()["body"]["confirmationTokenId"],
        confirmation_token_id
    );
    assert_eq!(replay.body()["body"]["confirmationToken"], Value::Null);
    assert_eq!(replay.body()["body"]["tokenAvailable"], false);
    assert_eq!(confirmation_count(&fixture.db_path), 1);

    let conflict = mutation_frame(
        ApplicationCommand::ConfirmationPrepare,
        "retention-confirmation-01",
        eligible_at_ms,
        json!({
            "action": "retention_delete",
            "recoveryPointId": fixture.old_recovery_point_id.as_str(),
            "destinationFingerprint": fixture.destination_fingerprint,
            "gracePeriodMs": 20_000,
            "ttlMs": 60_000,
        }),
    );
    let conflict = dispatcher
        .dispatch(authorized_at(&conflict, eligible_at_ms + 1))
        .expect_err("same key cannot bind another digest");
    assert_eq!(conflict.code(), "IPC_IDEMPOTENCY_CONFLICT");

    let forged = mutation_frame(
        ApplicationCommand::ConfirmationPrepare,
        "retention-confirmation-forged",
        eligible_at_ms,
        json!({
            "action": "retention_delete",
            "recoveryPointId": fixture.old_recovery_point_id.as_str(),
            "destinationFingerprint": fixture.destination_fingerprint,
            "gracePeriodMs": 10_000,
            "ttlMs": 60_000,
            "confirmationToken": "caller-supplied",
        }),
    );
    let rejected = dispatcher
        .dispatch(authorized_at(&forged, eligible_at_ms + 1))
        .expect("invalid payload is an application response");
    assert!(!rejected.is_success());
    assert_eq!(rejected.code(), "APPLICATION_PAYLOAD_INVALID");
    assert_eq!(
        fixture.store.ipc().request_count().expect("ledger count"),
        2
    );
    assert_eq!(confirmation_count(&fixture.db_path), 1);

    let pin = mutation_frame(
        ApplicationCommand::RetentionControlSet,
        "retention-pin-before-delete",
        eligible_at_ms,
        json!({
            "recoveryPointId": fixture.old_recovery_point_id.as_str(),
            "destinationFingerprint": fixture.destination_fingerprint,
            "pinned": true,
            "legalHold": false,
        }),
    );
    let pinned = dispatcher
        .dispatch(authorized_at(&pin, eligible_at_ms + 2))
        .expect("pin candidate after confirmation");
    assert!(pinned.is_success());
    let delete_payload = json!({
        "operationRunId": operation_run_id,
        "confirmationTokenId": confirmation_token_id,
        "confirmationToken": encoded_token,
        "authorization": body["authorization"].clone(),
    });
    let pinned_delete = mutation_frame(
        ApplicationCommand::RetentionDelete,
        "retention-delete-while-pinned",
        eligible_at_ms,
        delete_payload.clone(),
    );
    let pinned_delete = dispatcher
        .dispatch(authorized_at(&pinned_delete, eligible_at_ms + 3))
        .expect_err("delete acceptance must re-evaluate current retention authority");
    assert_eq!(pinned_delete.code(), "RETENTION_TOMBSTONE_INVALID");
    assert_eq!(
        fixture
            .store
            .ipc()
            .confirmation(
                &confirmation_token_id
                    .parse()
                    .expect("typed confirmation token ID"),
            )
            .expect("confirmation remains issued")
            .state(),
        "issued"
    );
    assert_eq!(work_count(&fixture.db_path), 0);
    assert_eq!(
        fixture.store.ipc().request_count().expect("ledger count"),
        3
    );

    let unpin = mutation_frame(
        ApplicationCommand::RetentionControlSet,
        "retention-unpin-before-delete",
        eligible_at_ms,
        json!({
            "recoveryPointId": fixture.old_recovery_point_id.as_str(),
            "destinationFingerprint": fixture.destination_fingerprint,
            "pinned": false,
            "legalHold": false,
        }),
    );
    let unpinned = dispatcher
        .dispatch(authorized_at(&unpin, eligible_at_ms + 4))
        .expect("remove current retention blocker");
    assert!(unpinned.is_success());

    let delete_frame = mutation_frame(
        ApplicationCommand::RetentionDelete,
        "retention-delete-01",
        eligible_at_ms,
        delete_payload.clone(),
    );
    direct(&fixture.db_path)
        .execute_batch(
            "CREATE TRIGGER fail_retention_delete_completion
             BEFORE UPDATE OF state ON ipc_request_ledger
             WHEN NEW.command_name = 'retention.delete'
              AND NEW.state = 'succeeded'
             BEGIN
                 SELECT RAISE(ABORT, 'injected retention delete completion failure');
             END;",
        )
        .expect("install retention delete completion failure injection");
    let injected_delete = dispatcher
        .dispatch(authorized_at(&delete_frame, eligible_at_ms + 5))
        .expect_err("delete completion failure must roll back every durable mutation");
    assert_eq!(injected_delete.code(), "OPERATIONAL_REPOSITORY_SQLITE");
    assert_eq!(
        fixture
            .store
            .ipc()
            .confirmation(
                &confirmation_token_id
                    .parse()
                    .expect("typed confirmation token ID"),
            )
            .expect("confirmation after injected failure")
            .state(),
        "issued"
    );
    assert_eq!(work_count(&fixture.db_path), 0);
    assert_eq!(
        direct(&fixture.db_path)
            .query_row(
                "SELECT state FROM operation_runs WHERE id = ?1",
                [&operation_run_id],
                |row| row.get::<_, String>(0),
            )
            .expect("tombstone state after injected failure"),
        "created"
    );
    assert_eq!(
        fixture.store.ipc().request_count().expect("ledger count"),
        4
    );
    direct(&fixture.db_path)
        .execute_batch("DROP TRIGGER fail_retention_delete_completion;")
        .expect("remove retention delete completion failure injection");
    let deleted = dispatcher
        .dispatch(authorized_at(&delete_frame, eligible_at_ms + 5))
        .expect("consume confirmation and enqueue durable retention delete");
    assert!(deleted.is_success());
    assert!(!deleted.deduplicated());
    let delete_body = &deleted.body()["body"];
    assert_eq!(delete_body["schemaVersion"], 1);
    assert_eq!(delete_body["operationRunId"], operation_run_id);
    assert_eq!(delete_body["state"], "queued");
    let work_item_id = delete_body["workItemId"]
        .as_str()
        .expect("delete work item ID");
    Uuid::parse_str(work_item_id).expect("canonical delete work UUID");
    assert_eq!(work_count(&fixture.db_path), 1);
    assert_eq!(
        fixture.store.ipc().request_count().expect("ledger count"),
        5
    );
    assert_eq!(
        fixture
            .store
            .ipc()
            .confirmation(
                &confirmation_token_id
                    .parse()
                    .expect("typed confirmation token ID"),
            )
            .expect("consumed confirmation")
            .state(),
        "consumed"
    );

    let operation: (String, String, String, String) = direct(&fixture.db_path)
        .query_row(
            "SELECT operation_runs.state, operation_runs.request_json,
                    work_items.id, work_items.state
             FROM operation_runs
             JOIN work_items ON work_items.operation_run_id = operation_runs.id
             WHERE operation_runs.id = ?1",
            [&operation_run_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("queued retention operation");
    assert_eq!(operation.0, "queued");
    assert_eq!(operation.2, work_item_id);
    assert_eq!(operation.3, "queued");
    let operation_request: Value =
        serde_json::from_str(&operation.1).expect("retention operation request");
    assert_eq!(
        operation_request["authorization"]["plan"]["now_ms"],
        eligible_at_ms + 5
    );
    assert_eq!(operation_request["authorized_at_ms"], eligible_at_ms + 5);
    assert_eq!(
        operation_request["confirmation_digest_blake3"],
        body["payloadDigestBlake3"]
    );
    assert_eq!(
        operation_request["dry_run_digest_blake3"],
        operation_request["authorization"]["digest_blake3"]
    );
    assert_ne!(
        operation_request["request_payload_digest_blake3"],
        operation_request["confirmation_digest_blake3"]
    );
    assert_eq!(
        operation_request["request_payload_digest_blake3"]
            .as_str()
            .expect("request payload digest")
            .len(),
        64
    );
    assert!(!operation.1.contains(encoded_token));
    for path in database_files(&fixture.db_path) {
        let bytes = fs::read(&path).expect("read operational store after delete enqueue");
        assert!(!contains(&bytes, encoded_token.as_bytes()));
        assert!(!contains(&bytes, raw_token.expose()));
    }

    let replayed_delete = dispatcher
        .dispatch(authorized_at(&delete_frame, eligible_at_ms + 6))
        .expect("replay durable retention delete");
    assert!(replayed_delete.is_success());
    assert!(replayed_delete.deduplicated());
    assert_eq!(replayed_delete.body()["body"]["workItemId"], work_item_id);
    assert_eq!(work_count(&fixture.db_path), 1);
    assert_eq!(
        fixture.store.ipc().request_count().expect("ledger count"),
        5
    );

    let token_reuse = mutation_frame(
        ApplicationCommand::RetentionDelete,
        "retention-delete-token-reuse",
        eligible_at_ms,
        delete_payload,
    );
    let token_reuse = dispatcher
        .dispatch(authorized_at(&token_reuse, eligible_at_ms + 7))
        .expect_err("one-time confirmation cannot enqueue a second delete");
    assert_eq!(token_reuse.code(), "CONFIRMATION_TOKEN_INVALID_OR_REPLAYED");
    assert_eq!(work_count(&fixture.db_path), 1);
    assert_eq!(
        fixture.store.ipc().request_count().expect("ledger count"),
        5
    );
}

struct Fixture {
    _temp: TempDir,
    db_path: PathBuf,
    store: OperationalStore,
    host: ServiceHost<NoopRuntime>,
    old_recovery_point_id: RecoveryPointId,
    destination_fingerprint: String,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().expect("temp directory");
        let db_path = temp.path().join("operational.db");
        let host = ServiceHost::start(
            HostConfig::new(
                format!("retention-application-{}", Uuid::new_v4()),
                db_path.clone(),
            )
            .expect("host config"),
            Arc::new(NoopRuntime),
        )
        .expect("start host");
        let store = host.operational_store().expect("operational store");
        let configuration = store.configuration();
        let connection_profile_id = ConnectionProfileId::new();
        let target_id = DatabaseTargetId::new();
        let destination_id = DestinationId::new();
        let destination_fingerprint = format!("local:{}", destination_id.as_str());
        let revision_id = PlanRevisionId::new();
        configuration
            .insert_connection(ConnectionProfileSpec::tcp(
                connection_profile_id.clone(),
                "primary",
                "postgresql",
                "127.0.0.1",
                5432,
                1,
            ))
            .expect("insert connection");
        configuration
            .insert_target(DatabaseTargetSpec::new(
                target_id.clone(),
                connection_profile_id,
                "orders",
                "orders",
                "orders",
                "source:orders",
                2,
            ))
            .expect("insert target");
        configuration
            .insert_destination(DestinationSpec::local(
                destination_id.clone(),
                "vault",
                r#"{"path":"C:\\vault"}"#,
                3,
            ))
            .expect("insert destination");
        configuration
            .insert_plan_draft(PlanDraftSpec::manual(
                BackupPlanId::new(),
                revision_id.clone(),
                "orders-nightly",
                1,
                "a".repeat(64),
                4,
            ))
            .expect("insert plan");
        configuration
            .add_revision_target(&revision_id, &target_id, 5)
            .expect("add target");
        configuration
            .add_revision_destination(
                &revision_id,
                &destination_id,
                DestinationRequirement::Required,
                6,
            )
            .expect("add destination");
        {
            let connection = direct(&db_path);
            connection
                .execute(
                    "UPDATE backup_plan_revisions
                     SET retention_policy_json = '{\"daily\":1,\"weekly\":0,\"monthly\":0}'
                     WHERE id = ?1",
                    [revision_id.as_str()],
                )
                .expect("set GFS policy");
            connection
                .execute(
                    "UPDATE destinations SET state = 'ready' WHERE id = ?1",
                    [destination_id.as_str()],
                )
                .expect("mark destination ready");
        }
        configuration
            .publish_revision(&revision_id, 7)
            .expect("publish revision");

        let old_recovery_point_id = RecoveryPointId::new();
        let new_recovery_point_id = RecoveryPointId::new();
        {
            let connection = direct(&db_path);
            seed_recovery_point(
                &connection,
                &revision_id,
                &target_id,
                &destination_fingerprint,
                &old_recovery_point_id,
                OLD_COMMITTED_AT_MS,
                true,
            );
            seed_recovery_point(
                &connection,
                &revision_id,
                &target_id,
                &destination_fingerprint,
                &new_recovery_point_id,
                NEW_COMMITTED_AT_MS,
                false,
            );
        }
        Self {
            _temp: temp,
            db_path,
            store,
            host,
            old_recovery_point_id,
            destination_fingerprint,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn seed_recovery_point(
    connection: &Connection,
    revision_id: &PlanRevisionId,
    target_id: &DatabaseTargetId,
    destination_fingerprint: &str,
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
                 ?1, 'instance-a', ?2, ?3,
                 'source:orders', 'orders', 'postgresql', 'complete',
                 'transaction_consistent', ?4, 2,
                 ?5, 'signing-key-a', 'valid',
                 1, 1, '{}', ?4, ?4
             )",
            params![
                recovery_point_id.as_str(),
                occurrence_id.as_str(),
                backup_run_id,
                committed_at_ms,
                "b".repeat(64)
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
                 'local', 'required', 'committed',
                 'none', 'eligible', ?4, ?4, '{}'
             )",
            params![
                replica_id,
                recovery_point_id.as_str(),
                destination_fingerprint,
                committed_at_ms
            ],
        )
        .expect("insert replica");
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
        .expect("insert snapshot");
    if !include_objects {
        return;
    }
    let part_id = Uuid::new_v4().to_string();
    connection
        .execute(
            "INSERT INTO catalog_artifact_parts(
                 id, recovery_point_id, part_role, logical_name,
                 required, dump_format, compression_format, encryption_format,
                 plaintext_checksum_blake3, metadata_json, created_at_ms
             ) VALUES (
                 ?1, ?2, 'database_dump', 'orders',
                 1, 'custom', 'zstd', 'chacha20poly1305-stream',
                 ?3, '{}', ?4
             )",
            params![
                part_id,
                recovery_point_id.as_str(),
                "c".repeat(64),
                committed_at_ms
            ],
        )
        .expect("insert part");
    connection
        .execute(
            "INSERT INTO catalog_stored_objects(
                 id, recovery_point_id, artifact_replica_id, artifact_part_id,
                 object_role, object_key, object_version, checksum_blake3,
                 state, observed_at_ms, metadata_json
             ) VALUES (
                 ?1, ?2, ?3, ?4,
                 'payload', 'parts/orders.enc', 'payload-version', ?5,
                 'present', ?6, '{}'
             )",
            params![
                Uuid::new_v4().to_string(),
                recovery_point_id.as_str(),
                replica_id,
                part_id,
                "d".repeat(64),
                committed_at_ms
            ],
        )
        .expect("insert payload");
    connection
        .execute(
            "INSERT INTO catalog_stored_objects(
                 id, recovery_point_id, artifact_replica_id,
                 object_role, object_key, object_version, checksum_blake3,
                 state, observed_at_ms, metadata_json
             ) VALUES (
                 ?1, ?2, ?3,
                 'set_manifest', 'set-manifest.json', 'manifest-version', ?4,
                 'present', ?5, '{}'
             )",
            params![
                Uuid::new_v4().to_string(),
                recovery_point_id.as_str(),
                replica_id,
                "e".repeat(64),
                committed_at_ms
            ],
        )
        .expect("insert manifest");
}

fn frame(payload: Value) -> Vec<u8> {
    encode_request(
        &RequestEnvelope::new(
            IpcRequestId::new(),
            ApplicationCommand::RetentionDryRun,
            None,
            1_000,
            2_000,
            payload,
        )
        .expect("request"),
    )
    .expect("encoded request")
}

fn mutation_frame(
    command: ApplicationCommand,
    idempotency_key: &str,
    issued_at_ms: i64,
    payload: Value,
) -> Vec<u8> {
    encode_request(
        &RequestEnvelope::new(
            IpcRequestId::new(),
            command,
            Some(idempotency_key.to_owned()),
            issued_at_ms,
            issued_at_ms + 100_000,
            payload,
        )
        .expect("mutation request"),
    )
    .expect("encoded mutation request")
}

fn authorized(frame: &[u8]) -> dbbackup_service::ipc::AuthorizedRequest {
    authorized_at(frame, 1_500)
}

fn authorized_at(frame: &[u8], now_ms: i64) -> dbbackup_service::ipc::AuthorizedRequest {
    authorize_and_decode(&policy(), &owner_identity(), frame, now_ms).expect("authorized request")
}

fn policy() -> AuthorizationPolicy {
    AuthorizationPolicy::new(
        sid(SERVICE_SID),
        sid(OWNER_SID),
        sid(OWNER_LOGON_SID),
        10,
        Vec::new(),
    )
    .expect("authorization policy")
}

fn owner_identity() -> TokenIdentity {
    TokenIdentity::new(sid(OWNER_SID), sid(OWNER_LOGON_SID), 10, false, true)
}

fn sid(value: &str) -> WindowsSid {
    WindowsSid::parse(value).expect("canonical SID")
}

const fn rehearsal_authority() -> AuthoritySnapshot {
    AuthoritySnapshot::new(
        ExecutionAuthority::LegacyDirect,
        CutoverMode::RehearsalOnly,
        false,
        false,
    )
}

const fn fixed_now_ms() -> i64 {
    NOW_MS
}

fn direct(path: &std::path::Path) -> Connection {
    let connection = Connection::open(path).expect("open direct connection");
    connection
        .execute_batch("PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")
        .expect("configure direct connection");
    connection
}

fn operation_count(path: &std::path::Path) -> i64 {
    direct(path)
        .query_row("SELECT COUNT(*) FROM operation_runs", [], |row| row.get(0))
        .expect("operation count")
}

fn work_count(path: &std::path::Path) -> i64 {
    direct(path)
        .query_row("SELECT COUNT(*) FROM work_items", [], |row| row.get(0))
        .expect("work count")
}

fn confirmation_count(path: &std::path::Path) -> i64 {
    direct(path)
        .query_row("SELECT COUNT(*) FROM confirmation_tokens", [], |row| {
            row.get(0)
        })
        .expect("confirmation count")
}

fn database_files(path: &std::path::Path) -> Vec<PathBuf> {
    ["", "-wal", "-shm"]
        .into_iter()
        .map(|suffix| PathBuf::from(format!("{}{}", path.display(), suffix)))
        .filter(|candidate| candidate.exists())
        .collect()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|candidate| candidate == needle)
}

struct NoopRuntime;

impl HostRuntime for NoopRuntime {
    fn started(&self, _diagnostics_only: bool) -> Result<(), dbbackup_service::host::HostError> {
        Ok(())
    }

    fn stop_accepting_ipc(&self) -> Result<(), dbbackup_service::host::HostError> {
        Ok(())
    }

    fn stop_claiming_work(&self) -> Result<(), dbbackup_service::host::HostError> {
        Ok(())
    }

    fn drain_clients(&self) -> Result<(), dbbackup_service::host::HostError> {
        Ok(())
    }

    fn checkpointed(&self) -> Result<(), dbbackup_service::host::HostError> {
        Ok(())
    }

    fn stopped(&self, _reason: dbbackup_service::host::ShutdownReason) {}
}
