use std::collections::BTreeMap;
use std::sync::Arc;

use ed25519_dalek::SigningKey;
use restoreplane_catalog::{
    rusqlite::Connection, BackupWorkSpec, ClaimRequest, CompleteRecoveryPointSpec,
    ConnectionProfileSpec, DatabaseTargetSpec, DestinationSpec, OccurrenceSpec, OperationalStore,
    PlanDraftSpec, ProcessExitSpec, ProcessStartSpec,
};
use restoreplane_core::{ArtifactPartRole, ConsistencyGrade, Engine, PartSpec};
use restoreplane_domain::{
    BackupPlanId, BackupRunId, ConnectionProfileId, DatabaseTargetId, DestinationId,
    DestinationRequirement, OccurrenceIdentity, OccurrenceOrigin, PlanOccurrenceId, PlanRevisionId,
    RecoveryPointId, WorkItemId,
};
use restoreplane_pipeline::{
    EncodingEvidenceV2, ManifestPartV2, NativeToolIdentityV2, ReaderStatusV2, RecoveryOriginV2,
    ReplicaCommitmentV2, ReplicaPublishOutcomeV2, ReplicaRequirementV2, SetManifestV2,
    SignedSetManifestV2, UploadReceipt,
};
use restoreplane_runtime::{
    ProcessInspector, ProcessObservation, ProcessProbe, ReconcileConfig, ReconcileError,
    ReconcileOutcome, RuntimeClock, StartupReconciler, VerifiedRecoveryPointSource,
};
use tempfile::TempDir;
use uuid::Uuid;

#[test]
fn manifest_published_before_database_commit_is_reconciled_exactly_once() {
    let fixture = Fixture::new();
    let source = Arc::new(FixedRecoveryPointSource(fixture.recovery_spec()));
    let reconciler = fixture.reconciler(source);

    assert_eq!(
        reconciler
            .reconcile_once()
            .expect("reconcile published manifest"),
        ReconcileOutcome::RecoveredCommit {
            work_item_id: fixture.work_item_id.to_string(),
            prior_attempt_no: 1,
            recovery_point_id: fixture.recovery_point_id.to_string(),
        }
    );
    assert_eq!(
        reconciler
            .reconcile_once()
            .expect("no duplicate reconciliation"),
        ReconcileOutcome::Idle
    );
    fixture.assert_committed();
}

#[test]
fn failures_after_each_database_checkpoint_roll_back_and_restart_reconciles_once() {
    for checkpoint in [
        CommitFaultCheckpoint::ProjectionWritten,
        CommitFaultCheckpoint::SubjectLinked,
        CommitFaultCheckpoint::WorkTerminalStaged,
    ] {
        let fixture = Fixture::new();
        fixture.install_fault(checkpoint);
        let source = Arc::new(FixedRecoveryPointSource(fixture.recovery_spec()));
        let failed = fixture
            .reconciler(source.clone())
            .reconcile_once()
            .expect_err("injected database fault must abort reconciliation");
        assert_eq!(failed.code(), "OPERATIONAL_REPOSITORY_SQLITE");
        fixture.assert_rolled_back();

        fixture.remove_fault(checkpoint);
        assert_eq!(
            fixture
                .reconciler(source)
                .reconcile_once()
                .expect("restart reconciliation"),
            ReconcileOutcome::RecoveredCommit {
                work_item_id: fixture.work_item_id.to_string(),
                prior_attempt_no: 1,
                recovery_point_id: fixture.recovery_point_id.to_string(),
            },
            "checkpoint={checkpoint:?}"
        );
        fixture.assert_committed();
    }
}

#[derive(Clone, Copy, Debug)]
enum CommitFaultCheckpoint {
    ProjectionWritten,
    SubjectLinked,
    WorkTerminalStaged,
}

impl CommitFaultCheckpoint {
    fn trigger_name(self) -> &'static str {
        match self {
            Self::ProjectionWritten => "fault_after_projection",
            Self::SubjectLinked => "fault_after_subject_link",
            Self::WorkTerminalStaged => "fault_after_work_terminal",
        }
    }

    fn create_sql(self) -> &'static str {
        match self {
            Self::ProjectionWritten => {
                "CREATE TRIGGER fault_after_projection
                 BEFORE UPDATE OF recovery_point_id ON backup_runs
                 WHEN NEW.recovery_point_id IS NOT NULL
                 BEGIN
                     SELECT RAISE(ABORT, 'fault_after_projection');
                 END;"
            }
            Self::SubjectLinked => {
                "CREATE TRIGGER fault_after_subject_link
                 BEFORE UPDATE OF state ON backup_runs
                 WHEN NEW.state = 'succeeded'
                 BEGIN
                     SELECT RAISE(ABORT, 'fault_after_subject_link');
                 END;"
            }
            Self::WorkTerminalStaged => {
                "CREATE TRIGGER fault_after_work_terminal
                 BEFORE UPDATE OF state ON work_attempts
                 WHEN NEW.state = 'succeeded'
                 BEGIN
                     SELECT RAISE(ABORT, 'fault_after_work_terminal');
                 END;"
            }
        }
    }
}

struct FixedRecoveryPointSource(CompleteRecoveryPointSpec);

impl VerifiedRecoveryPointSource for FixedRecoveryPointSource {
    fn verified_complete(
        &self,
        _claim: &restoreplane_catalog::ClaimedWork,
    ) -> Result<Option<CompleteRecoveryPointSpec>, ReconcileError> {
        Ok(Some(self.0.clone()))
    }
}

struct FixedClock(i64);

impl RuntimeClock for FixedClock {
    fn now_ms(&self) -> i64 {
        self.0
    }
}

struct ExitedProcessInspector;

impl ProcessInspector for ExitedProcessInspector {
    fn inspect(&self, _probe: &ProcessProbe) -> Result<ProcessObservation, ReconcileError> {
        Ok(ProcessObservation::Exited)
    }
}

struct Fixture {
    _temp: TempDir,
    db_path: std::path::PathBuf,
    store: OperationalStore,
    occurrence_id: PlanOccurrenceId,
    backup_run_id: BackupRunId,
    work_item_id: WorkItemId,
    target_id: DatabaseTargetId,
    recovery_point_id: RecoveryPointId,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().expect("temp directory");
        let db_path = temp.path().join("operational.db");
        let store = OperationalStore::open(db_path.clone()).expect("open store");
        let connection_id = ConnectionProfileId::new();
        let target_id = DatabaseTargetId::new();
        let destination_id = DestinationId::new();
        let plan_id = BackupPlanId::new();
        let revision_id = PlanRevisionId::new();
        let configuration = store.configuration();
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
                r#"{"path":"D:\\backup"}"#,
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
            .expect("publish plan");

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
                work_item_id.clone(),
                occurrence_id.clone(),
                revision_id,
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
        let process_id = Uuid::new_v4().to_string();
        runtime
            .record_process_start(ProcessStartSpec::new(
                process_id.clone(),
                claim.clone(),
                "native_dump",
                4_242,
                999,
                "boot-old",
                format!(r"Local\RestorePlane.Job.{}", Uuid::new_v4()),
                std::env::current_exe()
                    .expect("test executable")
                    .to_string_lossy()
                    .into_owned(),
                None,
                1_002,
            ))
            .expect("record process");
        runtime
            .mark_process_running(&claim, &process_id, 1_003)
            .expect("mark process running");
        runtime
            .record_process_exit(&claim, ProcessExitSpec::new(process_id, 1_010, 0, "exited"))
            .expect("record process exit");
        runtime
            .begin_claim_commit(&claim, 1_011)
            .expect("begin commit");

        Self {
            _temp: temp,
            db_path,
            store,
            occurrence_id,
            backup_run_id,
            work_item_id,
            target_id,
            recovery_point_id: RecoveryPointId::new(),
        }
    }

    fn recovery_spec(&self) -> CompleteRecoveryPointSpec {
        let prefix = format!(
            "v2/{}/{}/{}",
            self.occurrence_id.as_str(),
            self.target_id.as_str(),
            self.recovery_point_id.as_str()
        );
        let part_id = Uuid::new_v4();
        let manifest_key = format!("{prefix}/set-manifest.json");
        let manifest = SetManifestV2::new(
            RecoveryOriginV2::new(
                self.recovery_point_id.as_uuid(),
                "instance-a",
                self.occurrence_id.as_uuid(),
                self.backup_run_id.as_uuid(),
                self.target_id.as_uuid(),
                "source-fingerprint",
                "orders",
            )
            .expect("recovery origin"),
            Engine::Postgresql,
            ConsistencyGrade::ApplicationConsistent,
            1_010,
            "signing-key-01",
            NativeToolIdentityV2::new("pg_dump", "17.4", "a".repeat(64)).expect("native tool"),
            BTreeMap::new(),
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
                    "chacha20poly1305-stream",
                    1,
                )
                .expect("encoding evidence"),
                format!("{prefix}/parts/orders.dump.enc"),
            )
            .expect("Manifest part")],
            vec![ReplicaCommitmentV2::new(
                "local:vault-a",
                "local",
                ReplicaRequirementV2::Required,
                ReaderStatusV2::Eligible,
                "none",
                manifest_key.clone(),
            )
            .expect("replica")],
        )
        .expect("set Manifest");
        let signing_key = SigningKey::from_bytes(&[23_u8; 32]);
        let signed = SignedSetManifestV2::create(manifest, &signing_key).expect("signed Manifest");
        let manifest_bytes =
            u64::try_from(signed.to_json().expect("Manifest JSON").len()).expect("Manifest size");
        CompleteRecoveryPointSpec::from_verified_publish(
            &signed,
            &[ReplicaPublishOutcomeV2 {
                destination_fingerprint: "local:vault-a".to_owned(),
                requirement: ReplicaRequirementV2::Required,
                reader_status: ReaderStatusV2::Eligible,
                payload_receipts: vec![UploadReceipt {
                    key: format!("{prefix}/parts/orders.dump.enc"),
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
            1_101,
        )
        .expect("derive signed projection")
    }

    fn reconciler(&self, source: Arc<dyn VerifiedRecoveryPointSource>) -> StartupReconciler {
        StartupReconciler::new(
            self.store.runtime(),
            Arc::new(FixedClock(1_101)),
            ReconcileConfig::new(100, 1).expect("reconcile config"),
        )
        .with_process_inspector(Arc::new(ExitedProcessInspector))
        .with_recovery_point_source(source)
    }

    fn install_fault(&self, checkpoint: CommitFaultCheckpoint) {
        Connection::open(&self.db_path)
            .expect("open fault connection")
            .execute_batch(checkpoint.create_sql())
            .expect("install fault trigger");
    }

    fn remove_fault(&self, checkpoint: CommitFaultCheckpoint) {
        Connection::open(&self.db_path)
            .expect("open fault connection")
            .execute_batch(&format!("DROP TRIGGER {};", checkpoint.trigger_name()))
            .expect("drop fault trigger");
    }

    fn assert_rolled_back(&self) {
        let connection = Connection::open(&self.db_path).expect("open rollback probe");
        let row: (String, String, String, Option<String>, i64) = connection
            .query_row(
                "SELECT
                     work_items.state,
                     work_attempts.state,
                     backup_runs.state,
                     backup_runs.recovery_point_id,
                     (SELECT COUNT(*) FROM catalog_recovery_points)
                 FROM work_items
                 JOIN work_attempts ON work_attempts.work_item_id = work_items.id
                 JOIN backup_runs ON backup_runs.id = work_items.backup_run_id
                 WHERE work_items.id = ?1",
                [self.work_item_id.as_str()],
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
            .expect("query rollback state");
        assert_eq!(
            row,
            (
                "committing".to_owned(),
                "committing".to_owned(),
                "committing".to_owned(),
                None,
                0,
            )
        );
    }

    fn assert_committed(&self) {
        let connection = Connection::open(&self.db_path).expect("open commit probe");
        let row: (String, String, String, Option<String>, String, i64) = connection
            .query_row(
                "SELECT
                     work_items.state,
                     work_attempts.state,
                     backup_runs.state,
                     backup_runs.recovery_point_id,
                     plan_occurrences.state,
                     (SELECT COUNT(*) FROM catalog_recovery_points)
                 FROM work_items
                 JOIN work_attempts ON work_attempts.work_item_id = work_items.id
                 JOIN backup_runs ON backup_runs.id = work_items.backup_run_id
                 JOIN plan_occurrences ON plan_occurrences.id = backup_runs.plan_occurrence_id
                 WHERE work_items.id = ?1",
                [self.work_item_id.as_str()],
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
            .expect("query committed state");
        assert_eq!(
            row,
            (
                "succeeded".to_owned(),
                "succeeded".to_owned(),
                "succeeded".to_owned(),
                Some(self.recovery_point_id.to_string()),
                "succeeded".to_owned(),
                1,
            )
        );
        assert!(
            self.store
                .catalog_projection()
                .snapshot(&self.recovery_point_id)
                .is_ok(),
            "snapshot projection must exist"
        );
    }
}
