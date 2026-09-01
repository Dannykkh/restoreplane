use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use dbbackup_catalog::{
    rusqlite::{params, Connection},
    CertificationCapability, ClaimRequest, ConnectionProbeSpec, ConnectionProfileSpec,
    DiscoveredDatabaseSpec, DiscoverySnapshotSpec, EngineCertificationSpec, NativeToolSpec,
    OperationalStore, ProcessExitSpec, ProcessStartSpec, RestorePurpose, SafeRestoreSpec,
    SpoolVolumeSpec,
};
use dbbackup_core::ProviderCapability;
use dbbackup_domain::{
    ArtifactReplicaId, ConnectionProfileId, RecoveryPointId, RestoreRunId, WorkItemId,
};
use dbbackup_runtime::{
    ReconcileConfig, ReconcileOutcome, RestoreAdmission, RestoreExecutionError,
    RestoreExecutionEvidence, RestoreExecutor, RestoreValidationError, RestoreValidationEvidence,
    RestoreWorker, RestoreWorkerConfig, RestoreWorkerOutcome, RuntimeClock, StartupReconciler,
    SystemRuntimeClock,
};
use serde_json::{json, Value};
use tempfile::TempDir;

const PG_SERVER_IDENTITY: &str = "postgresql-system-id:7541424833265539027";

#[tokio::test]
async fn restore_succeeds_only_after_native_exit_zero_and_oracle_pass() {
    let fixture = Fixture::new();
    let executor = Arc::new(ScriptedExecutor::passing());
    let worker = fixture.worker(executor.clone());

    let outcome = worker.run_once().await.expect("run restore");

    assert!(matches!(
        outcome,
        RestoreWorkerOutcome::Succeeded {
            ref restore_run_id,
            measured_rto_ms,
        } if restore_run_id == fixture.restore_run_id.as_str() && measured_rto_ms > 0
    ));
    assert_eq!(executor.restore_calls.load(Ordering::SeqCst), 1);
    assert_eq!(executor.validation_calls.load(Ordering::SeqCst), 1);
    let row = fixture.restore_row();
    assert_eq!(row.state, "succeeded");
    assert!(row.measured_rto_ms.is_some_and(|value| value > 0));
    assert_eq!(row.error_code, None);
    let evidence: Value = serde_json::from_str(
        row.validation_result_json
            .as_deref()
            .expect("evidence JSON"),
    )
    .expect("valid evidence");
    assert_eq!(evidence["restore"]["nativeExitCode"], 0);
    assert_eq!(evidence["restore"]["targetCreated"], true);
    assert_eq!(evidence["validation"]["status"], "passed");
    assert_eq!(evidence["cleanup"]["required"], false);
    assert_eq!(fixture.work_state(), "succeeded");
}

#[tokio::test]
async fn restore_admission_deferral_requeues_without_starting_destructive_execution() {
    let fixture = Fixture::new();
    let worker = fixture.worker(Arc::new(DeferredExecutor));

    let outcome = worker.run_once().await.expect("defer restore admission");

    assert!(matches!(
        outcome,
        RestoreWorkerOutcome::Deferred {
            ref restore_run_id,
            ref wait_reason,
            available_at_ms,
        } if restore_run_id == fixture.restore_run_id.as_str()
            && wait_reason == "spool_capacity:fixture"
            && available_at_ms > fixture.worker_start_ms
    ));
    assert_eq!(fixture.restore_row().state, "queued");
    assert_eq!(fixture.work_state(), "delayed");
}

#[tokio::test]
async fn cancellation_observed_during_admission_settles_cancelled_without_retry() {
    let fixture = Fixture::new();
    let worker = fixture.worker(Arc::new(AdmissionCancellingExecutor {
        repository: fixture.store.runtime(),
        work_item_id: fixture.work_item_id.clone(),
    }));

    let outcome = worker.run_once().await.expect("cancel during admission");

    assert_eq!(
        outcome,
        RestoreWorkerOutcome::Cancelled {
            restore_run_id: fixture.restore_run_id.to_string(),
            cleanup_required: false,
        }
    );
    assert_eq!(fixture.restore_row().state, "cancelled");
    assert_eq!(fixture.work_state(), "cancelled");
}

#[tokio::test]
async fn failed_restore_drill_records_failure_without_upgrading_snapshot_verification() {
    let fixture = Fixture::drill();
    let worker = fixture.worker(Arc::new(ScriptedExecutor::oracle_failure()));

    let outcome = worker.run_once().await.expect("run failing restore drill");

    assert!(matches!(outcome, RestoreWorkerOutcome::Failed { .. }));
    assert_eq!(fixture.drill_projection_row().1, "failed");
    assert_eq!(fixture.snapshot_verification_row(), (None, None));
}

#[tokio::test]
async fn restore_drill_success_atomically_settles_the_bound_l3_verification_evidence() {
    let fixture = Fixture::drill();
    let worker = fixture.worker(Arc::new(ScriptedExecutor::passing()));

    let outcome = worker.run_once().await.expect("run restore drill");

    assert!(matches!(outcome, RestoreWorkerOutcome::Succeeded { .. }));
    let verification = fixture.drill_verification_row();
    assert_eq!(verification.state, "passed");
    assert_eq!(verification.achieved_level, Some(3));
    assert_eq!(verification.restore_run_id, fixture.restore_run_id.as_str());
    assert_eq!(verification.error_code, None);
    let evidence: Value =
        serde_json::from_str(&verification.evidence_json.expect("drill evidence"))
            .expect("valid drill evidence");
    assert_eq!(evidence["restore"]["nativeExitCode"], 0);
    assert_eq!(evidence["validation"]["status"], "passed");
    assert_eq!(evidence["cleanup"]["required"], false);
    let projection = fixture.drill_projection_row();
    assert_eq!(projection.0, 3);
    assert_eq!(projection.1, "passed");
    assert_eq!(fixture.snapshot_verification_row(), (Some(3), projection.2));
    assert_eq!(
        fixture
            .store
            .runtime()
            .claim_next(
                ClaimRequest::new("unexpected-verifier", "boot-v", 9_000, 100)
                    .for_kind(dbbackup_domain::WorkItemKind::Verification),
            )
            .expect("query verification queue"),
        None,
        "the restore Work Item itself owns L3 validation"
    );
}

#[tokio::test]
async fn native_exit_zero_with_oracle_failure_is_cleanup_required_never_success() {
    let fixture = Fixture::new();
    let executor = Arc::new(ScriptedExecutor::oracle_failure());
    let worker = fixture.worker(executor);

    let outcome = worker.run_once().await.expect("run restore");

    assert_eq!(
        outcome,
        RestoreWorkerOutcome::Failed {
            restore_run_id: fixture.restore_run_id.to_string(),
            error_code: "RESTORE_ORACLE_MISMATCH".to_owned(),
            cleanup_required: true,
        }
    );
    let row = fixture.restore_row();
    assert_eq!(row.state, "cleanup_required");
    assert_eq!(row.error_code.as_deref(), Some("RESTORE_ORACLE_MISMATCH"));
    let evidence: Value = serde_json::from_str(
        row.validation_result_json
            .as_deref()
            .expect("evidence JSON"),
    )
    .expect("valid evidence");
    assert_eq!(evidence["restore"]["nativeExitCode"], 0);
    assert_eq!(evidence["validation"]["status"], "failed");
    assert_eq!(evidence["cleanup"]["required"], true);
    assert_eq!(evidence["cleanup"]["targetPreserved"], true);
    assert_eq!(fixture.work_state(), "failed_terminal");
    assert_eq!(fixture.success_count(), 0);
}

#[tokio::test]
async fn cancel_after_target_creation_preserves_target_and_records_cleanup_evidence() {
    let fixture = Fixture::new();
    let executor = Arc::new(CancellingExecutor {
        repository: fixture.store.runtime(),
        work_item_id: fixture.work_item_id.clone(),
        release_calls: AtomicI64::new(0),
    });
    let worker = fixture.worker(executor.clone());

    let outcome = worker.run_once().await.expect("run cancelled restore");

    assert_eq!(
        outcome,
        RestoreWorkerOutcome::Cancelled {
            restore_run_id: fixture.restore_run_id.to_string(),
            cleanup_required: true,
        }
    );
    let row = fixture.restore_row();
    assert_eq!(row.state, "cleanup_required");
    assert_eq!(row.error_code.as_deref(), Some("CANCEL_REQUESTED"));
    let evidence: Value = serde_json::from_str(
        row.validation_result_json
            .as_deref()
            .expect("evidence JSON"),
    )
    .expect("valid evidence");
    assert_eq!(evidence["phase"], "cancelled");
    assert_eq!(evidence["cleanup"]["required"], true);
    assert_eq!(evidence["cleanup"]["targetPreserved"], true);
    assert_eq!(fixture.work_state(), "cancelled");
    assert_eq!(executor.release_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancellation_observed_inside_native_restore_settles_cancelled_without_requeue() {
    let fixture = Fixture::new();
    let executor = Arc::new(NativeCancellationExecutor {
        repository: fixture.store.runtime(),
        work_item_id: fixture.work_item_id.clone(),
    });
    let worker = fixture.worker(executor);

    let outcome = worker.run_once().await.expect("run native cancellation");

    assert_eq!(
        outcome,
        RestoreWorkerOutcome::Cancelled {
            restore_run_id: fixture.restore_run_id.to_string(),
            cleanup_required: true,
        }
    );
    let row = fixture.restore_row();
    assert_eq!(row.state, "cleanup_required");
    assert_eq!(row.error_code.as_deref(), Some("CANCEL_REQUESTED"));
    assert_eq!(fixture.work_state(), "cancelled");
    assert_eq!(
        worker
            .run_once()
            .await
            .expect("cancelled work stays terminal"),
        RestoreWorkerOutcome::Idle
    );
}

#[test]
fn expired_restore_before_destructive_marker_is_safely_requeued() {
    let fixture = Fixture::new();
    let repository = fixture.store.runtime();
    let claim = repository
        .claim_next(
            ClaimRequest::new("restore-worker-old", "boot-old", 1_100, 20)
                .for_kind(dbbackup_domain::WorkItemKind::Restore),
        )
        .expect("claim restore")
        .expect("due restore");
    repository
        .start_claim(&claim, 1_101)
        .expect("start restore");
    repository
        .mark_in_process_claim_running(&claim, 1_102)
        .expect("mark restore running");
    let reconciler = StartupReconciler::new(
        repository,
        Arc::new(IncrementingClock::new(1_121)),
        ReconcileConfig::new(100, 1).expect("reconcile config"),
    );

    assert!(matches!(
        reconciler.reconcile_once().expect("reconcile safe restore crash"),
        ReconcileOutcome::Requeued {
            ref work_item_id,
            prior_attempt_no: 1,
            ..
        } if work_item_id == fixture.work_item_id.as_str()
    ));
    assert_eq!(fixture.restore_row().state, "running");
    assert_eq!(fixture.work_state(), "delayed");
}

#[test]
fn destructive_marker_refuses_expired_authoritative_preflight() {
    let fixture = Fixture::build(
        1_000,
        1_099,
        "17.4",
        r"C:\dbbackup\tools\pg_restore.exe",
        "f".repeat(64),
    );
    let repository = fixture.store.runtime();
    let claim = repository
        .claim_next(
            ClaimRequest::new("restore-worker-old", "boot-old", 1_100, 100)
                .for_kind(dbbackup_domain::WorkItemKind::Restore),
        )
        .expect("claim restore")
        .expect("due restore");
    repository
        .start_claim(&claim, 1_101)
        .expect("start restore");
    repository
        .mark_in_process_claim_running(&claim, 1_102)
        .expect("mark restore running");

    let error = repository
        .begin_restore_destructive_step(
            &claim,
            1_103,
            1_203,
            Some("17.4"),
            Some(PG_SERVER_IDENTITY),
            false,
        )
        .expect_err("expired preflight must refuse destructive restore fence");
    assert_eq!(error.code(), "RESTORE_PREFLIGHT_STALE");
    let row = fixture.restore_row();
    assert_eq!(row.state, "running");
    assert_eq!(row.validation_result_json, None);
    assert_eq!(fixture.process_count(), 0);
}

#[test]
fn destructive_marker_preserves_valid_claim_authorization_after_long_materialization() {
    let fixture = Fixture::build(
        1_000,
        1_103,
        "17.4",
        r"C:\dbbackup\tools\pg_restore.exe",
        "f".repeat(64),
    );
    let repository = fixture.store.runtime();
    let claim = repository
        .claim_next(
            ClaimRequest::new("restore-worker-long", "boot-long", 1_100, 1_000_000)
                .for_kind(dbbackup_domain::WorkItemKind::Restore),
        )
        .expect("claim restore")
        .expect("due restore");
    repository
        .start_claim(&claim, 1_101)
        .expect("start restore");
    repository
        .mark_in_process_claim_running(&claim, 1_102)
        .expect("mark restore running");

    assert_eq!(
        repository
            .begin_restore_destructive_step(
                &claim,
                400_000,
                430_000,
                Some("17.4"),
                Some(PG_SERVER_IDENTITY),
                false,
            )
            .expect("preserve claim-time authorization"),
        dbbackup_catalog::DestructiveStepDisposition::Proceed
    );
    let row = fixture.restore_row();
    assert_eq!(row.state, "running");
    assert!(row.validation_result_json.is_some());
}

#[test]
fn destructive_marker_refuses_same_version_different_live_server_identity() {
    let fixture = Fixture::new();
    let repository = fixture.store.runtime();
    let claim = repository
        .claim_next(
            ClaimRequest::new("restore-worker-drift", "boot-drift", 1_100, 100)
                .for_kind(dbbackup_domain::WorkItemKind::Restore),
        )
        .expect("claim restore")
        .expect("due restore");
    repository
        .start_claim(&claim, 1_101)
        .expect("start restore");
    repository
        .mark_in_process_claim_running(&claim, 1_102)
        .expect("mark restore running");

    let error = repository
        .begin_restore_destructive_step(
            &claim,
            1_103,
            1_203,
            Some("17.4"),
            Some("postgresql-system-id:7541424833265539028"),
            false,
        )
        .expect_err("same-version failover must not arm the destructive marker");
    assert_eq!(error.code(), "RESTORE_PREFLIGHT_STALE");
    let row = fixture.restore_row();
    assert_eq!(row.state, "running");
    assert_eq!(row.validation_result_json, None);
    assert_eq!(fixture.process_count(), 0);
}

#[test]
fn expired_restore_after_destructive_marker_requires_operator_cleanup() {
    let fixture = Fixture::new();
    let repository = fixture.store.runtime();
    let claim = repository
        .claim_next(
            ClaimRequest::new("restore-worker-old", "boot-old", 1_100, 20)
                .for_kind(dbbackup_domain::WorkItemKind::Restore),
        )
        .expect("claim restore")
        .expect("due restore");
    repository
        .start_claim(&claim, 1_101)
        .expect("start restore");
    repository
        .mark_in_process_claim_running(&claim, 1_102)
        .expect("mark restore running");
    assert_eq!(
        repository
            .begin_restore_destructive_step(
                &claim,
                1_103,
                1_123,
                Some("17.4"),
                Some(PG_SERVER_IDENTITY),
                false,
            )
            .expect("arm destructive restore fence"),
        dbbackup_catalog::DestructiveStepDisposition::Proceed
    );
    let reconciler = StartupReconciler::new(
        repository,
        Arc::new(IncrementingClock::new(1_124)),
        ReconcileConfig::new(100, 1).expect("reconcile config"),
    );

    assert_eq!(
        reconciler
            .reconcile_once()
            .expect("reconcile destructive restore crash"),
        ReconcileOutcome::CleanupRequired {
            work_item_id: fixture.work_item_id.to_string(),
            prior_attempt_no: 1,
            reason: "restore_crash_target_preserved".to_owned(),
        }
    );
    let row = fixture.restore_row();
    assert_eq!(row.state, "cleanup_required");
    assert_eq!(
        row.error_code.as_deref(),
        Some("RESTORE_CRASH_CLEANUP_REQUIRED")
    );
    let evidence: Value = serde_json::from_str(
        row.validation_result_json
            .as_deref()
            .expect("evidence JSON"),
    )
    .expect("valid evidence");
    assert_eq!(evidence["phase"], "reconcile");
    assert_eq!(evidence["cleanup"]["required"], true);
    assert_eq!(evidence["cleanup"]["targetPreserved"], true);
    assert_eq!(fixture.work_state(), "failed_terminal");
    assert_eq!(
        reconciler.reconcile_once().expect("no repeated restore"),
        ReconcileOutcome::Idle
    );
}

#[cfg(windows)]
#[tokio::test]
#[ignore = "requires Docker Desktop and the pinned PostgreSQL G2 image"]
async fn live_postgresql_create_new_restore_runs_through_durable_restore_work_and_oracle() {
    use std::path::PathBuf;

    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root")
        .to_path_buf();
    let docker_path = PathBuf::from(r"C:\Program Files\Docker\Docker\resources\bin\docker.exe");
    let docker_sha256 =
        dbbackup_runtime::hash_executable(&docker_path).expect("hash the installed Docker CLI");
    let base_ms = SystemRuntimeClock.now_ms();
    assert!(base_ms > 0, "system clock must be available");
    let fixture = Fixture::build(
        base_ms,
        base_ms + 600_000,
        "18.4",
        "docker://postgres@sha256:3a82e1f56c8f0f5616a11103ac3d47e632c3938698946a7ad26da0df1334744a/usr/lib/postgresql/18/bin/pg_restore",
        "f392946d7d6e9504ad933d59a105ffa38bc57e05e00c960a4de5b27ed91a7237"
            .to_owned(),
    );
    let executor = Arc::new(DockerPostgresRestoreExecutor::new(
        fixture.store.runtime(),
        docker_path,
        docker_sha256,
        workspace,
    ));
    let worker = fixture.live_worker(executor.clone());

    let outcome = worker.run_once().await.expect("run live restore");
    let diagnostic_row = fixture.restore_row();

    assert!(
        matches!(
            outcome,
            RestoreWorkerOutcome::Succeeded {
                ref restore_run_id,
                measured_rto_ms,
            } if restore_run_id == fixture.restore_run_id.as_str() && measured_rto_ms > 0
        ),
        "unexpected outcome: {outcome:?}; restore row: {diagnostic_row:?}"
    );
    let row = fixture.restore_row();
    assert_eq!(row.state, "succeeded");
    assert_eq!(fixture.work_state(), "succeeded");
    assert!(fixture.process_count() >= 6);
    let evidence: Value = serde_json::from_str(
        row.validation_result_json
            .as_deref()
            .expect("live restore evidence"),
    )
    .expect("valid live evidence");
    assert_eq!(evidence["restore"]["nativeExitCode"], 0);
    assert_eq!(evidence["restore"]["targetCreated"], true);
    assert_eq!(evidence["validation"]["status"], "passed");
    assert_eq!(
        evidence["validation"]["evidence"]["oracleSha256"],
        POSTGRES_G2_ORACLE_SHA256
    );

    if let Some(path) = std::env::var_os("DBBACKUP_G3_EVIDENCE_PATH") {
        let document = json!({
            "schemaVersion": 1,
            "gateId": "G3-B",
            "scenario": "create-new-actual-restore-oracle",
            "engine": "postgresql",
            "imageDigest": POSTGRES_IMAGE_DIGEST,
            "artifactSha256": POSTGRES_G2_DUMP_SHA256,
            "restoreRunId": fixture.restore_run_id.as_str(),
            "workItemId": fixture.work_item_id.as_str(),
            "restoreState": row.state,
            "workState": fixture.work_state(),
            "processCount": fixture.process_count(),
            "oracleSha256": evidence["validation"]["evidence"]["oracleSha256"],
            "oracleMatched": true,
            "cleanupRequired": evidence["cleanup"]["required"],
        });
        let bytes = serde_json::to_vec_pretty(&document).expect("serialize G3-B evidence");
        let path = PathBuf::from(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create G3-B evidence directory");
        }
        std::fs::write(path, bytes).expect("write G3-B evidence");
    }
}

#[cfg(windows)]
const POSTGRES_IMAGE_DIGEST: &str =
    "sha256:3a82e1f56c8f0f5616a11103ac3d47e632c3938698946a7ad26da0df1334744a";
#[cfg(windows)]
const POSTGRES_IMAGE: &str =
    "postgres@sha256:3a82e1f56c8f0f5616a11103ac3d47e632c3938698946a7ad26da0df1334744a";
#[cfg(windows)]
const POSTGRES_G2_DUMP_SHA256: &str =
    "d69a019768d1c4e940b5379c128da4f82e490e08a1107aafcababd947968c29a";
#[cfg(windows)]
const POSTGRES_G2_ORACLE_SHA256: &str =
    "f232e0f6ca817640fd415a66a0ceca71c721ac99ea5bbaff705efd7e1713fb75";

#[cfg(windows)]
struct DockerPostgresRestoreExecutor {
    repository: dbbackup_catalog::RuntimeRepository,
    docker_path: std::path::PathBuf,
    docker_sha256: [u8; 32],
    workspace: std::path::PathBuf,
    container_name: String,
}

#[cfg(windows)]
impl DockerPostgresRestoreExecutor {
    fn new(
        repository: dbbackup_catalog::RuntimeRepository,
        docker_path: std::path::PathBuf,
        docker_sha256: [u8; 32],
        workspace: std::path::PathBuf,
    ) -> Self {
        Self {
            repository,
            docker_path,
            docker_sha256,
            workspace,
            container_name: format!("dbbackup-g3-restore-{}", uuid::Uuid::new_v4().simple()),
        }
    }

    fn run_tracked(
        &self,
        claim: &dbbackup_catalog::ClaimedWork,
        phase: &str,
        arguments: Vec<String>,
        allow_nonzero: bool,
    ) -> Result<dbbackup_runtime::ProcessExit, &'static str> {
        use std::time::{Duration, Instant};

        let spec = dbbackup_runtime::LaunchSpec::new(
            self.docker_path.clone(),
            self.workspace.clone(),
            self.docker_sha256,
        )
        .and_then(|spec| spec.with_args(arguments))
        .and_then(|spec| spec.with_output_tail_bytes(256 * 1024))
        .map_err(|_| "RESTORE_PROCESS_SPEC_INVALID")?;
        let mut process = dbbackup_runtime::ProcessSupervisor::launch(&spec)
            .map_err(|_| "RESTORE_PROCESS_LAUNCH_FAILED")?;
        let process_instance_id = uuid::Uuid::new_v4().to_string();
        let started_at_ms = SystemRuntimeClock.now_ms();
        let process_start_time_ms = windows_filetime_to_unix_ms(process.start_time_100ns())
            .ok_or("RESTORE_PROCESS_IDENTITY_INVALID")?;
        self.repository
            .record_process_start(ProcessStartSpec::new(
                process_instance_id.clone(),
                claim.clone(),
                phase,
                i64::from(process.id()),
                process_start_time_ms,
                claim.worker_boot_id(),
                process.job_key(),
                spec.executable().to_string_lossy().into_owned(),
                Some(hex_digest(&self.docker_sha256)),
                started_at_ms,
            ))
            .map_err(|_| "RESTORE_PROCESS_RECORD_FAILED")?;
        self.repository
            .mark_process_running(claim, &process_instance_id, started_at_ms)
            .map_err(|_| "RESTORE_PROCESS_RECORD_FAILED")?;

        let deadline = Instant::now() + Duration::from_secs(120);
        let (exit, reason) = loop {
            match process
                .wait_timeout(Duration::from_millis(250))
                .map_err(|_| "RESTORE_PROCESS_WAIT_FAILED")?
            {
                Some(exit) => break (exit, "exited"),
                None => {
                    let heartbeat_at_ms = SystemRuntimeClock.now_ms();
                    if self
                        .repository
                        .claim_cancel_requested(claim, heartbeat_at_ms)
                        .map_err(|_| "RESTORE_PROCESS_RECORD_FAILED")?
                    {
                        let exit = process
                            .terminate()
                            .map_err(|_| "RESTORE_PROCESS_TERMINATE_FAILED")?;
                        break (exit, "cancelled");
                    }
                    if Instant::now() >= deadline {
                        let exit = process
                            .terminate()
                            .map_err(|_| "RESTORE_PROCESS_TERMINATE_FAILED")?;
                        break (exit, "timeout");
                    }
                    self.repository
                        .heartbeat_claim(claim, heartbeat_at_ms, heartbeat_at_ms + 120_000)
                        .map_err(|_| "RESTORE_PROCESS_HEARTBEAT_FAILED")?;
                }
            }
        };
        self.repository
            .record_process_exit(
                claim,
                ProcessExitSpec::new(
                    process_instance_id,
                    SystemRuntimeClock.now_ms(),
                    i64::from(exit.exit_code()),
                    reason,
                ),
            )
            .map_err(|_| "RESTORE_PROCESS_RECORD_FAILED")?;
        if reason != "exited" {
            return Err(if reason == "cancelled" {
                "RESTORE_PROCESS_CANCELLED"
            } else {
                "RESTORE_PROCESS_TIMEOUT"
            });
        }
        if exit.exit_code() != 0 && !allow_nonzero {
            return Err("RESTORE_PROCESS_EXIT_NONZERO");
        }
        Ok(exit)
    }

    fn restore_error(
        &self,
        code: &'static str,
        target_created: bool,
        phase: &str,
    ) -> RestoreExecutionError {
        RestoreExecutionError::terminal(
            code,
            target_created,
            None,
            json!({
                "phase": phase,
                "imageDigest": POSTGRES_IMAGE_DIGEST,
            }),
        )
        .expect("static live restore error is valid")
    }
}

#[cfg(windows)]
#[async_trait]
impl RestoreExecutor for DockerPostgresRestoreExecutor {
    async fn restore(
        &self,
        claim: &dbbackup_catalog::ClaimedWork,
        context: &dbbackup_catalog::RestoreClaimContext,
    ) -> Result<RestoreExecutionEvidence, RestoreExecutionError> {
        let dump_path = self.workspace.join("artifacts/g2/postgresql/database.dump");
        let globals_path = self.workspace.join("artifacts/g2/postgresql/globals.sql");
        if sha256_file(&dump_path).as_deref() != Some(POSTGRES_G2_DUMP_SHA256)
            || !globals_path.is_file()
        {
            return Err(self.restore_error("RESTORE_SOURCE_ARTIFACT_INVALID", false, "source"));
        }
        let run = self.run_tracked(
            claim,
            "restore.infrastructure",
            vec![
                "run".to_owned(),
                "--detach".to_owned(),
                "--rm".to_owned(),
                "--name".to_owned(),
                self.container_name.clone(),
                "--env".to_owned(),
                "POSTGRES_HOST_AUTH_METHOD=trust".to_owned(),
                "--env".to_owned(),
                "POSTGRES_USER=target_admin".to_owned(),
                "--env".to_owned(),
                "POSTGRES_DB=target_admin".to_owned(),
                POSTGRES_IMAGE.to_owned(),
            ],
            false,
        );
        if let Err(code) = run {
            return Err(self.restore_error(code, false, "infrastructure"));
        }

        let mut ready = false;
        for _ in 0..40 {
            let result = self.run_tracked(
                claim,
                "restore.preflight",
                vec![
                    "exec".to_owned(),
                    self.container_name.clone(),
                    "pg_isready".to_owned(),
                    "-U".to_owned(),
                    "target_admin".to_owned(),
                    "-d".to_owned(),
                    "target_admin".to_owned(),
                ],
                true,
            );
            match result {
                Ok(exit) if exit.exit_code() == 0 => {
                    ready = true;
                    break;
                }
                Ok(_) => std::thread::sleep(std::time::Duration::from_millis(250)),
                Err(code) => return Err(self.restore_error(code, false, "preflight")),
            }
        }
        if !ready {
            return Err(self.restore_error("RESTORE_TARGET_NOT_READY", false, "preflight"));
        }

        for (source, target) in [
            (&globals_path, "/tmp/globals.sql"),
            (&dump_path, "/tmp/database.dump"),
        ] {
            let source = format!("{}:{}", source.display(), "");
            let source = source.trim_end_matches(':').to_owned();
            if let Err(code) = self.run_tracked(
                claim,
                "restore.materialize",
                vec![
                    "cp".to_owned(),
                    source,
                    format!("{}:{target}", self.container_name),
                ],
                false,
            ) {
                return Err(self.restore_error(code, false, "materialize"));
            }
        }
        if let Err(code) = self.run_tracked(
            claim,
            "restore.globals",
            vec![
                "exec".to_owned(),
                self.container_name.clone(),
                "psql".to_owned(),
                "-v".to_owned(),
                "ON_ERROR_STOP=1".to_owned(),
                "-U".to_owned(),
                "target_admin".to_owned(),
                "-d".to_owned(),
                "target_admin".to_owned(),
                "-f".to_owned(),
                "/tmp/globals.sql".to_owned(),
            ],
            false,
        ) {
            return Err(self.restore_error(code, false, "globals"));
        }
        if let Err(code) = self.run_tracked(
            claim,
            "restore.create_target",
            vec![
                "exec".to_owned(),
                self.container_name.clone(),
                "createdb".to_owned(),
                "-U".to_owned(),
                "target_admin".to_owned(),
                "-T".to_owned(),
                "template0".to_owned(),
                "-O".to_owned(),
                "app_owner".to_owned(),
                context.target_database_name().to_owned(),
            ],
            false,
        ) {
            return Err(self.restore_error(code, false, "create_target"));
        }
        if let Err(code) = self.run_tracked(
            claim,
            "restore.native",
            vec![
                "exec".to_owned(),
                self.container_name.clone(),
                "pg_restore".to_owned(),
                "-U".to_owned(),
                "target_admin".to_owned(),
                "-d".to_owned(),
                context.target_database_name().to_owned(),
                "--exit-on-error".to_owned(),
                "--single-transaction".to_owned(),
                "/tmp/database.dump".to_owned(),
            ],
            false,
        ) {
            return Err(self.restore_error(code, true, "native_restore"));
        }
        RestoreExecutionEvidence::completed(
            0,
            true,
            json!({
                "provider": "postgresql",
                "mode": "create_new",
                "imageDigest": POSTGRES_IMAGE_DIGEST,
                "artifactSha256": POSTGRES_G2_DUMP_SHA256,
                "target": context.target_database_name(),
            }),
        )
    }

    async fn validate(
        &self,
        claim: &dbbackup_catalog::ClaimedWork,
        context: &dbbackup_catalog::RestoreClaimContext,
        _restore: &RestoreExecutionEvidence,
    ) -> Result<RestoreValidationEvidence, RestoreValidationError> {
        let oracle_sql = concat!(
            "SELECT 'rows=' || count(*) FROM app.items;",
            "SELECT 'items=' || string_agg(id::text || ':' || label || ':' || ",
            "encode(payload, 'hex'), ',' ORDER BY id) FROM app.items;",
            "SELECT 'view=' || string_agg(id::text || ':' || label || ':' || ",
            "payload_hex, ',' ORDER BY id) FROM app.item_summary;",
            "SELECT 'function=' || app.item_label(2);",
            "SELECT 'trigger=' || count(*) FROM pg_trigger ",
            "WHERE tgname = 'items_normalize_label' AND NOT tgisinternal;",
            "SELECT 'blob=' || encode(digest(lo_get(object_id), 'sha256'), 'hex') ",
            "FROM app.large_objects WHERE id = 1;",
            "SELECT 'owner=' || tableowner FROM pg_tables ",
            "WHERE schemaname = 'app' AND tablename = 'items';",
            "SELECT 'extension=' || count(*) FROM pg_extension WHERE extname = 'pgcrypto';"
        );
        let exit = self
            .run_tracked(
                claim,
                "validation.content_oracle",
                vec![
                    "exec".to_owned(),
                    self.container_name.clone(),
                    "psql".to_owned(),
                    "-At".to_owned(),
                    "-v".to_owned(),
                    "ON_ERROR_STOP=1".to_owned(),
                    "-U".to_owned(),
                    "target_admin".to_owned(),
                    "-d".to_owned(),
                    context.target_database_name().to_owned(),
                    "-c".to_owned(),
                    oracle_sql.to_owned(),
                ],
                false,
            )
            .map_err(|code| {
                RestoreValidationError::failed(
                    code,
                    json!({
                        "provider": "postgresql",
                        "oracle": "schema-row-content",
                        "matched": false,
                    }),
                )
                .expect("static validation error is valid")
            })?;
        let output = String::from_utf8(exit.stdout().tail().to_vec()).map_err(|_| {
            RestoreValidationError::failed(
                "RESTORE_ORACLE_OUTPUT_INVALID",
                json!({"provider": "postgresql", "matched": false}),
            )
            .expect("static validation error is valid")
        })?;
        let mut oracle_lines = output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        oracle_lines.sort_unstable();
        let normalized = oracle_lines.join("\n");
        let oracle_sha256 = sha256_bytes(normalized.as_bytes());
        let marker_count = normalized.lines().count();
        if oracle_sha256 != POSTGRES_G2_ORACLE_SHA256 || marker_count != 8 {
            return Err(RestoreValidationError::failed(
                "RESTORE_ORACLE_MISMATCH",
                json!({
                    "provider": "postgresql",
                    "oracle": "schema-row-content",
                    "oracleSha256": oracle_sha256,
                    "markerCount": marker_count,
                    "matched": false,
                }),
            )
            .expect("static validation error is valid"));
        }
        RestoreValidationEvidence::passed(json!({
            "provider": "postgresql",
            "oracle": "schema-row-content",
            "oracleSha256": oracle_sha256,
            "markerCount": marker_count,
            "matched": true,
        }))
    }
}

#[cfg(windows)]
impl Drop for DockerPostgresRestoreExecutor {
    fn drop(&mut self) {
        use std::process::{Command, Stdio};

        let _ = Command::new(&self.docker_path)
            .args(["rm", "--force", &self.container_name])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(windows)]
fn windows_filetime_to_unix_ms(value: u64) -> Option<i64> {
    const WINDOWS_TO_UNIX_EPOCH_100NS: u64 = 116_444_736_000_000_000;
    value
        .checked_sub(WINDOWS_TO_UNIX_EPOCH_100NS)
        .and_then(|value| i64::try_from(value / 10_000).ok())
}

#[cfg(windows)]
fn sha256_file(path: &std::path::Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Some(hex_digest(&hasher.finalize().into()))
}

#[cfg(windows)]
fn sha256_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex_digest(&Sha256::digest(bytes).into())
}

#[cfg(windows)]
fn hex_digest(bytes: &[u8; 32]) -> String {
    use std::fmt::Write;

    let mut result = String::with_capacity(64);
    for byte in bytes {
        let _ = write!(result, "{byte:02x}");
    }
    result
}

struct ScriptedExecutor {
    restore_calls: AtomicI64,
    validation_calls: AtomicI64,
    validation_passes: bool,
}

struct DeferredExecutor;

struct AdmissionCancellingExecutor {
    repository: dbbackup_catalog::RuntimeRepository,
    work_item_id: WorkItemId,
}

#[async_trait]
impl RestoreExecutor for AdmissionCancellingExecutor {
    async fn admit(
        &self,
        claim: &dbbackup_catalog::ClaimedWork,
        _context: &dbbackup_catalog::RestoreClaimContext,
    ) -> Result<RestoreAdmission, RestoreExecutionError> {
        self.repository
            .request_cancel(&self.work_item_id, claim.fencing_token(), 1_135)
            .expect("request cancellation during admission");
        Err(RestoreExecutionError::retryable(
            "RECOVERY_CANCEL_REQUESTED",
            json!({"phase": "admission"}),
        )
        .expect("valid cancellation observation"))
    }

    async fn restore(
        &self,
        _claim: &dbbackup_catalog::ClaimedWork,
        _context: &dbbackup_catalog::RestoreClaimContext,
    ) -> Result<RestoreExecutionEvidence, RestoreExecutionError> {
        panic!("cancelled admission must not execute restore")
    }

    async fn validate(
        &self,
        _claim: &dbbackup_catalog::ClaimedWork,
        _context: &dbbackup_catalog::RestoreClaimContext,
        _restore: &RestoreExecutionEvidence,
    ) -> Result<RestoreValidationEvidence, RestoreValidationError> {
        panic!("cancelled admission must not validate")
    }
}

#[async_trait]
impl RestoreExecutor for DeferredExecutor {
    async fn admit(
        &self,
        _claim: &dbbackup_catalog::ClaimedWork,
        _context: &dbbackup_catalog::RestoreClaimContext,
    ) -> Result<RestoreAdmission, RestoreExecutionError> {
        Ok(RestoreAdmission::deferred("spool_capacity:fixture").expect("valid wait reason"))
    }

    async fn restore(
        &self,
        _claim: &dbbackup_catalog::ClaimedWork,
        _context: &dbbackup_catalog::RestoreClaimContext,
    ) -> Result<RestoreExecutionEvidence, RestoreExecutionError> {
        panic!("deferred admission must not execute restore")
    }

    async fn validate(
        &self,
        _claim: &dbbackup_catalog::ClaimedWork,
        _context: &dbbackup_catalog::RestoreClaimContext,
        _restore: &RestoreExecutionEvidence,
    ) -> Result<RestoreValidationEvidence, RestoreValidationError> {
        panic!("deferred admission must not validate")
    }
}

impl ScriptedExecutor {
    fn passing() -> Self {
        Self {
            restore_calls: AtomicI64::new(0),
            validation_calls: AtomicI64::new(0),
            validation_passes: true,
        }
    }

    fn oracle_failure() -> Self {
        Self {
            restore_calls: AtomicI64::new(0),
            validation_calls: AtomicI64::new(0),
            validation_passes: false,
        }
    }
}

#[async_trait]
impl RestoreExecutor for ScriptedExecutor {
    async fn restore(
        &self,
        _claim: &dbbackup_catalog::ClaimedWork,
        context: &dbbackup_catalog::RestoreClaimContext,
    ) -> Result<RestoreExecutionEvidence, RestoreExecutionError> {
        assert!(context.authoritatively_eligible());
        assert_eq!(context.conflict_policy(), "create_new");
        self.restore_calls.fetch_add(1, Ordering::SeqCst);
        RestoreExecutionEvidence::completed(
            0,
            true,
            json!({
                "tool": "fixture-restorer",
                "target": context.target_database_name()
            }),
        )
    }

    async fn validate(
        &self,
        _claim: &dbbackup_catalog::ClaimedWork,
        _context: &dbbackup_catalog::RestoreClaimContext,
        _restore: &RestoreExecutionEvidence,
    ) -> Result<RestoreValidationEvidence, RestoreValidationError> {
        self.validation_calls.fetch_add(1, Ordering::SeqCst);
        if self.validation_passes {
            RestoreValidationEvidence::passed(json!({
                "oracle": "schema-row-content",
                "matched": true
            }))
        } else {
            Err(RestoreValidationError::failed(
                "RESTORE_ORACLE_MISMATCH",
                json!({
                    "oracle": "schema-row-content",
                    "matched": false
                }),
            )
            .expect("validation error"))
        }
    }
}

struct CancellingExecutor {
    repository: dbbackup_catalog::RuntimeRepository,
    work_item_id: WorkItemId,
    release_calls: AtomicI64,
}

struct NativeCancellationExecutor {
    repository: dbbackup_catalog::RuntimeRepository,
    work_item_id: WorkItemId,
}

#[async_trait]
impl RestoreExecutor for NativeCancellationExecutor {
    async fn restore(
        &self,
        claim: &dbbackup_catalog::ClaimedWork,
        _context: &dbbackup_catalog::RestoreClaimContext,
    ) -> Result<RestoreExecutionEvidence, RestoreExecutionError> {
        self.repository
            .request_cancel(&self.work_item_id, claim.fencing_token(), 1_135)
            .expect("request cancellation during native restore");
        Err(RestoreExecutionError::terminal(
            "RECOVERY_CANCEL_REQUESTED",
            true,
            None,
            json!({"phase": "native", "targetCreated": true}),
        )
        .expect("valid native cancellation error"))
    }

    async fn validate(
        &self,
        _claim: &dbbackup_catalog::ClaimedWork,
        _context: &dbbackup_catalog::RestoreClaimContext,
        _restore: &RestoreExecutionEvidence,
    ) -> Result<RestoreValidationEvidence, RestoreValidationError> {
        panic!("native cancellation must not validate")
    }
}

#[async_trait]
impl RestoreExecutor for CancellingExecutor {
    fn release_claim_resources(
        &self,
        _claim: &dbbackup_catalog::ClaimedWork,
        _context: &dbbackup_catalog::RestoreClaimContext,
    ) {
        self.release_calls.fetch_add(1, Ordering::SeqCst);
    }

    async fn restore(
        &self,
        _claim: &dbbackup_catalog::ClaimedWork,
        _context: &dbbackup_catalog::RestoreClaimContext,
    ) -> Result<RestoreExecutionEvidence, RestoreExecutionError> {
        self.repository
            .request_cancel(&self.work_item_id, 1, 1_135)
            .expect("request cancellation after target creation");
        RestoreExecutionEvidence::completed(
            0,
            true,
            json!({"tool": "fixture-restorer", "targetCreated": true}),
        )
    }

    async fn validate(
        &self,
        _claim: &dbbackup_catalog::ClaimedWork,
        _context: &dbbackup_catalog::RestoreClaimContext,
        _restore: &RestoreExecutionEvidence,
    ) -> Result<RestoreValidationEvidence, RestoreValidationError> {
        panic!("validation must not run after cancellation");
    }
}

struct IncrementingClock(AtomicI64);

impl IncrementingClock {
    fn new(now_ms: i64) -> Self {
        Self(AtomicI64::new(now_ms))
    }
}

impl RuntimeClock for IncrementingClock {
    fn now_ms(&self) -> i64 {
        self.0.fetch_add(10, Ordering::SeqCst)
    }
}

#[derive(Debug)]
struct RestoreRow {
    state: String,
    validation_result_json: Option<String>,
    measured_rto_ms: Option<i64>,
    error_code: Option<String>,
}

#[derive(Debug)]
struct DrillVerificationRow {
    state: String,
    achieved_level: Option<i64>,
    restore_run_id: String,
    evidence_json: Option<String>,
    error_code: Option<String>,
}

struct Fixture {
    _temp: TempDir,
    db_path: std::path::PathBuf,
    store: OperationalStore,
    restore_run_id: RestoreRunId,
    work_item_id: WorkItemId,
    worker_start_ms: i64,
}

impl Fixture {
    fn new() -> Self {
        Self::build(
            1_000,
            5_000,
            "17.4",
            r"C:\dbbackup\tools\pg_restore.exe",
            "f".repeat(64),
        )
    }

    fn drill() -> Self {
        Self::build_with_purpose(
            1_000,
            5_000,
            "17.4",
            r"C:\dbbackup\tools\pg_restore.exe",
            "f".repeat(64),
            RestorePurpose::RestoreDrill,
        )
    }

    fn build(
        base_ms: i64,
        expires_at_ms: i64,
        server_version: &str,
        restore_tool_path: &str,
        restore_tool_sha256: String,
    ) -> Self {
        Self::build_with_purpose(
            base_ms,
            expires_at_ms,
            server_version,
            restore_tool_path,
            restore_tool_sha256,
            RestorePurpose::IncidentRecovery,
        )
    }

    fn build_with_purpose(
        base_ms: i64,
        expires_at_ms: i64,
        server_version: &str,
        restore_tool_path: &str,
        restore_tool_sha256: String,
        purpose: RestorePurpose,
    ) -> Self {
        let temp = TempDir::new().expect("temp directory");
        let db_path = temp.path().join("operational.db");
        let store = OperationalStore::open(db_path.clone()).expect("open store");
        let target_connection_id = ConnectionProfileId::new();
        store
            .configuration()
            .insert_connection(
                ConnectionProfileSpec::tcp(
                    target_connection_id.clone(),
                    "Recovery PostgreSQL",
                    "postgresql",
                    "recovery.internal",
                    5432,
                    100,
                )
                .with_restore_test_target(purpose == RestorePurpose::RestoreDrill),
            )
            .expect("insert target connection");
        let recovery_point_id = RecoveryPointId::new();
        let source_replica_id = ArtifactReplicaId::new();
        seed_restore_truth(
            &db_path,
            &target_connection_id,
            &recovery_point_id,
            &source_replica_id,
            base_ms,
        );
        certify_postgresql(&store, base_ms, restore_tool_path, restore_tool_sha256);
        seed_target_preflight_evidence(
            &store,
            &db_path,
            &target_connection_id,
            server_version,
            base_ms,
        );
        let restore_run_id = RestoreRunId::new();
        let work_item_id = WorkItemId::new();
        let target_fingerprint = blake3::hash(
            format!(
                "restore-target-v2\0{}\0{}\0{}\0{}\0{}",
                target_connection_id.as_str(),
                "postgresql",
                "orders_restore",
                server_version,
                PG_SERVER_IDENTITY
            )
            .as_bytes(),
        )
        .to_hex()
        .to_string();
        let preflight_version = blake3::hash(
            format!(
                "restore-preflight-v1\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}",
                recovery_point_id.as_str(),
                base_ms - 700,
                source_replica_id.as_str(),
                target_fingerprint,
                "create_new",
                base_ms - 100,
                base_ms - 90,
                0,
                true
            )
            .as_bytes(),
        )
        .to_hex()
        .to_string();
        store
            .recovery()
            .enqueue_safe(
                SafeRestoreSpec::new(
                    restore_run_id.clone(),
                    work_item_id.clone(),
                    recovery_point_id.clone(),
                    source_replica_id.as_str(),
                    target_connection_id.clone(),
                    "orders_restore",
                    &target_fingerprint,
                    &preflight_version,
                    json!({
                        "eligible": true,
                        "mode": "create_new",
                        "purpose": purpose.as_str(),
                        "evaluatedAtMs": base_ms - 50,
                        "expiresAtMs": expires_at_ms,
                        "preflightVersion": preflight_version,
                        "targetFingerprint": target_fingerprint,
                        "targetConnectionId": target_connection_id.as_str(),
                        "targetName": "orders_restore",
                        "targetExists": false,
                        "serverIdentity": PG_SERVER_IDENTITY,
                        "restoreTestTarget": purpose == RestorePurpose::RestoreDrill,
                        "targetRole": if purpose == RestorePurpose::RestoreDrill {
                            "restore_test"
                        } else {
                            "general"
                        },
                        "source": {
                            "snapshotId": recovery_point_id.as_str(),
                            "replicaId": source_replica_id.as_str()
                        }
                    })
                    .to_string(),
                    base_ms,
                )
                .with_purpose(purpose),
            )
            .expect("enqueue safe restore");
        Self {
            _temp: temp,
            db_path,
            store,
            restore_run_id,
            work_item_id,
            worker_start_ms: base_ms + 100,
        }
    }

    fn worker(&self, executor: Arc<dyn RestoreExecutor>) -> RestoreWorker {
        RestoreWorker::new(
            self.store.runtime(),
            executor,
            Arc::new(IncrementingClock::new(self.worker_start_ms)),
            RestoreWorkerConfig::new("restore-worker", "boot-a", 10_000, 100)
                .expect("worker config"),
        )
    }

    fn live_worker(&self, executor: Arc<dyn RestoreExecutor>) -> RestoreWorker {
        RestoreWorker::new(
            self.store.runtime(),
            executor,
            Arc::new(SystemRuntimeClock),
            RestoreWorkerConfig::new(
                "restore-worker-live",
                uuid::Uuid::new_v4().to_string(),
                120_000,
                1_000,
            )
            .expect("live worker config"),
        )
    }

    fn restore_row(&self) -> RestoreRow {
        Connection::open(&self.db_path)
            .expect("open restore query")
            .query_row(
                "SELECT state, validation_result_json, measured_rto_ms, error_code
                 FROM restore_runs WHERE id = ?1",
                [self.restore_run_id.as_str()],
                |row| {
                    Ok(RestoreRow {
                        state: row.get(0)?,
                        validation_result_json: row.get(1)?,
                        measured_rto_ms: row.get(2)?,
                        error_code: row.get(3)?,
                    })
                },
            )
            .expect("restore row")
    }

    fn drill_verification_row(&self) -> DrillVerificationRow {
        Connection::open(&self.db_path)
            .expect("open drill verification query")
            .query_row(
                "SELECT state, achieved_level, restore_run_id, evidence_json, error_code
                 FROM verification_runs
                 WHERE restore_run_id = ?1 AND trigger_kind = 'restore_drill'",
                [self.restore_run_id.as_str()],
                |row| {
                    Ok(DrillVerificationRow {
                        state: row.get(0)?,
                        achieved_level: row.get(1)?,
                        restore_run_id: row.get(2)?,
                        evidence_json: row.get(3)?,
                        error_code: row.get(4)?,
                    })
                },
            )
            .expect("drill verification row")
    }

    fn drill_projection_row(&self) -> (i64, String, Option<i64>) {
        Connection::open(&self.db_path)
            .expect("open drill projection query")
            .query_row(
                "SELECT projection.verify_level, projection.result,
                        snapshot.last_verified_at_ms
                 FROM verification_runs AS verification
                 JOIN catalog_verification_projection AS projection
                   ON projection.origin_verification_run_id = verification.id
                 JOIN catalog_snapshot_projection AS snapshot
                   ON snapshot.recovery_point_id = verification.recovery_point_id
                 WHERE verification.restore_run_id = ?1
                   AND verification.trigger_kind = 'restore_drill'",
                [self.restore_run_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("drill verification projection")
    }

    fn snapshot_verification_row(&self) -> (Option<i64>, Option<i64>) {
        Connection::open(&self.db_path)
            .expect("open snapshot verification query")
            .query_row(
                "SELECT snapshot.highest_verified_level, snapshot.last_verified_at_ms
                 FROM restore_runs AS restore
                 JOIN catalog_snapshot_projection AS snapshot
                   ON snapshot.recovery_point_id = restore.recovery_point_id
                 WHERE restore.id = ?1",
                [self.restore_run_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("snapshot verification row")
    }

    fn work_state(&self) -> String {
        Connection::open(&self.db_path)
            .expect("open work query")
            .query_row(
                "SELECT state FROM work_items WHERE id = ?1",
                [self.work_item_id.as_str()],
                |row| row.get(0),
            )
            .expect("work state")
    }

    fn success_count(&self) -> i64 {
        Connection::open(&self.db_path)
            .expect("open success query")
            .query_row(
                "SELECT COUNT(*) FROM restore_runs WHERE state = 'succeeded'",
                [],
                |row| row.get(0),
            )
            .expect("success count")
    }

    fn process_count(&self) -> i64 {
        Connection::open(&self.db_path)
            .expect("open process query")
            .query_row(
                "SELECT COUNT(*)
                 FROM process_instances AS process
                 JOIN work_attempts AS attempt
                   ON attempt.id = process.work_attempt_id
                 WHERE attempt.work_item_id = ?1",
                [self.work_item_id.as_str()],
                |row| row.get(0),
            )
            .expect("process count")
    }
}

fn certify_postgresql(
    store: &OperationalStore,
    base_ms: i64,
    restore_tool_path: &str,
    restore_tool_sha256: String,
) {
    let mut certification = EngineCertificationSpec::candidate(
        "cert-postgresql-17-restore-worker",
        "postgresql",
        ">=17,<18",
        "pg_restore",
        ">=17,<18",
        "windows",
        "x86_64",
        "custom",
        "application-consistent",
        "evidence/restore-worker.json",
        "e".repeat(64),
        base_ms - 300,
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
        .expect("insert certification candidate");
    store
        .certification()
        .certify(
            "cert-postgresql-17-restore-worker",
            base_ms - 200,
            base_ms + 600_000,
        )
        .expect("certify PostgreSQL");
    let tool = store
        .certification()
        .record_tool(NativeToolSpec::observed(
            "tool-pg-restore-worker",
            "pg_restore",
            restore_tool_path,
            "17.4",
            "x86_64",
            restore_tool_sha256,
            "bundled",
            base_ms - 220,
        ))
        .expect("record restore tool");
    store
        .certification()
        .trust_verified_tool(
            tool.id(),
            "PostgreSQL Global Development Group",
            base_ms - 190,
        )
        .expect("trust restore tool");
}

fn seed_target_preflight_evidence(
    store: &OperationalStore,
    database_path: &std::path::Path,
    connection_id: &ConnectionProfileId,
    server_version: &str,
    base_ms: i64,
) {
    store
        .configuration()
        .record_connection_probe(ConnectionProbeSpec::passed(
            uuid::Uuid::new_v4().to_string(),
            connection_id.clone(),
            server_version,
            json!({
                "restorePreflight": true,
                "serverIdentity": PG_SERVER_IDENTITY
            })
            .to_string(),
            base_ms - 120,
            base_ms - 100,
        ))
        .expect("record target probe");
    store
        .configuration()
        .record_discovery_snapshot(DiscoverySnapshotSpec::successful(
            uuid::Uuid::new_v4().to_string(),
            connection_id.clone(),
            vec![DiscoveredDatabaseSpec::eligible(
                "database:orders",
                "orders",
                "orders",
                "postgresql:recovery.internal:5432:orders",
            )],
            base_ms - 99,
            base_ms - 95,
        ))
        .expect("record target discovery");
    store
        .runtime()
        .register_spool_volume(SpoolVolumeSpec::ready(
            uuid::Uuid::new_v4().to_string(),
            "restore-spool",
            r"D:\DbBackup\spool",
            10_000_000,
            8_000_000,
            1_000_000,
            9_000_000,
            base_ms - 90,
        ))
        .expect("register restore spool");
    let direct = Connection::open(database_path).expect("open credential fixture");
    let secret_ref_id = uuid::Uuid::new_v4().to_string();
    direct
        .execute(
            "INSERT INTO secret_refs(
                 id, provider, provider_locator, purpose, version, state,
                 fingerprint, created_at_ms
             ) VALUES (
                 ?1, 'external', ?2, 'database_auth', 1, 'active',
                 ?3, ?4
             )",
            params![
                secret_ref_id,
                format!("fixture:{secret_ref_id}"),
                "1".repeat(64),
                base_ms - 150
            ],
        )
        .expect("insert database auth reference");
    direct
        .execute(
            "INSERT INTO connection_secret_bindings(
                 connection_profile_id, secret_role, secret_ref_id, created_at_ms
             ) VALUES (?1, 'database_auth', ?2, ?3)",
            params![connection_id.as_str(), secret_ref_id, base_ms - 150],
        )
        .expect("bind database auth reference");
}

fn seed_restore_truth(
    database_path: &std::path::Path,
    target_connection_id: &ConnectionProfileId,
    recovery_point_id: &RecoveryPointId,
    source_replica_id: &ArtifactReplicaId,
    base_ms: i64,
) {
    let connection = Connection::open(database_path).expect("open restore truth fixture");
    connection
        .execute_batch("PRAGMA foreign_keys=ON;")
        .expect("enable foreign keys");
    connection
        .execute(
            "UPDATE connection_profiles
             SET state = 'ready', updated_at_ms = ?2
             WHERE id = ?1",
            params![target_connection_id.as_str(), base_ms - 800],
        )
        .expect("mark target ready");
    connection
        .execute(
            "INSERT INTO catalog_recovery_points(
                 id, origin_instance_id, source_target_identity, source_display_name,
                 engine, state, consistency_grade, committed_at_ms,
                 manifest_schema_version, set_manifest_checksum_blake3,
                 signing_key_id, signature_status,
                 required_part_count, present_part_count, plaintext_bytes,
                 engine_metadata_json, first_ingested_at_ms, last_ingested_at_ms
             ) VALUES (
                 ?1, 'fixture-instance', 'source:orders', 'orders',
                 'postgresql', 'complete', 'application_consistent', ?3,
                 2, ?2, 'signing-key-01', 'valid',
                 1, 1, 1000, '{}', ?3, ?3
             )",
            params![recovery_point_id.as_str(), "d".repeat(64), base_ms - 700],
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
                 'none', 'eligible', ?3, ?3, '{}'
             )",
            params![
                source_replica_id.as_str(),
                recovery_point_id.as_str(),
                base_ms - 700
            ],
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
                 'postgresql', ?2, 'complete', 'healthy',
                 'compliant', 'eligible', 'orders',
                 1, ?2
             )",
            params![recovery_point_id.as_str(), base_ms - 700],
        )
        .expect("insert snapshot projection");
}
