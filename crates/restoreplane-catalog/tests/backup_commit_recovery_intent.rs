use restoreplane_catalog::{
    BackupWorkSpec, ClaimRequest, ConnectionProfileSpec, DatabaseTargetSpec, DestinationSpec,
    OccurrenceSpec, OperationalStore, PlanDraftSpec,
};
use restoreplane_domain::{
    BackupPlanId, BackupRunId, ConnectionProfileId, DatabaseTargetId, DestinationId,
    DestinationRequirement, OccurrenceIdentity, OccurrenceOrigin, PlanRevisionId, RecoveryPointId,
    WorkItemId,
};
use tempfile::TempDir;
use uuid::Uuid;

#[test]
fn backup_commit_persists_an_exact_recovery_intent_for_expired_claim_reconciliation() {
    let temp = TempDir::new().expect("temp directory");
    let store = OperationalStore::open(temp.path().join("operational.db")).expect("open store");
    let configuration = store.configuration();
    let connection_id = ConnectionProfileId::new();
    let target_id = DatabaseTargetId::new();
    let destination_id = DestinationId::new();
    let plan_id = BackupPlanId::new();
    let revision_id = PlanRevisionId::new();
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
            serde_json::json!({ "path": temp.path().join("vault") }).to_string(),
            120,
        ))
        .expect("insert destination");
    configuration
        .insert_plan_draft(PlanDraftSpec::manual(
            plan_id,
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
    configuration
        .publish_revision(&revision_id, 140)
        .expect("publish revision");

    let occurrence_id =
        OccurrenceIdentity::scheduled(revision_id.clone(), OccurrenceOrigin::Scheduled, 1_000)
            .expect("occurrence identity")
            .deterministic_id(Uuid::NAMESPACE_URL);
    let backup_run_id = BackupRunId::new();
    let work_item_id = WorkItemId::new();
    let runtime = store.runtime();
    runtime
        .upsert_occurrence(OccurrenceSpec::scheduled(
            occurrence_id.clone(),
            revision_id.clone(),
            OccurrenceOrigin::Scheduled,
            1_000,
            900,
        ))
        .expect("insert occurrence");
    runtime
        .enqueue_backup_work(BackupWorkSpec::new(
            backup_run_id.clone(),
            work_item_id,
            occurrence_id.clone(),
            revision_id.clone(),
            target_id.clone(),
            "postgresql",
            "source-fingerprint",
            950,
        ))
        .expect("enqueue backup");
    let claim = runtime
        .claim_next(ClaimRequest::new("worker-old", "boot-old", 1_000, 100))
        .expect("claim backup")
        .expect("due backup");
    runtime.start_claim(&claim, 1_001).expect("start backup");
    runtime
        .mark_in_process_claim_running(&claim, 1_002)
        .expect("mark backup running");
    let recovery_point_id = RecoveryPointId::new();
    runtime
        .begin_backup_claim_commit(&claim, &recovery_point_id, 1_010)
        .expect("begin commit with recovery intent");

    let context = runtime
        .backup_commit_recovery_context(&claim)
        .expect("read expired commit recovery context");
    assert_eq!(context.backup_run_id(), &backup_run_id);
    assert_eq!(context.plan_occurrence_id(), &occurrence_id);
    assert_eq!(context.plan_revision_id(), &revision_id);
    assert_eq!(context.backup_scope_id().as_str(), target_id.as_str());
    assert_eq!(context.recovery_point_id(), Some(&recovery_point_id));
    let expected_manifest_key = format!(
        "v2/{}/{}/{}/set-manifest.json",
        occurrence_id, target_id, recovery_point_id
    );
    assert_eq!(
        context.manifest_object_key().as_deref(),
        Some(expected_manifest_key.as_str())
    );
}
