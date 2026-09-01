use std::sync::Arc;

use restoreplane_catalog::{
    ClaimRequest, ConnectionProfileSpec, DatabaseTargetSpec, DestinationSpec,
    OccurrenceMaterializationSpec, OperationWorkSpec, OperationalStore, PlanDraftSpec,
    ProcessStartSpec,
};
use restoreplane_domain::{
    BackupPlanId, ConnectionProfileId, DatabaseTargetId, DestinationId, DestinationRequirement,
    OccurrenceOrigin, OperationRunId, PlanRevisionId, WorkItemId,
};
use restoreplane_runtime::{
    ProcessInspector, ProcessObservation, ProcessProbe, ReconcileConfig, ReconcileError,
    ReconcileOutcome, RuntimeClock, StartupReconciler,
};
use tempfile::TempDir;
use uuid::Uuid;

#[test]
fn startup_reconciler_recovers_an_expired_prelaunch_operation() {
    let temp = TempDir::new().expect("temp directory");
    let db_path = temp.path().join("operational.db");
    let store = OperationalStore::open(db_path.clone()).expect("open operational store");
    let repository = store.runtime();
    let work_item_id = WorkItemId::new();
    repository
        .enqueue_operation_work(OperationWorkSpec::new(
            OperationRunId::new(),
            work_item_id.clone(),
            "tool_rescan",
            "service",
            "startup-reconcile",
            "{}",
            1_000,
        ))
        .expect("enqueue operation");
    let stale = repository
        .claim_next(ClaimRequest::new("worker-old", "boot-old", 1_000, 20))
        .expect("claim old work")
        .expect("due work");
    repository
        .start_claim(&stale, 1_001)
        .expect("persist prelaunch start");

    let reconciler = StartupReconciler::new(
        repository,
        Arc::new(FixedClock(1_021)),
        ReconcileConfig::new(100, 1).expect("reconcile config"),
    );
    assert_eq!(
        reconciler.reconcile_once().expect("startup reconcile"),
        ReconcileOutcome::Requeued {
            work_item_id: work_item_id.to_string(),
            prior_attempt_no: 1,
            reason: "process_missing".to_owned(),
        }
    );
    assert_eq!(
        reconciler.reconcile_once().expect("no second candidate"),
        ReconcileOutcome::Idle
    );
}

#[test]
fn startup_reconciler_requeues_only_after_inspector_confirms_recorded_process_is_gone() {
    let temp = TempDir::new().expect("temp directory");
    let db_path = temp.path().join("operational.db");
    let store = OperationalStore::open(db_path).expect("open operational store");
    let repository = store.runtime();
    let work_item_id = WorkItemId::new();
    repository
        .enqueue_operation_work(OperationWorkSpec::new(
            OperationRunId::new(),
            work_item_id.clone(),
            "tool_rescan",
            "service",
            "startup-reconcile-process",
            "{}",
            2_000,
        ))
        .expect("enqueue operation");
    let stale = repository
        .claim_next(ClaimRequest::new("worker-old", "boot-old", 2_000, 20))
        .expect("claim old work")
        .expect("due work");
    repository.start_claim(&stale, 2_001).expect("start claim");
    let process_instance_id = Uuid::new_v4().to_string();
    repository
        .record_process_start(ProcessStartSpec::new(
            process_instance_id.clone(),
            stale.clone(),
            "native_tool",
            1_234,
            1_999,
            "boot-old",
            format!(r"Local\RestorePlane.Job.{}", Uuid::new_v4()),
            std::env::current_exe()
                .expect("current executable")
                .to_string_lossy()
                .into_owned(),
            None,
            2_002,
        ))
        .expect("record process");
    repository
        .mark_process_running(&stale, &process_instance_id, 2_003)
        .expect("mark process running");

    let reconciler = StartupReconciler::new(
        repository,
        Arc::new(FixedClock(2_021)),
        ReconcileConfig::new(100, 1).expect("reconcile config"),
    )
    .with_process_inspector(Arc::new(FixedInspector(ProcessObservation::Missing)));
    assert_eq!(
        reconciler.reconcile_once().expect("startup reconcile"),
        ReconcileOutcome::Requeued {
            work_item_id: work_item_id.to_string(),
            prior_attempt_no: 1,
            reason: "process_missing".to_owned(),
        }
    );
}

#[test]
fn startup_batch_does_not_let_an_attention_claim_starve_later_recovery() {
    let temp = TempDir::new().expect("temp directory");
    let store =
        OperationalStore::open(temp.path().join("operational.db")).expect("operational store");
    let repository = store.runtime();

    let attention_work = WorkItemId::new();
    repository
        .enqueue_operation_work(OperationWorkSpec::new(
            OperationRunId::new(),
            attention_work.clone(),
            "tool_rescan",
            "service",
            "attention-first",
            "{}",
            3_000,
        ))
        .expect("enqueue attention operation");
    let attention_claim = repository
        .claim_next(ClaimRequest::new("worker-old", "boot-old", 3_000, 20))
        .expect("claim attention work")
        .expect("attention work");
    repository
        .start_claim(&attention_claim, 3_001)
        .expect("start attention claim");
    let process_instance_id = Uuid::new_v4().to_string();
    repository
        .record_process_start(ProcessStartSpec::new(
            process_instance_id.clone(),
            attention_claim.clone(),
            "native_tool",
            1_234,
            2_999,
            "boot-old",
            format!(r"Local\RestorePlane.Job.{}", Uuid::new_v4()),
            std::env::current_exe()
                .expect("current executable")
                .to_string_lossy()
                .into_owned(),
            None,
            3_002,
        ))
        .expect("record process");
    repository
        .mark_process_running(&attention_claim, &process_instance_id, 3_003)
        .expect("mark process running");

    let recoverable_work = WorkItemId::new();
    repository
        .enqueue_operation_work(OperationWorkSpec::new(
            OperationRunId::new(),
            recoverable_work.clone(),
            "tool_rescan",
            "service",
            "recoverable-second",
            "{}",
            3_001,
        ))
        .expect("enqueue recoverable operation");
    let recoverable_claim = repository
        .claim_next(ClaimRequest::new("worker-old-2", "boot-old", 3_001, 20))
        .expect("claim recoverable work")
        .expect("recoverable work");
    repository
        .start_claim(&recoverable_claim, 3_002)
        .expect("start recoverable claim");

    let reconciler = StartupReconciler::new(
        repository,
        Arc::new(FixedClock(3_022)),
        ReconcileConfig::new(100, 10).expect("reconcile config"),
    );
    let outcomes = reconciler.reconcile_batch().expect("bounded startup batch");
    assert!(outcomes.iter().any(|outcome| matches!(
        outcome,
        ReconcileOutcome::AttentionRequired { work_item_id, .. }
            if work_item_id == attention_work.as_str()
    )));
    assert!(outcomes.iter().any(|outcome| matches!(
        outcome,
        ReconcileOutcome::Requeued { work_item_id, .. }
            if work_item_id == recoverable_work.as_str()
    )));
}

#[test]
fn startup_reconciler_requeues_expired_backup_that_never_crossed_the_prelaunch_marker() {
    let temp = TempDir::new().expect("temp directory");
    let store =
        OperationalStore::open(temp.path().join("operational.db")).expect("operational store");
    let configuration = store.configuration();
    let connection_id = ConnectionProfileId::new();
    let target_id = DatabaseTargetId::new();
    let destination_id = DestinationId::new();
    let revision_id = PlanRevisionId::new();
    configuration
        .insert_connection(ConnectionProfileSpec::tcp(
            connection_id.clone(),
            "primary",
            "postgresql",
            "db.internal",
            5432,
            100,
        ))
        .expect("connection");
    configuration
        .insert_target(DatabaseTargetSpec::new(
            target_id.clone(),
            connection_id,
            "database:app",
            "app",
            "app",
            "source-fingerprint",
            101,
        ))
        .expect("target");
    configuration
        .insert_destination(DestinationSpec::local(
            destination_id.clone(),
            "local",
            r#"{"path":"D:\\backup"}"#,
            102,
        ))
        .expect("destination");
    configuration
        .insert_plan_draft(PlanDraftSpec::manual(
            BackupPlanId::new(),
            revision_id.clone(),
            "nightly",
            1,
            "a".repeat(64),
            103,
        ))
        .expect("plan");
    configuration
        .add_revision_target(&revision_id, &target_id, 104)
        .expect("target membership");
    configuration
        .add_revision_destination(
            &revision_id,
            &destination_id,
            DestinationRequirement::Required,
            105,
        )
        .expect("destination membership");
    configuration
        .publish_revision(&revision_id, 106)
        .expect("publish");
    let runtime = store.runtime();
    runtime
        .materialize_occurrence(OccurrenceMaterializationSpec::due(
            revision_id,
            OccurrenceOrigin::Scheduled,
            1_000,
            1_000,
        ))
        .expect("materialize");
    let claim = runtime
        .claim_next(ClaimRequest::new("old-worker", "old-boot", 1_000, 20))
        .expect("claim")
        .expect("backup work");
    runtime.start_claim(&claim, 1_001).expect("start backup");

    let reconciler = StartupReconciler::new(
        runtime,
        Arc::new(FixedClock(1_021)),
        ReconcileConfig::new(100, 1).expect("config"),
    );
    assert_eq!(
        reconciler.reconcile_once().expect("reconcile backup"),
        ReconcileOutcome::Requeued {
            work_item_id: claim.work_item_id().to_string(),
            prior_attempt_no: 1,
            reason: "process_missing".to_owned(),
        }
    );
}

#[test]
fn startup_reconciler_safely_requeues_expired_running_in_process_sqlite_backup() {
    let temp = TempDir::new().expect("temp directory");
    let store =
        OperationalStore::open(temp.path().join("operational.db")).expect("operational store");
    let configuration = store.configuration();
    let connection_id = ConnectionProfileId::new();
    let target_id = DatabaseTargetId::new();
    let destination_id = DestinationId::new();
    let revision_id = PlanRevisionId::new();
    configuration
        .insert_connection(ConnectionProfileSpec::file(
            connection_id.clone(),
            "sqlite",
            "sqlite",
            r"D:\data\app.sqlite",
            100,
        ))
        .expect("connection");
    configuration
        .insert_target(DatabaseTargetSpec::new(
            target_id.clone(),
            connection_id,
            "file:app.sqlite",
            "app.sqlite",
            "app.sqlite",
            "source-fingerprint",
            101,
        ))
        .expect("target");
    configuration
        .insert_destination(DestinationSpec::local(
            destination_id.clone(),
            "local",
            r#"{"path":"D:\\backup"}"#,
            102,
        ))
        .expect("destination");
    configuration
        .insert_plan_draft(PlanDraftSpec::manual(
            BackupPlanId::new(),
            revision_id.clone(),
            "sqlite",
            1,
            "a".repeat(64),
            103,
        ))
        .expect("plan");
    configuration
        .add_revision_target(&revision_id, &target_id, 104)
        .expect("target membership");
    configuration
        .add_revision_destination(
            &revision_id,
            &destination_id,
            DestinationRequirement::Required,
            105,
        )
        .expect("destination membership");
    configuration
        .publish_revision(&revision_id, 106)
        .expect("publish");
    let runtime = store.runtime();
    runtime
        .materialize_occurrence(OccurrenceMaterializationSpec::due(
            revision_id,
            OccurrenceOrigin::Scheduled,
            2_000,
            2_000,
        ))
        .expect("materialize");
    let claim = runtime
        .claim_next(ClaimRequest::new("old-worker", "old-boot", 2_000, 20))
        .expect("claim")
        .expect("backup work");
    runtime.start_claim(&claim, 2_001).expect("start backup");
    runtime
        .mark_in_process_claim_running(&claim, 2_002)
        .expect("mark SQLite in-process work running");

    let reconciler = StartupReconciler::new(
        runtime,
        Arc::new(FixedClock(2_021)),
        ReconcileConfig::new(100, 1).expect("config"),
    );
    assert_eq!(
        reconciler.reconcile_once().expect("reconcile backup"),
        ReconcileOutcome::Requeued {
            work_item_id: claim.work_item_id().to_string(),
            prior_attempt_no: 1,
            reason: "process_missing".to_owned(),
        }
    );
}

struct FixedClock(i64);

impl RuntimeClock for FixedClock {
    fn now_ms(&self) -> i64 {
        self.0
    }
}

struct FixedInspector(ProcessObservation);

impl ProcessInspector for FixedInspector {
    fn inspect(&self, _probe: &ProcessProbe) -> Result<ProcessObservation, ReconcileError> {
        Ok(self.0)
    }
}
