use std::collections::BTreeMap;

use dbbackup_catalog::{
    rusqlite::{params, Connection},
    BackupClaimContext, BackupWorkSpec, CancelRequestOutcome, ClaimCompletionSpec, ClaimRequest,
    ClaimTerminalOutcome, ClaimedWork, CompleteRecoveryPointSpec, ConnectionProfileSpec,
    DatabaseObjectDiscoverySpec, DatabaseTargetSpec, DestinationSpec, DiscoveredDatabaseObjectSpec,
    ExplicitBackupScopeSpec, OccurrenceSpec, OperationWorkSpec, OperationalStore, PlanDraftSpec,
    ProcessExitSpec, ProcessStartSpec, ReplicaRepairSecretRefsV2, ReplicaRepairWorkSpec,
    WHOLE_DATABASE_SELECTOR_HASH_BLAKE3,
};
use dbbackup_core::{ArtifactPartRole, ConsistencyGrade, Engine, PartSpec};
use dbbackup_domain::{
    BackupPlanId, BackupRunId, BackupScopeId, DatabaseTargetId, DestinationId,
    DestinationRequirement, OccurrenceIdentity, OccurrenceOrigin, OperationRunId, PlanOccurrenceId,
    PlanRevisionId, RecoveryPointId, WorkItemId,
};
use dbbackup_pipeline::{
    EncodingEvidenceV2, ManifestPartV2, NativeToolIdentityV2, ReaderStatusV2, RecoveryOriginV2,
    ReplicaCommitmentV2, ReplicaPublishOutcomeV2, ReplicaRequirementV2, SetManifestV2,
    SignedSetManifestV2, UploadReceipt,
};
use ed25519_dalek::SigningKey;
use tempfile::TempDir;
use uuid::Uuid;

#[test]
fn scheduled_catchup_and_manual_occurrence_upserts_are_idempotent() {
    let fixture = Fixture::new();
    fixture.seed_published_plan();
    let repository = fixture.store.runtime();

    let scheduled_identity = OccurrenceIdentity::scheduled(
        fixture.revision_id.clone(),
        OccurrenceOrigin::Scheduled,
        1_000,
    )
    .expect("scheduled identity");
    let scheduled_id = scheduled_identity.deterministic_id(Uuid::NAMESPACE_URL);

    let first = repository
        .upsert_occurrence(OccurrenceSpec::scheduled(
            scheduled_id.clone(),
            fixture.revision_id.clone(),
            OccurrenceOrigin::Scheduled,
            1_000,
            900,
        ))
        .expect("insert scheduled occurrence");
    let catchup = repository
        .upsert_occurrence(OccurrenceSpec::scheduled(
            scheduled_id.clone(),
            fixture.revision_id.clone(),
            OccurrenceOrigin::CatchUp,
            1_000,
            1_100,
        ))
        .expect("select existing scheduled identity");

    assert!(first.created());
    assert!(!catchup.created());
    assert_eq!(first.id(), catchup.id());

    let manual_identity = OccurrenceIdentity::manual("manual-request-01").expect("manual identity");
    let manual_id = manual_identity.deterministic_id(Uuid::NAMESPACE_URL);
    let manual_first = repository
        .upsert_occurrence(OccurrenceSpec::manual(
            manual_id.clone(),
            fixture.revision_id.clone(),
            "manual-request-01",
            1_200,
        ))
        .expect("insert manual occurrence");
    let manual_replay = repository
        .upsert_occurrence(OccurrenceSpec::manual(
            manual_id,
            fixture.revision_id.clone(),
            "manual-request-01",
            1_300,
        ))
        .expect("select existing manual occurrence");

    assert!(manual_first.created());
    assert!(!manual_replay.created());
    assert_eq!(repository.occurrence_count().expect("occurrence count"), 2);
}

#[test]
fn backup_work_subject_and_claim_fence_are_enforced_by_database_and_repository() {
    let fixture = Fixture::new();
    fixture.seed_published_plan();
    let repository = fixture.store.runtime();
    let occurrence_identity = OccurrenceIdentity::scheduled(
        fixture.revision_id.clone(),
        OccurrenceOrigin::Scheduled,
        2_000,
    )
    .expect("scheduled identity");
    let occurrence_id = occurrence_identity.deterministic_id(Uuid::NAMESPACE_URL);
    repository
        .upsert_occurrence(OccurrenceSpec::scheduled(
            occurrence_id.clone(),
            fixture.revision_id.clone(),
            OccurrenceOrigin::Scheduled,
            2_000,
            1_900,
        ))
        .expect("insert occurrence");

    let backup_run_id = BackupRunId::new();
    let work_item_id = WorkItemId::new();
    repository
        .enqueue_backup_work(BackupWorkSpec::new(
            backup_run_id.clone(),
            work_item_id.clone(),
            occurrence_id,
            fixture.revision_id.clone(),
            fixture.target_id.clone(),
            "postgresql",
            "source-fingerprint",
            1_950,
        ))
        .expect("enqueue backup work");

    let first = repository
        .claim_next(ClaimRequest::new("worker-a", "boot-a", 2_000, 100))
        .expect("claim first attempt")
        .expect("due work");
    assert_eq!(first.work_item_id(), &work_item_id);
    assert_eq!(first.attempt_no(), 1);
    assert_eq!(first.fencing_token(), 1);
    repository
        .heartbeat_claim(&first, 2_010, 2_110)
        .expect("current fence heartbeat");
    repository
        .requeue_claim(&first, 2_020, 2_021)
        .expect("retryable release");

    let second = repository
        .claim_next(ClaimRequest::new("worker-b", "boot-b", 2_021, 100))
        .expect("claim second attempt")
        .expect("requeued work");
    assert_eq!(second.attempt_no(), 2);
    assert_eq!(second.fencing_token(), 2);
    let stale = repository
        .heartbeat_claim(&first, 2_022, 2_122)
        .expect_err("old fence must not mutate current lease");
    assert_eq!(stale.code(), "WORK_ITEM_FENCE_STALE");
    repository
        .heartbeat_claim(&second, 2_023, 2_123)
        .expect("new fence heartbeat");
    assert_eq!(
        repository
            .attempt_count(&work_item_id)
            .expect("attempt count"),
        2
    );

    let direct = Connection::open(&fixture.db_path).expect("open direct invariant probe");
    direct
        .execute_batch("PRAGMA foreign_keys=ON;")
        .expect("enable foreign keys");
    let operation_run_id = OperationRunId::new();
    direct
        .execute(
            "INSERT INTO operation_runs(
                 id, operation_kind, subject_type, subject_id, request_json,
                 state, created_at_ms, updated_at_ms
             ) VALUES (?1, 'tool_rescan', 'service', 'local', '{}', 'queued', 2100, 2100)",
            [operation_run_id.as_str()],
        )
        .expect("insert operation subject");
    let invalid = direct.execute(
        "INSERT INTO work_items(
             id, kind, backup_run_id, operation_run_id, priority, state,
             available_at_ms, attempt_limit, created_at_ms, updated_at_ms
         ) VALUES (?1, 'backup', ?2, ?3, 100, 'queued', 2100, 3, 2100, 2100)",
        params![
            WorkItemId::new().as_str(),
            backup_run_id.as_str(),
            operation_run_id.as_str()
        ],
    );
    assert!(invalid.is_err(), "multiple work subjects must fail CHECK");
}

#[test]
fn generic_tool_rescan_operation_has_exactly_one_durable_work_subject() {
    let fixture = Fixture::new();
    let repository = fixture.store.runtime();
    let operation_run_id = OperationRunId::new();
    let work_item_id = WorkItemId::new();
    let spec = OperationWorkSpec::new(
        operation_run_id.clone(),
        work_item_id.clone(),
        "tool_rescan",
        "service",
        "local",
        r#"{"inventory":"native-tools"}"#,
        3_000,
    );

    let first = repository
        .enqueue_operation_work(spec.clone())
        .expect("enqueue operation work");
    let replay = repository
        .enqueue_operation_work(spec)
        .expect("idempotent operation replay");
    assert!(first.created());
    assert!(!replay.created());
    assert_eq!(first.operation_run_id(), &operation_run_id);
    assert_eq!(first.work_item_id(), &work_item_id);
    assert_eq!(replay.operation_run_id(), &operation_run_id);
    assert_eq!(replay.work_item_id(), &work_item_id);
    let conflict = repository
        .enqueue_operation_work(OperationWorkSpec::new(
            operation_run_id.clone(),
            work_item_id.clone(),
            "tool_rescan",
            "service",
            "local",
            r#"{"inventory":"different-request"}"#,
            3_000,
        ))
        .expect_err("same operation ID with a different request must conflict");
    assert_eq!(conflict.code(), "OPERATIONAL_RUNTIME_STATE_INVALID");

    let direct = Connection::open(&fixture.db_path).expect("open exact-subject probe");
    direct
        .execute_batch("PRAGMA foreign_keys=ON;")
        .expect("enable foreign keys");
    let subject: (String, i64, String) = direct
        .query_row(
            "SELECT
                 kind,
                 (backup_run_id IS NOT NULL)
                   + (restore_run_id IS NOT NULL)
                   + (verification_run_id IS NOT NULL)
                   + (catalog_rebuild_run_id IS NOT NULL)
                   + (operation_run_id IS NOT NULL),
                 operation_run_id
             FROM work_items
             WHERE id = ?1",
            [work_item_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("query work subject");
    assert_eq!(
        subject,
        ("operation".to_owned(), 1, operation_run_id.to_string())
    );
    let counts: (i64, i64) = direct
        .query_row(
            "SELECT
                 (SELECT COUNT(*) FROM operation_runs),
                 (SELECT COUNT(*) FROM work_items)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("query operation/work counts");
    assert_eq!(counts, (1, 1));
}

#[test]
fn running_in_process_work_can_track_multiple_sequential_native_phases() {
    let fixture = Fixture::new();
    let repository = fixture.store.runtime();
    let operation_run_id = OperationRunId::new();
    let work_item_id = WorkItemId::new();
    repository
        .enqueue_operation_work(OperationWorkSpec::new(
            operation_run_id,
            work_item_id.clone(),
            "tool_rescan",
            "service",
            "local",
            r#"{"inventory":"native-tools"}"#,
            3_200,
        ))
        .expect("enqueue operation work");
    let claim = repository
        .claim_next(ClaimRequest::new(
            "worker-phases",
            "boot-phases",
            3_210,
            1_000,
        ))
        .expect("claim operation")
        .expect("due operation");
    repository.start_claim(&claim, 3_211).expect("start work");
    repository
        .mark_in_process_claim_running(&claim, 3_212)
        .expect("mark in-process phase running");

    for (index, phase) in ["restore", "validation"].into_iter().enumerate() {
        let process_instance_id = Uuid::new_v4().to_string();
        let observed_at_ms = 3_220 + i64::try_from(index).expect("small index") * 10;
        repository
            .record_process_start(ProcessStartSpec::new(
                process_instance_id.clone(),
                claim.clone(),
                phase,
                9_000 + i64::try_from(index).expect("small index"),
                observed_at_ms - 1,
                "boot-phases",
                format!("job-object/{phase}"),
                std::env::current_exe()
                    .expect("current executable")
                    .to_string_lossy()
                    .into_owned(),
                None,
                observed_at_ms,
            ))
            .expect("record process after work entered running");
        repository
            .mark_process_running(&claim, &process_instance_id, observed_at_ms + 1)
            .expect("mark sequential process running");
        repository
            .record_process_exit(
                &claim,
                ProcessExitSpec::new(process_instance_id, observed_at_ms + 2, 0, "exited"),
            )
            .expect("record sequential process exit");
    }

    let direct = Connection::open(&fixture.db_path).expect("open process phase probe");
    let phases: String = direct
        .query_row(
            "SELECT GROUP_CONCAT(phase, ',')
             FROM (
                 SELECT phase
                 FROM process_instances
                 WHERE work_attempt_id = ?1
                 ORDER BY started_at_ms
             )",
            [claim.attempt_id().as_str()],
            |row| row.get(0),
        )
        .expect("query tracked phases");
    assert_eq!(phases, "restore,validation");
    let state: String = direct
        .query_row(
            "SELECT state FROM work_items WHERE id = ?1",
            [work_item_id.as_str()],
            |row| row.get(0),
        )
        .expect("query running state");
    assert_eq!(state, "running");
}

#[test]
fn replica_repair_is_a_deterministic_durable_operation_without_a_backup_run() {
    let fixture = Fixture::new();
    let repository = fixture.store.runtime();
    let recovery_point_id = RecoveryPointId::new();
    let repair = ReplicaRepairWorkSpec::new(
        recovery_point_id.clone(),
        "local:source",
        "local:target",
        ReplicaRepairSecretRefsV2::local(),
        3_500,
    );

    let first = repository
        .enqueue_replica_repair(repair.clone())
        .expect("enqueue replica repair");
    let replay = repository
        .enqueue_replica_repair(repair)
        .expect("idempotent replay");
    assert!(first.created());
    assert!(!replay.created());
    assert_eq!(first.operation_run_id(), replay.operation_run_id());
    assert_eq!(first.work_item_id(), replay.work_item_id());

    let direct = Connection::open(&fixture.db_path).expect("open repair probe");
    let row: (String, String, String, String, i64) = direct
        .query_row(
            "SELECT operation_runs.operation_kind, operation_runs.subject_type,
                    operation_runs.subject_id, operation_runs.request_json,
                    work_items.backup_run_id IS NULL
             FROM operation_runs
             JOIN work_items ON work_items.operation_run_id = operation_runs.id
             WHERE operation_runs.id = ?1",
            [first.operation_run_id().as_str()],
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
        .expect("repair operation row");
    assert_eq!(row.0, "replica_repair");
    assert_eq!(row.1, "recovery_point");
    assert_eq!(row.2, recovery_point_id.as_str());
    assert_eq!(row.4, 1, "repair must not create or rerun a DB dump");
    let request: serde_json::Value = serde_json::from_str(&row.3).expect("repair request JSON");
    assert_eq!(request["source_destination_fingerprint"], "local:source");
    assert_eq!(request["target_destination_fingerprint"], "local:target");
    assert_eq!(request["schema_version"], 2);
    assert!(request["source_reader_secret_ref_id"].is_null());
    assert!(request["target_writer_secret_ref_id"].is_null());
    assert!(request["target_reader_secret_ref_id"].is_null());

    let invalid = repository
        .enqueue_replica_repair(ReplicaRepairWorkSpec::new(
            RecoveryPointId::new(),
            "local:same",
            "local:same",
            ReplicaRepairSecretRefsV2::local(),
            3_500,
        ))
        .expect_err("repair source and target must be distinct");
    assert_eq!(invalid.code(), "OPERATIONAL_RUNTIME_STATE_INVALID");
}

#[test]
fn failed_backup_attempt_atomically_settles_run_occurrence_and_work_states() {
    let fixture = Fixture::new();
    fixture.seed_published_plan();
    let repository = fixture.store.runtime();
    let occurrence_identity = OccurrenceIdentity::scheduled(
        fixture.revision_id.clone(),
        OccurrenceOrigin::Scheduled,
        4_000,
    )
    .expect("scheduled identity");
    let occurrence_id = occurrence_identity.deterministic_id(Uuid::NAMESPACE_URL);
    repository
        .upsert_occurrence(OccurrenceSpec::scheduled(
            occurrence_id.clone(),
            fixture.revision_id.clone(),
            OccurrenceOrigin::Scheduled,
            4_000,
            3_900,
        ))
        .expect("insert occurrence");
    let backup_run_id = BackupRunId::new();
    let work_item_id = WorkItemId::new();
    repository
        .enqueue_backup_work(BackupWorkSpec::new(
            backup_run_id.clone(),
            work_item_id.clone(),
            occurrence_id.clone(),
            fixture.revision_id.clone(),
            fixture.target_id.clone(),
            "postgresql",
            "source-fingerprint",
            3_950,
        ))
        .expect("enqueue backup work");
    let claim = repository
        .claim_next(ClaimRequest::new("worker-c", "boot-c", 4_000, 1_000))
        .expect("claim backup")
        .expect("due backup");
    let process_instance_id = Uuid::new_v4().to_string();
    repository
        .start_claim(&claim, 4_001)
        .expect("persist backup start");
    repository
        .record_process_start(ProcessStartSpec::new(
            process_instance_id.clone(),
            claim.clone(),
            "native_dump",
            8_888,
            3_999,
            "boot-c",
            "job-object/backup-failure",
            std::env::current_exe()
                .expect("current executable")
                .to_string_lossy()
                .into_owned(),
            None,
            4_002,
        ))
        .expect("record backup process");
    repository
        .mark_process_running(&claim, &process_instance_id, 4_003)
        .expect("mark backup running");
    repository
        .record_process_exit(
            &claim,
            ProcessExitSpec::new(process_instance_id, 4_010, 2, "exited"),
        )
        .expect("record failed process exit");
    repository
        .complete_claim(ClaimCompletionSpec::failed(
            claim.clone(),
            4_011,
            "NATIVE_DUMP_FAILED",
            Some(r#"{"exitCode":2}"#.to_owned()),
        ))
        .expect("settle failed backup");
    let replay = repository
        .complete_claim(ClaimCompletionSpec::failed(
            claim,
            4_012,
            "NATIVE_DUMP_FAILED",
            None,
        ))
        .expect_err("terminal replay must be stale");
    assert_eq!(replay.code(), "WORK_ITEM_FENCE_STALE");

    let direct = Connection::open(&fixture.db_path).expect("open lifecycle probe");
    let states: (String, String, String, Option<String>, String) = direct
        .query_row(
            "SELECT
                 work_items.state,
                 work_attempts.state,
                 backup_runs.state,
                 backup_runs.error_code,
                 plan_occurrences.state
             FROM work_items
             JOIN work_attempts
               ON work_attempts.work_item_id = work_items.id
             JOIN backup_runs
               ON backup_runs.id = work_items.backup_run_id
             JOIN plan_occurrences
               ON plan_occurrences.id = backup_runs.plan_occurrence_id
             WHERE work_items.id = ?1",
            [work_item_id.as_str()],
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
        .expect("query settled states");
    assert_eq!(
        states,
        (
            "failed_terminal".to_owned(),
            "failed".to_owned(),
            "failed".to_owned(),
            Some("NATIVE_DUMP_FAILED".to_owned()),
            "failed".to_owned()
        )
    );
}

#[test]
fn active_backup_claim_atomically_commits_verified_recovery_point_and_measured_bytes() {
    let fixture = Fixture::new();
    fixture.seed_published_plan();
    let repository = fixture.store.runtime();
    let occurrence_id = OccurrenceIdentity::scheduled(
        fixture.revision_id.clone(),
        OccurrenceOrigin::Scheduled,
        5_000,
    )
    .expect("scheduled identity")
    .deterministic_id(Uuid::NAMESPACE_URL);
    repository
        .upsert_occurrence(OccurrenceSpec::scheduled(
            occurrence_id.clone(),
            fixture.revision_id.clone(),
            OccurrenceOrigin::Scheduled,
            5_000,
            4_900,
        ))
        .expect("insert occurrence");
    let backup_run_id = BackupRunId::new();
    let work_item_id = WorkItemId::new();
    repository
        .enqueue_backup_work(BackupWorkSpec::new(
            backup_run_id.clone(),
            work_item_id.clone(),
            occurrence_id.clone(),
            fixture.revision_id.clone(),
            fixture.target_id.clone(),
            "postgresql",
            "source-fingerprint",
            4_950,
        ))
        .expect("enqueue backup work");
    let claim = repository
        .claim_next(
            ClaimRequest::new("backup-worker", "backup-boot", 5_000, 10_000)
                .for_kind(dbbackup_domain::WorkItemKind::Backup),
        )
        .expect("claim backup")
        .expect("due backup");

    let context = repository
        .backup_claim_context(&claim, 5_001)
        .expect("load immutable backup context");
    assert_eq!(context.backup_run_id(), &backup_run_id);
    assert_eq!(context.plan_occurrence_id(), &occurrence_id);
    assert_eq!(context.plan_revision_id(), &fixture.revision_id);
    assert_eq!(context.database_target_id(), &fixture.target_id);
    assert_eq!(context.connection_profile_id(), &fixture.connection_id);
    assert_eq!(context.engine(), "postgresql");
    assert_eq!(context.engine_object_id(), "oid:100");
    assert_eq!(context.target_display_name(), "orders");
    assert_eq!(context.source_fingerprint(), "source-fingerprint");

    repository.start_claim(&claim, 5_002).expect("start backup");
    repository
        .mark_in_process_claim_running(&claim, 5_003)
        .expect("mark service-owned backup running");
    repository
        .begin_claim_commit(&claim, 5_010)
        .expect("begin irreversible publish boundary");
    let cancel_outcome = repository
        .request_cancel(&work_item_id, claim.fencing_token(), 5_011)
        .expect("committing backup remains settleable after a late cancel request");
    assert_eq!(cancel_outcome, CancelRequestOutcome::CommitInProgress);
    let recovery_point_id = RecoveryPointId::new();
    let spec = fixture.verified_recovery_spec_with_scope(
        &occurrence_id,
        &backup_run_id,
        &recovery_point_id,
        5_011,
        5_012,
        ManifestScopeEvidence::from_context(&context),
    );
    let outcome = repository
        .commit_backup_claim(
            &claim,
            spec,
            ClaimTerminalOutcome::Succeeded,
            5_012,
            Some(r#"{"schemaVersion":1,"replicasCommitted":1}"#.to_owned()),
        )
        .expect("commit recovery point and claim together");
    assert_eq!(outcome, ClaimTerminalOutcome::Succeeded);

    let direct = Connection::open(&fixture.db_path).expect("open commit probe");
    let row: (
        String,
        String,
        String,
        Option<String>,
        Option<i64>,
        String,
        i64,
    ) = direct
        .query_row(
            "SELECT work_items.state, work_attempts.state, backup_runs.state,
                    backup_runs.recovery_point_id, backup_runs.plaintext_bytes,
                    plan_occurrences.state,
                    (SELECT COUNT(*) FROM catalog_recovery_points)
             FROM work_items
             JOIN work_attempts ON work_attempts.work_item_id = work_items.id
             JOIN backup_runs ON backup_runs.id = work_items.backup_run_id
             JOIN plan_occurrences ON plan_occurrences.id = backup_runs.plan_occurrence_id
             WHERE work_items.id = ?1",
            [work_item_id.as_str()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .expect("query committed backup");
    assert_eq!(
        row,
        (
            "succeeded".to_owned(),
            "succeeded".to_owned(),
            "succeeded".to_owned(),
            Some(recovery_point_id.to_string()),
            Some(128),
            "succeeded".to_owned(),
            1,
        )
    );

    let stale = repository
        .backup_claim_context(&claim, 5_013)
        .expect_err("terminal claim cannot be reused");
    assert_eq!(stale.code(), "WORK_ITEM_FENCE_STALE");
}

#[test]
fn backup_commit_rejects_signed_manifest_scope_target_mismatch_atomically() {
    assert_scope_manifest_mismatch_rejected(ScopeManifestMutation::TargetId);
}

#[test]
fn backup_commit_rejects_signed_manifest_scope_source_fingerprint_mismatch_atomically() {
    assert_scope_manifest_mismatch_rejected(ScopeManifestMutation::SourceFingerprint);
}

#[test]
fn backup_commit_rejects_signed_manifest_scope_mode_mismatch_atomically() {
    assert_scope_manifest_mismatch_rejected(ScopeManifestMutation::Mode);
}

#[test]
fn backup_commit_rejects_signed_manifest_scope_snapshot_hash_mismatch_atomically() {
    assert_scope_manifest_mismatch_rejected(ScopeManifestMutation::SnapshotHash);
}

#[test]
fn backup_commit_rejects_signed_manifest_scope_object_count_mismatch_atomically() {
    assert_scope_manifest_mismatch_rejected(ScopeManifestMutation::ObjectCount);
}

#[test]
fn expired_backup_commit_reconciliation_cannot_bypass_scope_integrity_validation() {
    let fixture = Fixture::new();
    fixture.seed_published_plan();
    let (occurrence_id, backup_run_id, _claim, context) =
        fixture.begin_backup_commit("source-fingerprint");
    let mut evidence = ManifestScopeEvidence::from_context(&context);
    evidence.source_fingerprint = "mismatched-scope-fingerprint".to_owned();
    let recovery_point_id = RecoveryPointId::new();
    let spec = fixture.verified_recovery_spec_with_scope(
        &occurrence_id,
        &backup_run_id,
        &recovery_point_id,
        5_011,
        15_001,
        evidence,
    );
    let runtime = fixture.store.runtime();
    let expired = runtime
        .expired_claims(15_001, 1)
        .expect("query expired commit")
        .into_iter()
        .next()
        .expect("expired committing backup");
    let error = runtime
        .reconcile_expired_backup_commit(&expired, spec, 15_001, false)
        .expect_err("startup reconciliation must enforce signed scope evidence");
    assert_eq!(error.code(), "OPERATIONAL_RUNTIME_STATE_INVALID");

    let direct = Connection::open(&fixture.db_path).expect("open reconcile rejection probe");
    let state: (String, Option<String>, i64) = direct
        .query_row(
            "SELECT state, recovery_point_id,
                    (SELECT COUNT(*) FROM catalog_recovery_points)
             FROM backup_runs WHERE id = ?1",
            [backup_run_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("query reconcile rejection state");
    assert_eq!(state, ("committing".to_owned(), None, 0));
}

#[test]
fn backup_commit_accepts_complete_signed_partial_scope_evidence() {
    let fixture = Fixture::new();
    let source_fingerprint = fixture.seed_partial_published_plan();
    let (occurrence_id, backup_run_id, claim, context) =
        fixture.begin_backup_commit(&source_fingerprint);
    assert_eq!(context.scope_mode(), "explicit_tables");
    assert_eq!(context.resolved_object_count(), 1);

    let recovery_point_id = RecoveryPointId::new();
    let spec = fixture.verified_recovery_spec_with_scope(
        &occurrence_id,
        &backup_run_id,
        &recovery_point_id,
        5_011,
        5_012,
        ManifestScopeEvidence::from_context(&context),
    );
    fixture
        .store
        .runtime()
        .commit_backup_claim(&claim, spec, ClaimTerminalOutcome::Succeeded, 5_012, None)
        .expect("commit partial recovery point with exact immutable scope evidence");

    let direct = Connection::open(&fixture.db_path).expect("open partial commit probe");
    let linked_scope: (String, String, i64) = direct
        .query_row(
            "SELECT run_scope.backup_scope_id, run_scope.scope_mode,
                    run_scope.resolved_object_count
             FROM backup_runs AS run
             JOIN backup_run_scopes AS run_scope ON run_scope.backup_run_id = run.id
             WHERE run.id = ?1 AND run.recovery_point_id = ?2",
            params![backup_run_id.as_str(), recovery_point_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("query partial recovery point scope link");
    assert_eq!(
        linked_scope,
        (
            context.backup_scope_id().to_string(),
            "explicit_tables".to_owned(),
            1,
        )
    );
}

#[test]
fn backup_commit_preserves_legacy_whole_scope_manifest_compatibility() {
    let fixture = Fixture::new();
    fixture.seed_published_plan();
    let (occurrence_id, backup_run_id, claim, context) =
        fixture.begin_backup_commit("source-fingerprint");
    assert_eq!(context.scope_mode(), "whole_database");
    assert_eq!(context.resolved_object_count(), 0);
    assert_eq!(
        context.scope_snapshot_hash_blake3(),
        WHOLE_DATABASE_SELECTOR_HASH_BLAKE3
    );

    let recovery_point_id = RecoveryPointId::new();
    let spec = fixture.verified_recovery_spec_with_scope(
        &occurrence_id,
        &backup_run_id,
        &recovery_point_id,
        5_011,
        5_012,
        ManifestScopeEvidence::legacy_whole(&context),
    );
    fixture
        .store
        .runtime()
        .commit_backup_claim(&claim, spec, ClaimTerminalOutcome::Succeeded, 5_012, None)
        .expect("legacy whole-database manifest remains commit-compatible");
}

#[test]
fn signed_scope_snapshot_body_must_match_its_hash_mode_and_object_count() {
    let fixture = Fixture::new();
    let source_fingerprint = fixture.seed_partial_published_plan();
    let (occurrence_id, backup_run_id, _claim, context) =
        fixture.begin_backup_commit(&source_fingerprint);
    let original: serde_json::Value =
        serde_json::from_str(context.scope_snapshot_json()).expect("parse immutable snapshot");

    let mut hash_drift = original.clone();
    hash_drift["objects"][0]["name"] = serde_json::Value::String("other_table".to_owned());

    let mut mode_drift = original.clone();
    mode_drift["mode"] = serde_json::Value::String("whole_database".to_owned());

    let mut count_drift = original;
    let duplicate = count_drift["objects"][0].clone();
    count_drift["objects"]
        .as_array_mut()
        .expect("snapshot objects")
        .push(duplicate);

    for (label, snapshot, replace_hash) in [
        ("hash", hash_drift, false),
        ("mode", mode_drift, true),
        ("count", count_drift, true),
    ] {
        let snapshot = serde_json::to_string(&snapshot).expect("canonical snapshot mutation");
        let mut evidence = ManifestScopeEvidence::from_context(&context);
        evidence
            .options
            .insert("backupScopeSnapshot".to_owned(), snapshot.clone());
        if replace_hash {
            evidence.options.insert(
                "backupScopeSnapshotHashBlake3".to_owned(),
                blake3::hash(snapshot.as_bytes()).to_hex().to_string(),
            );
        }
        let error = fixture
            .try_verified_recovery_spec_with_scope(
                &occurrence_id,
                &backup_run_id,
                &RecoveryPointId::new(),
                5_011,
                5_012,
                evidence,
            )
            .expect_err("scope snapshot body drift must be rejected");
        assert_eq!(
            error.code(),
            "CATALOG_PROJECTION_INPUT_INVALID",
            "unexpected parser result for {label} drift"
        );
    }
}

#[test]
fn partial_scope_manifest_cannot_omit_any_signed_scope_evidence_field() {
    let fixture = Fixture::new();
    let source_fingerprint = fixture.seed_partial_published_plan();
    let (occurrence_id, backup_run_id, _claim, context) =
        fixture.begin_backup_commit(&source_fingerprint);
    let mut evidence = ManifestScopeEvidence::from_context(&context);
    evidence.options.remove("backupScopeSnapshot");
    let error = fixture
        .try_verified_recovery_spec_with_scope(
            &occurrence_id,
            &backup_run_id,
            &RecoveryPointId::new(),
            5_011,
            5_012,
            evidence,
        )
        .expect_err("partial scope evidence must be all-or-none");
    assert_eq!(error.code(), "CATALOG_PROJECTION_INPUT_INVALID");
}

#[derive(Clone, Copy)]
enum ScopeManifestMutation {
    TargetId,
    SourceFingerprint,
    Mode,
    SnapshotHash,
    ObjectCount,
}

fn assert_scope_manifest_mismatch_rejected(mutation: ScopeManifestMutation) {
    let fixture = Fixture::new();
    fixture.seed_published_plan();
    let (occurrence_id, backup_run_id, claim, context) =
        fixture.begin_backup_commit("source-fingerprint");
    let mut evidence = ManifestScopeEvidence::from_context(&context);
    match mutation {
        ScopeManifestMutation::TargetId => {
            evidence.target_id = BackupScopeId::new();
            evidence
                .options
                .insert("backupScopeId".to_owned(), evidence.target_id.to_string());
        }
        ScopeManifestMutation::SourceFingerprint => {
            evidence.source_fingerprint = "mismatched-scope-fingerprint".to_owned();
        }
        ScopeManifestMutation::Mode => {
            let snapshot = r#"{"mode":"explicit_tables","objects":[]}"#;
            evidence
                .options
                .insert("backupScopeMode".to_owned(), "explicit_tables".to_owned());
            evidence
                .options
                .insert("backupScopeSnapshot".to_owned(), snapshot.to_owned());
            evidence.options.insert(
                "backupScopeSnapshotHashBlake3".to_owned(),
                blake3::hash(snapshot.as_bytes()).to_hex().to_string(),
            );
        }
        ScopeManifestMutation::SnapshotHash => {
            let mismatched_hash = "f".repeat(64);
            evidence.options.insert(
                "backupScopeSnapshotHashBlake3".to_owned(),
                mismatched_hash.clone(),
            );
            evidence.options.insert(
                "backupScopeSnapshot".to_owned(),
                format!(r#"{{"blake3":"{mismatched_hash}"}}"#),
            );
        }
        ScopeManifestMutation::ObjectCount => {
            let snapshot = r#"{"mode":"whole_database","objects":[{}]}"#;
            evidence
                .options
                .insert("backupScopeObjectCount".to_owned(), "1".to_owned());
            evidence
                .options
                .insert("backupScopeSnapshot".to_owned(), snapshot.to_owned());
            evidence.options.insert(
                "backupScopeSnapshotHashBlake3".to_owned(),
                blake3::hash(snapshot.as_bytes()).to_hex().to_string(),
            );
        }
    }
    let recovery_point_id = RecoveryPointId::new();
    let spec = fixture.verified_recovery_spec_with_scope(
        &occurrence_id,
        &backup_run_id,
        &recovery_point_id,
        5_011,
        5_012,
        evidence,
    );
    let error = fixture
        .store
        .runtime()
        .commit_backup_claim(&claim, spec, ClaimTerminalOutcome::Succeeded, 5_012, None)
        .expect_err("scope evidence mismatch must fail closed");
    assert_eq!(error.code(), "OPERATIONAL_RUNTIME_STATE_INVALID");

    let direct = Connection::open(&fixture.db_path).expect("open rejected commit probe");
    let state: (String, Option<String>, i64) = direct
        .query_row(
            "SELECT state, recovery_point_id,
                    (SELECT COUNT(*) FROM catalog_recovery_points)
             FROM backup_runs WHERE id = ?1",
            [backup_run_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("query rejected commit state");
    assert_eq!(state, ("committing".to_owned(), None, 0));
}

struct ManifestScopeEvidence {
    target_id: BackupScopeId,
    source_fingerprint: String,
    options: BTreeMap<String, String>,
}

impl ManifestScopeEvidence {
    fn from_context(context: &BackupClaimContext) -> Self {
        let mut options = BTreeMap::new();
        options.insert(
            "backupScopeId".to_owned(),
            context.backup_scope_id().to_string(),
        );
        options.insert(
            "backupScopeMode".to_owned(),
            context.scope_mode().to_owned(),
        );
        options.insert(
            "backupScopeSnapshotHashBlake3".to_owned(),
            context.scope_snapshot_hash_blake3().to_owned(),
        );
        options.insert(
            "backupScopeSnapshot".to_owned(),
            context.scope_snapshot_json().to_owned(),
        );
        options.insert(
            "backupScopeObjectCount".to_owned(),
            context.resolved_object_count().to_string(),
        );
        Self {
            target_id: context.backup_scope_id().clone(),
            source_fingerprint: context.source_fingerprint().to_owned(),
            options,
        }
    }

    fn legacy_whole(context: &BackupClaimContext) -> Self {
        Self {
            target_id: context.backup_scope_id().clone(),
            source_fingerprint: context.source_fingerprint().to_owned(),
            options: BTreeMap::new(),
        }
    }
}

struct Fixture {
    _temp_dir: TempDir,
    db_path: std::path::PathBuf,
    store: OperationalStore,
    connection_id: dbbackup_domain::ConnectionProfileId,
    target_id: DatabaseTargetId,
    destination_id: DestinationId,
    plan_id: BackupPlanId,
    revision_id: PlanRevisionId,
}

impl Fixture {
    fn new() -> Self {
        let temp_dir = TempDir::new().expect("temp directory");
        let db_path = temp_dir.path().join("operational.db");
        let store = OperationalStore::open(db_path.clone()).expect("open operational store");
        Self {
            _temp_dir: temp_dir,
            db_path,
            store,
            connection_id: dbbackup_domain::ConnectionProfileId::new(),
            target_id: DatabaseTargetId::new(),
            destination_id: DestinationId::new(),
            plan_id: BackupPlanId::new(),
            revision_id: PlanRevisionId::new(),
        }
    }

    fn seed_published_plan(&self) {
        let repository = self.store.configuration();
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
                self.target_id.clone(),
                self.connection_id.clone(),
                "oid:100",
                "orders",
                "orders",
                "source-fingerprint",
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
        repository
            .insert_plan_draft(PlanDraftSpec::manual(
                self.plan_id.clone(),
                self.revision_id.clone(),
                "nightly",
                1,
                "a".repeat(64),
                130,
            ))
            .expect("insert plan draft");
        repository
            .add_revision_target(&self.revision_id, &self.target_id, 131)
            .expect("add target membership");
        repository
            .add_revision_destination(
                &self.revision_id,
                &self.destination_id,
                DestinationRequirement::Required,
                132,
            )
            .expect("add destination membership");
        repository
            .publish_revision(&self.revision_id, 140)
            .expect("publish revision");
    }

    fn seed_partial_published_plan(&self) -> String {
        let repository = self.store.configuration();
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
                self.target_id.clone(),
                self.connection_id.clone(),
                "oid:100",
                "orders",
                "orders",
                "source-fingerprint",
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
        repository
            .insert_plan_draft(PlanDraftSpec::manual(
                self.plan_id.clone(),
                self.revision_id.clone(),
                "selected tables",
                1,
                "a".repeat(64),
                130,
            ))
            .expect("insert plan draft");

        let discovery_id = OperationRunId::new();
        let direct = Connection::open(&self.db_path).expect("open discovery writer");
        direct
            .execute_batch("PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")
            .expect("configure discovery writer");
        direct
            .execute(
                "INSERT INTO operation_runs(
                     id, operation_kind, subject_type, subject_id, request_json,
                     state, created_at_ms, updated_at_ms
                 ) VALUES (?1, 'connection_discovery', 'database_target', ?2,
                           '{\"kind\":\"backup_scope_object_discovery\"}',
                           'running', 125, 125)",
                params![discovery_id.as_str(), self.target_id.as_str()],
            )
            .expect("insert discovery operation");
        let scopes = self.store.backup_scopes();
        scopes
            .record_object_discovery(DatabaseObjectDiscoverySpec::new(
                discovery_id.clone(),
                self.target_id.clone(),
                "d".repeat(64),
                vec![DiscoveredDatabaseObjectSpec::eligible_table(
                    "opaque:orders",
                    "public",
                    "orders",
                    "e".repeat(64),
                )],
                126,
            ))
            .expect("record discovered table");
        let scope_id = BackupScopeId::new();
        let scope = scopes
            .create_explicit(ExplicitBackupScopeSpec::new(
                scope_id.clone(),
                self.target_id.clone(),
                "orders table",
                discovery_id,
                vec!["opaque:orders".to_owned()],
                127,
            ))
            .expect("create explicit scope");
        scopes
            .bind_revision_scope(&self.revision_id, &scope_id, 131)
            .expect("bind explicit scope");
        repository
            .add_revision_destination(
                &self.revision_id,
                &self.destination_id,
                DestinationRequirement::Required,
                132,
            )
            .expect("add destination membership");
        repository
            .publish_revision(&self.revision_id, 140)
            .expect("publish partial revision");
        scope.source_fingerprint().to_owned()
    }

    fn begin_backup_commit(
        &self,
        source_fingerprint: &str,
    ) -> (
        PlanOccurrenceId,
        BackupRunId,
        ClaimedWork,
        BackupClaimContext,
    ) {
        let repository = self.store.runtime();
        let occurrence_id = OccurrenceIdentity::scheduled(
            self.revision_id.clone(),
            OccurrenceOrigin::Scheduled,
            5_000,
        )
        .expect("scheduled identity")
        .deterministic_id(Uuid::NAMESPACE_URL);
        repository
            .upsert_occurrence(OccurrenceSpec::scheduled(
                occurrence_id.clone(),
                self.revision_id.clone(),
                OccurrenceOrigin::Scheduled,
                5_000,
                4_900,
            ))
            .expect("insert occurrence");
        let backup_run_id = BackupRunId::new();
        repository
            .enqueue_backup_work(BackupWorkSpec::new(
                backup_run_id.clone(),
                WorkItemId::new(),
                occurrence_id.clone(),
                self.revision_id.clone(),
                self.target_id.clone(),
                "postgresql",
                source_fingerprint,
                4_950,
            ))
            .expect("enqueue backup work");
        let claim = repository
            .claim_next(
                ClaimRequest::new("backup-worker", "backup-boot", 5_000, 10_000)
                    .for_kind(dbbackup_domain::WorkItemKind::Backup),
            )
            .expect("claim backup")
            .expect("due backup");
        let context = repository
            .backup_claim_context(&claim, 5_001)
            .expect("load immutable scope context");
        repository.start_claim(&claim, 5_002).expect("start backup");
        repository
            .mark_in_process_claim_running(&claim, 5_003)
            .expect("mark backup running");
        repository
            .begin_claim_commit(&claim, 5_010)
            .expect("begin commit boundary");
        (occurrence_id, backup_run_id, claim, context)
    }

    fn verified_recovery_spec_with_scope(
        &self,
        occurrence_id: &PlanOccurrenceId,
        backup_run_id: &BackupRunId,
        recovery_point_id: &RecoveryPointId,
        committed_at_ms: i64,
        ingested_at_ms: i64,
        scope: ManifestScopeEvidence,
    ) -> CompleteRecoveryPointSpec {
        self.try_verified_recovery_spec_with_scope(
            occurrence_id,
            backup_run_id,
            recovery_point_id,
            committed_at_ms,
            ingested_at_ms,
            scope,
        )
        .expect("verified recovery point")
    }

    fn try_verified_recovery_spec_with_scope(
        &self,
        occurrence_id: &PlanOccurrenceId,
        backup_run_id: &BackupRunId,
        recovery_point_id: &RecoveryPointId,
        committed_at_ms: i64,
        ingested_at_ms: i64,
        scope: ManifestScopeEvidence,
    ) -> Result<CompleteRecoveryPointSpec, dbbackup_catalog::RepositoryError> {
        let prefix = format!(
            "v2/{}/{}/{}",
            occurrence_id.as_str(),
            scope.target_id.as_str(),
            recovery_point_id.as_str()
        );
        let part_id = Uuid::new_v4();
        let manifest_key = format!("{prefix}/set-manifest.json");
        let manifest = SetManifestV2::new(
            RecoveryOriginV2::new(
                recovery_point_id.as_uuid(),
                "instance-a",
                occurrence_id.as_uuid(),
                backup_run_id.as_uuid(),
                scope.target_id.as_uuid(),
                scope.source_fingerprint,
                "orders",
            )
            .expect("recovery origin"),
            Engine::Postgresql,
            ConsistencyGrade::ApplicationConsistent,
            committed_at_ms,
            "signing-key-01",
            NativeToolIdentityV2::new("pg_dump", "17.4", "a".repeat(64)).expect("native tool"),
            scope.options,
            vec![ManifestPartV2::encoded(
                PartSpec::new(
                    part_id,
                    ArtifactPartRole::DatabaseDump,
                    "orders.dump",
                    true,
                    "custom",
                )
                .expect("part contract"),
                EncodingEvidenceV2::new(
                    "b".repeat(64),
                    "c".repeat(64),
                    128,
                    96,
                    "zstd",
                    "chacha20poly1305_stream",
                    1,
                )
                .expect("encoding evidence"),
                format!("{prefix}/parts/{part_id}.enc"),
            )
            .expect("manifest part")],
            vec![ReplicaCommitmentV2::new(
                format!("local:{}", self.destination_id),
                "local",
                ReplicaRequirementV2::Required,
                ReaderStatusV2::Eligible,
                "none",
                manifest_key.clone(),
            )
            .expect("replica")],
        )
        .expect("set manifest");
        let signing_key = SigningKey::from_bytes(&[23_u8; 32]);
        let signed = SignedSetManifestV2::create(manifest, &signing_key).expect("signed manifest");
        let manifest_bytes =
            u64::try_from(signed.to_json().expect("manifest json").len()).expect("manifest size");
        CompleteRecoveryPointSpec::from_verified_publish(
            &signed,
            &[ReplicaPublishOutcomeV2 {
                destination_fingerprint: format!("local:{}", self.destination_id),
                requirement: ReplicaRequirementV2::Required,
                reader_status: ReaderStatusV2::Eligible,
                payload_receipts: vec![UploadReceipt {
                    key: format!("{prefix}/parts/{part_id}.enc"),
                    bytes: 96,
                    version: None,
                }],
                manifest_receipt: Some(UploadReceipt {
                    key: manifest_key,
                    bytes: manifest_bytes,
                    version: None,
                }),
                attempts: 1,
                result: Ok(()),
            }],
            &signing_key.verifying_key(),
            ingested_at_ms,
        )
    }
}
