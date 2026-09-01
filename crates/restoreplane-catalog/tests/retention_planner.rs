use restoreplane_catalog::{
    rusqlite::{params, Connection},
    ClaimRequest, ClaimTerminalOutcome, ConnectionProfileSpec, DatabaseTargetSpec, DestinationSpec,
    OperationalStore, PlanDraftSpec, ReplicaRepairSecretRefsV2, ReplicaRepairWorkSpec,
    RetentionDeleteSettlementOutcome, RetentionDeleteSettlementSpec, RetentionMaintenanceSpec,
    RetentionSafetyControlSpec, RetentionTombstoneSpec,
};
use restoreplane_domain::{
    BackupPlanId, ConnectionProfileId, DatabaseTargetId, DestinationId, DestinationRequirement,
    OperationRunId, PlanOccurrenceId, PlanRevisionId, RecoveryPointId,
};
use restoreplane_pipeline::{
    DeleteObjectDispositionV2, DeleteObjectObservationV2, RetentionDeleteReportV2,
    RetentionDispositionV2, RetentionObjectRoleV2, RetentionReasonV2,
};
use serde_json::Value;
use tempfile::TempDir;
use uuid::Uuid;

const OLD_COMMITTED_AT_MS: i64 = 86_400_000;
const NEW_COMMITTED_AT_MS: i64 = 172_800_000;
const NOW_MS: i64 = 259_200_000;
const GRACE_PERIOD_MS: i64 = 10_000;

#[test]
fn bounded_retention_cursor_rotates_without_starving_later_candidates() {
    let fixture = Fixture::new();
    let maintenance = fixture.store.retention_runtime();

    let first = maintenance
        .reconcile(RetentionMaintenanceSpec::new(NOW_MS, GRACE_PERIOD_MS, 1))
        .expect("evaluate oldest bounded candidate");
    assert_eq!(first.candidates_evaluated(), 1);
    assert_eq!(first.tombstones_staged(), 1);

    let second = maintenance
        .reconcile(RetentionMaintenanceSpec::new(
            NOW_MS + 1,
            GRACE_PERIOD_MS,
            1,
        ))
        .expect("advance to later bounded candidate");
    assert_eq!(second.candidates_evaluated(), 1);
    assert_eq!(second.preserved(), 1);

    let wrapped = maintenance
        .reconcile(RetentionMaintenanceSpec::new(
            NOW_MS + GRACE_PERIOD_MS,
            GRACE_PERIOD_MS,
            1,
        ))
        .expect("wrap cursor to eligible oldest candidate");
    assert_eq!(wrapped.candidates_evaluated(), 1);
    assert_eq!(wrapped.deletes_enqueued(), 1);
}

#[test]
fn published_policy_automatically_stages_grace_and_enqueues_auditable_delete_work() {
    let fixture = Fixture::new();
    let maintenance = fixture.store.retention_runtime();

    let staged = maintenance
        .reconcile(RetentionMaintenanceSpec::new(NOW_MS, GRACE_PERIOD_MS, 64))
        .expect("stage automatic tombstone");
    assert_eq!(staged.candidates_evaluated(), 2);
    assert_eq!(staged.tombstones_staged(), 1);
    assert_eq!(staged.deletes_enqueued(), 0);
    assert_eq!(staged.preserved(), 1);
    assert_eq!(staged.failures(), 0);

    let operation_before: (String, String, String) = direct(&fixture.db_path)
        .query_row(
            "SELECT id, state, request_json
             FROM operation_runs
             WHERE operation_kind = 'retention_delete'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("automatic tombstone operation");
    assert_eq!(operation_before.1, "created");
    let tombstone: Value = serde_json::from_str(&operation_before.2).expect("tombstone document");
    assert_eq!(tombstone["schema_version"], 1);
    assert_eq!(tombstone["phase"], "tombstoned");

    let waiting = maintenance
        .reconcile(RetentionMaintenanceSpec::new(
            NOW_MS + GRACE_PERIOD_MS - 1,
            GRACE_PERIOD_MS,
            64,
        ))
        .expect("grace remains active");
    assert_eq!(waiting.waiting_for_grace(), 1);
    assert_eq!(waiting.deletes_enqueued(), 0);

    let enqueued = maintenance
        .reconcile(RetentionMaintenanceSpec::new(
            NOW_MS + GRACE_PERIOD_MS,
            GRACE_PERIOD_MS,
            64,
        ))
        .expect("published policy authorizes durable delete");
    assert_eq!(enqueued.deletes_enqueued(), 1);
    assert_eq!(enqueued.failures(), 0);
    let replayed = maintenance
        .reconcile(RetentionMaintenanceSpec::new(
            NOW_MS + GRACE_PERIOD_MS + 1,
            GRACE_PERIOD_MS,
            64,
        ))
        .expect("idempotent policy maintenance replay");
    assert_eq!(replayed.deletes_enqueued(), 0);
    assert_eq!(replayed.already_active(), 1);

    let connection = direct(&fixture.db_path);
    let operation_after: (String, String, String) = connection
        .query_row(
            "SELECT id, state, request_json
             FROM operation_runs
             WHERE operation_kind = 'retention_delete'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("queued automatic delete");
    assert_eq!(operation_after.0, operation_before.0);
    assert_eq!(operation_after.1, "queued");
    let request: Value =
        serde_json::from_str(&operation_after.2).expect("automatic delete request");
    assert_eq!(request["schema_version"], 3);
    assert_eq!(request["authority_kind"], "published_retention_policy");
    let policy_document = &request["policy_authorization"]["document"];
    assert_eq!(
        policy_document["policy"]["retention_policy_json"],
        r#"{"daily":1,"weekly":0,"monthly":0}"#
    );
    assert!(policy_document["policy"]["plan_revision_id"]
        .as_str()
        .and_then(|value| Uuid::parse_str(value).ok())
        .is_some());
    let policy_document_digest = blake3::hash(
        &serde_json::to_vec(policy_document).expect("canonical policy authorization document"),
    )
    .to_hex()
    .to_string();
    assert_eq!(
        request["policy_authorization"]["authorization_digest_blake3"],
        policy_document_digest
    );
    let expected_authorization_id = Uuid::new_v5(
        &Uuid::parse_str(&operation_after.0).expect("operation UUID"),
        policy_document_digest.as_bytes(),
    )
    .to_string();
    assert_eq!(
        request["policy_authorization"]["authorization_id"],
        expected_authorization_id
    );
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM confirmation_tokens", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("confirmation count"),
        0
    );
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM ipc_request_ledger", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("IPC ledger count"),
        0
    );
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM work_items", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("work count"),
        1
    );
    drop(connection);

    let runtime = fixture.store.runtime();
    let claim = runtime
        .claim_next(
            ClaimRequest::new(
                "retention-worker-a",
                "retention-boot-a",
                NOW_MS + GRACE_PERIOD_MS,
                GRACE_PERIOD_MS,
            )
            .for_operation_kinds(["retention_delete"]),
        )
        .expect("claim automatic delete")
        .expect("queued delete exists");
    let execution = runtime
        .load_claimed_retention_delete_v2(&claim, NOW_MS + GRACE_PERIOD_MS)
        .expect("published policy authorization passes fail-closed loader");
    assert_eq!(
        execution.recovery_point_id(),
        &fixture.old_recovery_point_id
    );
    assert_eq!(
        execution.destination_fingerprint(),
        fixture.destination_fingerprint
    );
    runtime
        .start_claim(&claim, NOW_MS + GRACE_PERIOD_MS)
        .expect("start automatic delete");
    let settled = runtime
        .settle_retention_delete(
            &claim,
            RetentionDeleteSettlementSpec::new(
                fixture.old_recovery_point_id.clone(),
                fixture.destination_fingerprint.clone(),
                RetentionDeleteReportV2 {
                    observations: vec![
                        DeleteObjectObservationV2 {
                            object_key: "set-manifest.json".to_owned(),
                            object_version: Some("manifest-version".to_owned()),
                            role: RetentionObjectRoleV2::SetManifest,
                            disposition: DeleteObjectDispositionV2::Deleted,
                            diagnostic: None,
                        },
                        DeleteObjectObservationV2 {
                            object_key: "parts/orders.enc".to_owned(),
                            object_version: Some("payload-version".to_owned()),
                            role: RetentionObjectRoleV2::Payload,
                            disposition: DeleteObjectDispositionV2::Deleted,
                            diagnostic: None,
                        },
                    ],
                },
                NOW_MS + GRACE_PERIOD_MS + 1,
                NOW_MS + GRACE_PERIOD_MS + 2,
            ),
        )
        .expect("settle automatic policy delete");
    assert_eq!(
        settled,
        RetentionDeleteSettlementOutcome::Terminal(ClaimTerminalOutcome::Succeeded)
    );
}

#[test]
fn automatic_delete_rechecks_new_safety_controls_at_claim_time() {
    let fixture = Fixture::new();
    let maintenance = fixture.store.retention_runtime();
    maintenance
        .reconcile(RetentionMaintenanceSpec::new(NOW_MS, GRACE_PERIOD_MS, 64))
        .expect("stage automatic tombstone");
    maintenance
        .reconcile(RetentionMaintenanceSpec::new(
            NOW_MS + GRACE_PERIOD_MS,
            GRACE_PERIOD_MS,
            64,
        ))
        .expect("enqueue automatic delete");
    fixture
        .store
        .retention_planner()
        .set_safety_control(RetentionSafetyControlSpec::new(
            fixture.old_recovery_point_id.clone(),
            fixture.destination_fingerprint.clone(),
            true,
            false,
            NOW_MS + GRACE_PERIOD_MS + 1,
        ))
        .expect("pin before destructive claim");

    let runtime = fixture.store.runtime();
    let claim = runtime
        .claim_next(
            ClaimRequest::new(
                "retention-worker-b",
                "retention-boot-b",
                NOW_MS + GRACE_PERIOD_MS + 1,
                GRACE_PERIOD_MS,
            )
            .for_operation_kinds(["retention_delete"]),
        )
        .expect("claim query")
        .expect("queued delete exists");
    let error = runtime
        .load_claimed_retention_delete_v2(&claim, NOW_MS + GRACE_PERIOD_MS + 1)
        .expect_err("new pin must revoke automatic delete authority");
    assert_eq!(error.code(), "OPERATIONAL_RUNTIME_STATE_INVALID");
}

#[test]
fn automatic_policy_authorization_tampering_fails_closed_before_start() {
    let fixture = Fixture::new();
    let maintenance = fixture.store.retention_runtime();
    maintenance
        .reconcile(RetentionMaintenanceSpec::new(NOW_MS, GRACE_PERIOD_MS, 64))
        .expect("stage automatic tombstone");
    maintenance
        .reconcile(RetentionMaintenanceSpec::new(
            NOW_MS + GRACE_PERIOD_MS,
            GRACE_PERIOD_MS,
            64,
        ))
        .expect("enqueue automatic delete");

    let connection = direct(&fixture.db_path);
    let (operation_run_id, request_json): (String, String) = connection
        .query_row(
            "SELECT id, request_json
             FROM operation_runs
             WHERE operation_kind = 'retention_delete'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("automatic operation");
    let mut request: Value = serde_json::from_str(&request_json).expect("request JSON");
    request["policy_authorization"]["authorization_id"] = Value::String(Uuid::new_v4().to_string());
    connection
        .execute(
            "UPDATE operation_runs SET request_json = ?2 WHERE id = ?1",
            params![operation_run_id, request.to_string()],
        )
        .expect("tamper policy authorization ID");
    drop(connection);

    let runtime = fixture.store.runtime();
    let claim = runtime
        .claim_next(
            ClaimRequest::new(
                "retention-worker-tamper",
                "retention-boot-tamper",
                NOW_MS + GRACE_PERIOD_MS,
                GRACE_PERIOD_MS,
            )
            .for_operation_kinds(["retention_delete"]),
        )
        .expect("claim query")
        .expect("queued delete exists");
    let error = runtime
        .load_claimed_retention_delete_v2(&claim, NOW_MS + GRACE_PERIOD_MS)
        .expect_err("tampered automatic authority must not reach delete worker");
    assert_eq!(error.code(), "OPERATIONAL_RUNTIME_STATE_INVALID");
}

#[test]
fn automatic_delete_rechecks_safety_before_each_destructive_object_step() {
    let fixture = Fixture::new();
    let maintenance = fixture.store.retention_runtime();
    maintenance
        .reconcile(RetentionMaintenanceSpec::new(NOW_MS, GRACE_PERIOD_MS, 64))
        .expect("stage automatic tombstone");
    maintenance
        .reconcile(RetentionMaintenanceSpec::new(
            NOW_MS + GRACE_PERIOD_MS,
            GRACE_PERIOD_MS,
            64,
        ))
        .expect("enqueue automatic delete");

    let runtime = fixture.store.runtime();
    let claim = runtime
        .claim_next(
            ClaimRequest::new(
                "retention-worker-c",
                "retention-boot-c",
                NOW_MS + GRACE_PERIOD_MS,
                GRACE_PERIOD_MS,
            )
            .for_operation_kinds(["retention_delete"]),
        )
        .expect("claim query")
        .expect("queued delete exists");
    runtime
        .load_claimed_retention_delete_v2(&claim, NOW_MS + GRACE_PERIOD_MS)
        .expect("initial automatic policy authority");
    runtime
        .start_claim(&claim, NOW_MS + GRACE_PERIOD_MS)
        .expect("start destructive claim");
    fixture
        .store
        .retention_planner()
        .set_safety_control(RetentionSafetyControlSpec::new(
            fixture.old_recovery_point_id.clone(),
            fixture.destination_fingerprint.clone(),
            false,
            true,
            NOW_MS + GRACE_PERIOD_MS + 1,
        ))
        .expect("legal hold appears during execution");

    let error = runtime
        .renew_destructive_step(
            &claim,
            NOW_MS + GRACE_PERIOD_MS + 1,
            NOW_MS + (2 * GRACE_PERIOD_MS),
        )
        .expect_err("legal hold must fence the next object deletion");
    assert_eq!(error.code(), "OPERATIONAL_RUNTIME_STATE_INVALID");
}

#[test]
fn dry_run_derives_every_safety_gate_and_exact_object_set_from_authoritative_state() {
    let fixture = Fixture::new();
    let planner = fixture.store.retention_planner();

    let current = planner
        .prepare_delete_plan(
            &fixture.new_recovery_point_id,
            &fixture.destination_fingerprint,
            NOW_MS,
            GRACE_PERIOD_MS,
        )
        .expect("newest daily generation is retained");
    assert_eq!(
        current.decision().disposition,
        RetentionDispositionV2::Preserve
    );
    assert_eq!(
        current.decision().reasons,
        vec![RetentionReasonV2::PolicyNotExpired]
    );

    planner
        .set_safety_control(RetentionSafetyControlSpec::new(
            fixture.old_recovery_point_id.clone(),
            fixture.destination_fingerprint.clone(),
            true,
            false,
            NOW_MS,
        ))
        .expect("pin recovery point");
    assert_preserved_for(
        &planner
            .prepare_delete_plan(
                &fixture.old_recovery_point_id,
                &fixture.destination_fingerprint,
                NOW_MS,
                GRACE_PERIOD_MS,
            )
            .expect("pinned dry run"),
        RetentionReasonV2::Pinned,
    );

    planner
        .set_safety_control(RetentionSafetyControlSpec::new(
            fixture.old_recovery_point_id.clone(),
            fixture.destination_fingerprint.clone(),
            false,
            true,
            NOW_MS + 1,
        ))
        .expect("place legal hold");
    assert_preserved_for(
        &planner
            .prepare_delete_plan(
                &fixture.old_recovery_point_id,
                &fixture.destination_fingerprint,
                NOW_MS,
                GRACE_PERIOD_MS,
            )
            .expect("legal hold dry run"),
        RetentionReasonV2::LegalHold,
    );

    planner
        .set_safety_control(RetentionSafetyControlSpec::new(
            fixture.old_recovery_point_id.clone(),
            fixture.destination_fingerprint.clone(),
            false,
            false,
            NOW_MS + 2,
        ))
        .expect("release explicit controls");
    fixture.set_newer_compliance("non_compliant");
    let last_compliant = planner
        .prepare_delete_plan(
            &fixture.old_recovery_point_id,
            &fixture.destination_fingerprint,
            NOW_MS,
            GRACE_PERIOD_MS,
        )
        .expect("last compliant dry run");
    assert_preserved_for(
        &last_compliant,
        RetentionReasonV2::LastCompliantRecoveryPoint,
    );
    assert!(last_compliant.decision().would_break_compliance);
    fixture.set_newer_compliance("compliant");

    fixture.insert_active_restore();
    assert_preserved_for(
        &planner
            .prepare_delete_plan(
                &fixture.old_recovery_point_id,
                &fixture.destination_fingerprint,
                NOW_MS,
                GRACE_PERIOD_MS,
            )
            .expect("active restore dry run"),
        RetentionReasonV2::ActiveRestoreSource,
    );
    fixture.complete_restore();

    let repair = fixture
        .store
        .runtime()
        .enqueue_replica_repair(ReplicaRepairWorkSpec::new(
            fixture.old_recovery_point_id.clone(),
            fixture.destination_fingerprint.clone(),
            format!("local:{}", DestinationId::new().as_str()),
            ReplicaRepairSecretRefsV2::local(),
            NOW_MS,
        ))
        .expect("enqueue active repair");
    assert_preserved_for(
        &planner
            .prepare_delete_plan(
                &fixture.old_recovery_point_id,
                &fixture.destination_fingerprint,
                NOW_MS,
                GRACE_PERIOD_MS,
            )
            .expect("active repair dry run"),
        RetentionReasonV2::ActiveRepairSource,
    );
    fixture.complete_repair(repair.operation_run_id().as_str());

    fixture.set_object_lock("governance", None);
    assert_preserved_for(
        &planner
            .prepare_delete_plan(
                &fixture.old_recovery_point_id,
                &fixture.destination_fingerprint,
                NOW_MS,
                GRACE_PERIOD_MS,
            )
            .expect("missing Object Lock evidence dry run"),
        RetentionReasonV2::ObjectLockEvidenceMissing,
    );
    fixture.set_object_lock("governance", Some(NOW_MS + 1));
    assert_preserved_for(
        &planner
            .prepare_delete_plan(
                &fixture.old_recovery_point_id,
                &fixture.destination_fingerprint,
                NOW_MS,
                GRACE_PERIOD_MS,
            )
            .expect("active Object Lock dry run"),
        RetentionReasonV2::ObjectLockActive,
    );
    fixture.set_object_lock("none", None);

    let unstaged = planner
        .prepare_delete_plan(
            &fixture.old_recovery_point_id,
            &fixture.destination_fingerprint,
            NOW_MS,
            GRACE_PERIOD_MS,
        )
        .expect("expired candidate requires tombstone");
    assert_eq!(
        unstaged.decision().disposition,
        RetentionDispositionV2::TombstoneRequired
    );
    assert!(unstaged.decision().reasons.is_empty());

    fixture
        .store
        .ipc()
        .stage_retention_tombstone(RetentionTombstoneSpec::new(
            OperationRunId::new(),
            fixture.old_recovery_point_id.clone(),
            fixture.destination_fingerprint.clone(),
            GRACE_PERIOD_MS,
            NOW_MS,
        ))
        .expect("stage authoritative tombstone");
    let waiting = planner
        .prepare_delete_plan(
            &fixture.old_recovery_point_id,
            &fixture.destination_fingerprint,
            NOW_MS + GRACE_PERIOD_MS - 1,
            GRACE_PERIOD_MS,
        )
        .expect("grace period still active");
    assert_eq!(
        waiting.decision().disposition,
        RetentionDispositionV2::WaitForGrace
    );
    assert_eq!(
        waiting.decision().eligible_after_ms,
        Some(NOW_MS + GRACE_PERIOD_MS)
    );

    let eligible = planner
        .prepare_delete_plan(
            &fixture.old_recovery_point_id,
            &fixture.destination_fingerprint,
            NOW_MS + GRACE_PERIOD_MS,
            GRACE_PERIOD_MS,
        )
        .expect("grace period elapsed");
    assert_eq!(
        eligible.decision().disposition,
        RetentionDispositionV2::DeleteEligible
    );
    let authorization: Value = serde_json::from_str(
        &eligible
            .authorization()
            .to_json()
            .expect("authorization JSON"),
    )
    .expect("parse authorization JSON");
    assert_eq!(authorization["plan"]["pinned"], false);
    assert_eq!(authorization["plan"]["legal_hold"], false);
    assert_eq!(authorization["plan"]["active_restore_source"], false);
    assert_eq!(authorization["plan"]["active_repair_source"], false);
    assert_eq!(
        authorization["plan"]["objects"],
        serde_json::json!([
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
}

fn assert_preserved_for(
    plan: &restoreplane_pipeline::RetentionDeletePlanV2,
    reason: RetentionReasonV2,
) {
    assert_eq!(
        plan.decision().disposition,
        RetentionDispositionV2::Preserve
    );
    assert_eq!(plan.decision().reasons, vec![reason]);
}

struct Fixture {
    _temp_dir: TempDir,
    db_path: std::path::PathBuf,
    store: OperationalStore,
    connection_profile_id: ConnectionProfileId,
    destination_fingerprint: String,
    old_recovery_point_id: RecoveryPointId,
    old_replica_id: String,
    new_recovery_point_id: RecoveryPointId,
    new_restore_id: String,
}

impl Fixture {
    fn new() -> Self {
        let temp_dir = TempDir::new().expect("temp dir");
        let db_path = temp_dir.path().join("operational.sqlite3");
        let store = OperationalStore::open(db_path.clone()).expect("open operational store");
        let configuration = store.configuration();
        let connection_profile_id = ConnectionProfileId::new();
        let target_id = DatabaseTargetId::new();
        let destination_id = DestinationId::new();
        let destination_fingerprint = format!("local:{}", destination_id.as_str());
        let plan_id = BackupPlanId::new();
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
                connection_profile_id.clone(),
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
                plan_id,
                revision_id.clone(),
                "orders-nightly",
                1,
                "a".repeat(64),
                4,
            ))
            .expect("insert plan draft");
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
            let direct = direct(&db_path);
            direct
                .execute(
                    "UPDATE backup_plan_revisions
                     SET retention_policy_json = '{\"daily\":1,\"weekly\":0,\"monthly\":0}'
                     WHERE id = ?1",
                    [revision_id.as_str()],
                )
                .expect("set GFS policy");
            direct
                .execute(
                    "UPDATE destinations SET state = 'ready' WHERE id = ?1",
                    [destination_id.as_str()],
                )
                .expect("mark destination ready");
        }
        configuration
            .publish_revision(&revision_id, 7)
            .expect("publish plan revision");

        let old_recovery_point_id = RecoveryPointId::new();
        let new_recovery_point_id = RecoveryPointId::new();
        let old_replica_id = Uuid::new_v4().to_string();
        let new_replica_id = Uuid::new_v4().to_string();
        {
            let connection = direct(&db_path);
            seed_recovery_point(
                &connection,
                &revision_id,
                &target_id,
                &destination_fingerprint,
                &old_recovery_point_id,
                &old_replica_id,
                OLD_COMMITTED_AT_MS,
            );
            seed_recovery_point(
                &connection,
                &revision_id,
                &target_id,
                &destination_fingerprint,
                &new_recovery_point_id,
                &new_replica_id,
                NEW_COMMITTED_AT_MS,
            );
        }
        Self {
            _temp_dir: temp_dir,
            db_path,
            store,
            connection_profile_id,
            destination_fingerprint,
            old_recovery_point_id,
            old_replica_id,
            new_recovery_point_id,
            new_restore_id: Uuid::new_v4().to_string(),
        }
    }

    fn set_newer_compliance(&self, value: &str) {
        direct(&self.db_path)
            .execute(
                "UPDATE catalog_snapshot_projection
                 SET protection_compliance = ?1
                 WHERE recovery_point_id = ?2",
                params![value, self.new_recovery_point_id.as_str()],
            )
            .expect("change newer compliance");
    }

    fn insert_active_restore(&self) {
        direct(&self.db_path)
            .execute(
                "INSERT INTO restore_runs(
                     id, recovery_point_id, source_replica_id,
                     target_connection_profile_id, target_database_name,
                     target_fingerprint, purpose, conflict_policy,
                     eligibility_snapshot_json, state, created_at_ms, updated_at_ms
                 ) VALUES (
                     ?1, ?2, ?3, ?4, 'restore_orders',
                     'target:restore_orders', 'manual_validation', 'create_new',
                     '{}', 'queued', ?5, ?5
                 )",
                params![
                    self.new_restore_id,
                    self.old_recovery_point_id.as_str(),
                    self.old_replica_id,
                    self.connection_profile_id.as_str(),
                    NOW_MS
                ],
            )
            .expect("insert active restore");
    }

    fn complete_restore(&self) {
        direct(&self.db_path)
            .execute(
                "UPDATE restore_runs
                 SET state = 'succeeded', completed_at_ms = ?2, updated_at_ms = ?2
                 WHERE id = ?1",
                params![self.new_restore_id, NOW_MS + 1],
            )
            .expect("complete restore");
    }

    fn complete_repair(&self, operation_run_id: &str) {
        direct(&self.db_path)
            .execute(
                "UPDATE operation_runs
                 SET state = 'succeeded', completed_at_ms = ?2, updated_at_ms = ?2
                 WHERE id = ?1",
                params![operation_run_id, NOW_MS + 2],
            )
            .expect("complete repair");
    }

    fn set_object_lock(&self, capability: &str, retention_until_ms: Option<i64>) {
        direct(&self.db_path)
            .execute(
                "UPDATE catalog_artifact_replicas
                 SET immutability_capability = ?1, retention_until_ms = ?2
                 WHERE id = ?3",
                params![capability, retention_until_ms, self.old_replica_id],
            )
            .expect("change Object Lock evidence");
    }
}

#[test]
fn missing_plan_authority_fails_closed_instead_of_guessing_expiry() {
    let fixture = Fixture::new();
    direct(&fixture.db_path)
        .execute(
            "UPDATE catalog_recovery_points
             SET origin_backup_run_id = NULL
             WHERE id = ?1",
            [fixture.old_recovery_point_id.as_str()],
        )
        .expect("remove origin authority");

    let error = fixture
        .store
        .retention_planner()
        .prepare_delete_plan(
            &fixture.old_recovery_point_id,
            &fixture.destination_fingerprint,
            NOW_MS,
            GRACE_PERIOD_MS,
        )
        .expect_err("missing immutable plan evidence must fail closed");
    assert_eq!(error.code(), "RETENTION_AUTHORITY_INVALID");
}

#[allow(clippy::too_many_arguments)]
fn seed_recovery_point(
    connection: &Connection,
    revision_id: &PlanRevisionId,
    target_id: &DatabaseTargetId,
    destination_fingerprint: &str,
    recovery_point_id: &RecoveryPointId,
    replica_id: &str,
    committed_at_ms: i64,
) {
    let occurrence_id = PlanOccurrenceId::new();
    let backup_run_id = Uuid::new_v4().to_string();
    let part_id = Uuid::new_v4().to_string();
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
            "INSERT INTO catalog_artifact_parts(
                 id, recovery_point_id, part_role, logical_name,
                 required, dump_format, compression_format, encryption_format,
                 plaintext_checksum_blake3, plaintext_bytes, cipher_bytes,
                 metadata_json, created_at_ms
             ) VALUES (
                 ?1, ?2, 'database_dump', 'orders',
                 1, 'custom', 'zstd', 'chacha20poly1305-stream',
                 ?3, 5, 5, '{}', ?4
             )",
            params![
                part_id,
                recovery_point_id.as_str(),
                "c".repeat(64),
                committed_at_ms
            ],
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
                 'none', 'eligible', ?4, ?4, '{}'
             )",
            params![
                replica_id,
                recovery_point_id.as_str(),
                destination_fingerprint,
                committed_at_ms
            ],
        )
        .expect("insert artifact replica");
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
                "d".repeat(64),
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
                "e".repeat(64),
                committed_at_ms
            ],
        )
        .expect("insert set manifest");
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
}

fn direct(path: &std::path::Path) -> Connection {
    let connection = Connection::open(path).expect("open direct connection");
    connection
        .execute_batch("PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")
        .expect("configure direct connection");
    connection
}
