use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ed25519_dalek::VerifyingKey;
use restoreplane_catalog::{
    BackupCommitRecoveryContext, BackupDestinationBinding, ClaimedWork, CompleteRecoveryPointSpec,
    OperationalStore,
};
use restoreplane_core::Engine;
use restoreplane_domain::DestinationRequirement;
use restoreplane_pipeline::{
    DestinationReaderV2, ReaderStatusV2, ReplicaPublishError, ReplicaPublishOutcomeV2,
    ReplicaRequirementV2, SignedSetManifestV2, UploadReceipt, MAX_SET_MANIFEST_BYTES_V2,
};
use restoreplane_runtime::{
    ReconcileConfig, ReconcileError, ReconcileOutcome, StartupReconciler, SystemRuntimeClock,
    VerifiedRecoveryPointSource, WindowsProcessInspector,
};

use crate::host::{ClaimRuntime, HostError};
use crate::operations::destination::resolve_recovery_reader;
use crate::secrets::ServiceSecretStore;

const STARTUP_RETRY_DELAY_MS: i64 = 5_000;
const STARTUP_SCAN_LIMIT: usize = 1_000;
const STARTUP_RECOVERY_VERIFY_TIMEOUT: Duration = Duration::from_secs(20);
const RECOVERY_RECONCILE_INTERVAL: Duration = Duration::from_secs(5);
const MAX_RECOVERY_DESTINATIONS: usize = 1_024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StartupRecoveryReport {
    pub recovered_commits: usize,
    pub requeued: usize,
    pub cleanup_required: usize,
    pub attention_required: usize,
}

type RecoveryCycle = dyn Fn() -> Result<StartupRecoveryReport, ReconcileError> + Send + Sync;

/// Owns reconciliation after startup. The first synchronous startup pass remains the readiness
/// gate; this runtime then repeats bounded passes so claims that expire shortly after a fast
/// service restart are not stranded until the next restart.
pub(crate) struct ServiceRecoveryRuntimeHandle {
    stop: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl ServiceRecoveryRuntimeHandle {
    pub(crate) fn spawn(
        store: OperationalStore,
        secrets: Arc<ServiceSecretStore>,
        trusted_signing_keys: BTreeMap<String, VerifyingKey>,
    ) -> Result<Self, HostError> {
        let cycle = Arc::new(move || {
            // A source is intentionally created for every pass. Its 20 second shared verification
            // budget therefore applies to one bounded scan, never to the lifetime of the service.
            reconcile_expired_work(
                &store,
                Arc::new(ServiceVerifiedRecoveryPointSource::new(
                    store.clone(),
                    Arc::clone(&secrets),
                    trusted_signing_keys.clone(),
                )),
            )
        });
        Self::spawn_with_cycle(RECOVERY_RECONCILE_INTERVAL, cycle)
    }

    fn spawn_with_cycle(interval: Duration, cycle: Arc<RecoveryCycle>) -> Result<Self, HostError> {
        if interval.is_zero() {
            return Err(HostError::InvalidConfig);
        }
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("restoreplane-recovery-reconciler".to_owned())
            .spawn(move || {
                while !thread_stop.load(Ordering::Acquire) {
                    match cycle() {
                        Ok(report)
                            if report.recovered_commits > 0
                                || report.requeued > 0
                                || report.cleanup_required > 0
                                || report.attention_required > 0 =>
                        {
                            tracing::info!(
                                recovered_commits = report.recovered_commits,
                                requeued = report.requeued,
                                cleanup_required = report.cleanup_required,
                                attention_required = report.attention_required,
                                "periodic expired-work reconciliation completed"
                            );
                        }
                        Ok(_) => {}
                        Err(error) => tracing::error!(
                            error_code = error.code(),
                            "periodic expired-work reconciliation failed"
                        ),
                    }
                    if thread_stop.load(Ordering::Acquire) {
                        break;
                    }
                    thread::park_timeout(interval);
                }
            })
            .map_err(|_| HostError::RuntimeFailed)?;
        Ok(Self {
            stop,
            thread: Mutex::new(Some(thread)),
        })
    }

    fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
        if let Ok(thread) = self.thread.lock() {
            if let Some(thread) = thread.as_ref() {
                thread.thread().unpark();
            }
        }
    }

    fn wait_stopped(&self) -> Result<(), HostError> {
        let thread = self
            .thread
            .lock()
            .map_err(|_| HostError::InternalState)?
            .take();
        if let Some(thread) = thread {
            thread.join().map_err(|_| HostError::RuntimeFailed)?;
        }
        Ok(())
    }
}

impl ClaimRuntime for ServiceRecoveryRuntimeHandle {
    fn request_stop(&self) {
        Self::request_stop(self);
    }

    fn wait_stopped(&self) -> Result<(), HostError> {
        Self::wait_stopped(self)
    }
}

impl Drop for ServiceRecoveryRuntimeHandle {
    fn drop(&mut self) {
        self.request_stop();
        if let Ok(thread) = self.thread.get_mut() {
            if let Some(thread) = thread.take() {
                let _ = thread.join();
            }
        }
    }
}

pub(crate) struct ServiceVerifiedRecoveryPointSource {
    store: OperationalStore,
    secrets: Arc<ServiceSecretStore>,
    trusted_signing_keys: BTreeMap<String, VerifyingKey>,
    verification_budget: RecoveryVerificationBudget,
}

impl ServiceVerifiedRecoveryPointSource {
    pub(crate) fn new(
        store: OperationalStore,
        secrets: Arc<ServiceSecretStore>,
        trusted_signing_keys: BTreeMap<String, VerifyingKey>,
    ) -> Self {
        Self {
            store,
            secrets,
            trusted_signing_keys,
            verification_budget: RecoveryVerificationBudget::new(STARTUP_RECOVERY_VERIFY_TIMEOUT),
        }
    }

    async fn verify_exact_commit(&self, claim: &ClaimedWork) -> Option<CompleteRecoveryPointSpec> {
        let context = self
            .store
            .runtime()
            .backup_commit_recovery_context(claim)
            .ok()?;
        let recovery_point_id = context.recovery_point_id()?;
        let manifest_key = context.manifest_object_key()?;
        let bindings = self
            .store
            .configuration()
            .backup_destination_bindings(context.plan_revision_id())
            .ok()?;
        if bindings.is_empty()
            || bindings.len() > MAX_RECOVERY_DESTINATIONS
            || !bindings
                .iter()
                .any(|binding| binding.requirement() == DestinationRequirement::Required)
        {
            return None;
        }

        let mut observations = BTreeMap::new();
        let mut canonical_bytes: Option<Vec<u8>> = None;
        let mut canonical_signed: Option<SignedSetManifestV2> = None;
        for binding in bindings
            .iter()
            .filter(|binding| binding.requirement() == DestinationRequirement::Required)
        {
            let observation = self
                .verify_replica(binding, &context, &bindings, &manifest_key)
                .await?;
            if canonical_bytes
                .as_ref()
                .is_some_and(|bytes| bytes != &observation.signed_bytes)
            {
                return None;
            }
            canonical_bytes.get_or_insert_with(|| observation.signed_bytes.clone());
            canonical_signed.get_or_insert_with(|| observation.signed.clone());
            if observations
                .insert(binding.fingerprint(), observation)
                .is_some()
            {
                return None;
            }
        }

        for binding in bindings
            .iter()
            .filter(|binding| binding.requirement() == DestinationRequirement::BestEffort)
        {
            let Some(observation) = self
                .verify_replica(binding, &context, &bindings, &manifest_key)
                .await
            else {
                continue;
            };
            if canonical_bytes
                .as_ref()
                .is_some_and(|bytes| bytes != &observation.signed_bytes)
            {
                return None;
            }
            if observations
                .insert(binding.fingerprint(), observation)
                .is_some()
            {
                return None;
            }
        }

        let signed = canonical_signed?;
        if signed.manifest.recovery_point_id != recovery_point_id.as_uuid() {
            return None;
        }
        let verifying_key = self
            .trusted_signing_keys
            .get(&signed.manifest.signing_key_id)?;
        let mut outcomes = Vec::with_capacity(signed.manifest.replicas.len());
        for replica in &signed.manifest.replicas {
            if let Some(observation) = observations.remove(&replica.destination_fingerprint) {
                outcomes.push(ReplicaPublishOutcomeV2 {
                    destination_fingerprint: replica.destination_fingerprint.clone(),
                    requirement: replica.requirement,
                    reader_status: ReaderStatusV2::Eligible,
                    payload_receipts: observation.payload_receipts,
                    manifest_receipt: Some(observation.manifest_receipt),
                    attempts: 0,
                    result: Ok(()),
                });
            } else {
                outcomes.push(unavailable_outcome(replica));
            }
        }
        if !observations.is_empty() {
            return None;
        }
        let spec = CompleteRecoveryPointSpec::from_verified_publish(
            &signed,
            &outcomes,
            verifying_key,
            unix_time_ms()?,
        )
        .ok()?;
        context.matches_verified_spec(&spec).then_some(spec)
    }

    async fn verify_replica(
        &self,
        binding: &BackupDestinationBinding,
        context: &BackupCommitRecoveryContext,
        bindings: &[BackupDestinationBinding],
        manifest_key: &str,
    ) -> Option<VerifiedReplica> {
        if binding.state() == "disabled" {
            return None;
        }
        let reader =
            resolve_recovery_reader(binding.destination(), Some(self.secrets.as_ref())).ok()?;
        let signed_bytes = reader
            .read_bytes_bounded(manifest_key, MAX_SET_MANIFEST_BYTES_V2)
            .await
            .ok()?;
        let manifest_probe = reader.probe_object(manifest_key).await.ok()?;
        if manifest_probe.bytes != signed_bytes.len() as u64
            || manifest_probe.blake3 != blake3::hash(&signed_bytes).to_hex().to_string()
        {
            return None;
        }
        let signed = serde_json::from_slice::<SignedSetManifestV2>(&signed_bytes).ok()?;
        if signed.manifest.validate().is_err()
            || signed.to_json().ok()?.as_bytes() != signed_bytes
            || !manifest_matches_context(&signed, context, manifest_key)
            || !manifest_matches_bindings(&signed, bindings, manifest_key)
        {
            return None;
        }
        let verifying_key = self
            .trusted_signing_keys
            .get(&signed.manifest.signing_key_id)?;
        signed.verify(verifying_key).ok()?;

        let payload_receipts = verify_payload_metadata(reader.as_ref(), &signed).await?;
        Some(VerifiedReplica {
            signed,
            signed_bytes,
            payload_receipts,
            manifest_receipt: UploadReceipt {
                key: manifest_key.to_owned(),
                bytes: manifest_probe.bytes,
                version: manifest_probe.version,
            },
        })
    }
}

impl VerifiedRecoveryPointSource for ServiceVerifiedRecoveryPointSource {
    fn verified_complete(
        &self,
        claim: &ClaimedWork,
    ) -> Result<Option<CompleteRecoveryPointSpec>, ReconcileError> {
        let Some(remaining_budget) = self.verification_budget.remaining() else {
            return Ok(None);
        };
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(_) => return Ok(None),
        };
        Ok(runtime
            .block_on(async {
                tokio::time::timeout(remaining_budget, self.verify_exact_commit(claim)).await
            })
            .ok()
            .flatten())
    }
}

#[derive(Clone, Copy)]
struct RecoveryVerificationBudget {
    deadline: Instant,
}

impl RecoveryVerificationBudget {
    fn new(limit: Duration) -> Self {
        Self {
            deadline: Instant::now() + limit,
        }
    }

    fn remaining(self) -> Option<Duration> {
        self.deadline.checked_duration_since(Instant::now())
    }
}

struct VerifiedReplica {
    signed: SignedSetManifestV2,
    signed_bytes: Vec<u8>,
    payload_receipts: Vec<UploadReceipt>,
    manifest_receipt: UploadReceipt,
}

async fn verify_payload_metadata(
    reader: &dyn DestinationReaderV2,
    signed: &SignedSetManifestV2,
) -> Option<Vec<UploadReceipt>> {
    // Publishing writes this signed manifest only after the pinned reader has
    // hashed every payload. Startup authenticates that final commit marker and
    // uses metadata here so multi-terabyte objects cannot block SCM startup.
    let mut receipts = Vec::with_capacity(signed.manifest.parts.len());
    for part in &signed.manifest.parts {
        let metadata = reader.stat_object(&part.object_key).await.ok()?;
        if metadata.bytes != part.ciphertext_bytes {
            return None;
        }
        receipts.push(UploadReceipt {
            key: part.object_key.clone(),
            bytes: metadata.bytes,
            version: metadata.version,
        });
    }
    Some(receipts)
}

fn manifest_matches_context(
    signed: &SignedSetManifestV2,
    context: &BackupCommitRecoveryContext,
    manifest_key: &str,
) -> bool {
    let Some(recovery_point_id) = context.recovery_point_id() else {
        return false;
    };
    signed.manifest.recovery_point_id == recovery_point_id.as_uuid()
        && signed.manifest.occurrence_id == context.plan_occurrence_id().as_uuid()
        && signed.manifest.backup_run_id == context.backup_run_id().as_uuid()
        && signed.manifest.target_id == context.backup_scope_id().as_uuid()
        && signed.manifest.engine == parse_engine(context.engine())
        && signed
            .manifest
            .replicas
            .iter()
            .all(|replica| replica.manifest_object_key == manifest_key)
}

fn manifest_matches_bindings(
    signed: &SignedSetManifestV2,
    bindings: &[BackupDestinationBinding],
    manifest_key: &str,
) -> bool {
    if signed.manifest.replicas.len() != bindings.len() {
        return false;
    }
    let mut fingerprints = BTreeSet::new();
    bindings.iter().all(|binding| {
        let fingerprint = binding.fingerprint();
        if !fingerprints.insert(fingerprint.clone()) {
            return false;
        }
        signed.manifest.replicas.iter().any(|replica| {
            replica.destination_fingerprint == fingerprint
                && replica.destination_kind == binding.kind()
                && replica.requirement == map_requirement(binding.requirement())
                && replica.reader_status == ReaderStatusV2::Eligible
                && replica.manifest_object_key == manifest_key
        })
    })
}

fn unavailable_outcome(
    replica: &restoreplane_pipeline::ReplicaCommitmentV2,
) -> ReplicaPublishOutcomeV2 {
    ReplicaPublishOutcomeV2 {
        destination_fingerprint: replica.destination_fingerprint.clone(),
        requirement: replica.requirement,
        reader_status: ReaderStatusV2::Unavailable,
        payload_receipts: Vec::new(),
        manifest_receipt: None,
        attempts: 0,
        result: Err(ReplicaPublishError::ConfigInvalid {
            reason: "replica was unavailable during bounded startup verification".to_owned(),
        }),
    }
}

fn map_requirement(requirement: DestinationRequirement) -> ReplicaRequirementV2 {
    match requirement {
        DestinationRequirement::Required => ReplicaRequirementV2::Required,
        DestinationRequirement::BestEffort => ReplicaRequirementV2::BestEffort,
    }
}

fn parse_engine(engine: &str) -> Engine {
    match engine {
        "postgresql" => Engine::Postgresql,
        "mysql" => Engine::Mysql,
        "mariadb" => Engine::Mariadb,
        "sqlite" => Engine::Sqlite,
        _ => unreachable!("recovery context validates supported backup engines"),
    }
}

fn unix_time_ms() -> Option<i64> {
    let elapsed = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(elapsed.as_millis()).ok()
}

pub fn reconcile_expired_work(
    store: &OperationalStore,
    recovery_point_source: Arc<dyn VerifiedRecoveryPointSource>,
) -> Result<StartupRecoveryReport, ReconcileError> {
    let reconciler = StartupReconciler::new(
        store.runtime(),
        Arc::new(SystemRuntimeClock),
        ReconcileConfig::new(STARTUP_RETRY_DELAY_MS, STARTUP_SCAN_LIMIT)?,
    )
    .with_process_inspector(Arc::new(WindowsProcessInspector))
    .with_recovery_point_source(recovery_point_source);
    let mut report = StartupRecoveryReport::default();
    for outcome in reconciler.reconcile_batch()? {
        match outcome {
            ReconcileOutcome::Idle => {}
            ReconcileOutcome::RecoveredCommit { .. } => {
                report.recovered_commits = report.recovered_commits.saturating_add(1);
            }
            ReconcileOutcome::Requeued { .. } => {
                report.requeued = report.requeued.saturating_add(1);
            }
            ReconcileOutcome::CleanupRequired { .. } => {
                report.cleanup_required = report.cleanup_required.saturating_add(1);
            }
            ReconcileOutcome::AttentionRequired { .. } => {
                report.attention_required = report.attention_required.saturating_add(1);
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use async_trait::async_trait;
    use ed25519_dalek::SigningKey;
    use restoreplane_catalog::{ClaimRequest, OperationWorkSpec};
    use restoreplane_core::{ArtifactPartRole, ConsistencyGrade, PartSpec};
    use restoreplane_domain::{OperationRunId, WorkItemId};
    use restoreplane_pipeline::{
        DestError, EncodingEvidenceV2, ManifestPartV2, NativeToolIdentityV2,
        ReaderObjectMetadataV2, ReaderObjectProbeV2, RecoveryOriginV2, ReplicaCommitmentV2,
        SetManifestV2,
    };
    use uuid::Uuid;

    use super::*;

    struct MetadataOnlyReader {
        expected_key: String,
        bytes: u64,
        stat_called: AtomicBool,
    }

    struct NoVerifiedRecoveryPoint;

    impl VerifiedRecoveryPointSource for NoVerifiedRecoveryPoint {
        fn verified_complete(
            &self,
            _claim: &ClaimedWork,
        ) -> Result<Option<CompleteRecoveryPointSpec>, ReconcileError> {
            Ok(None)
        }
    }

    #[async_trait]
    impl DestinationReaderV2 for MetadataOnlyReader {
        async fn probe_object(&self, _key: &str) -> Result<ReaderObjectProbeV2, DestError> {
            panic!("large payload verification must not read or hash payload bytes")
        }

        async fn read_bytes_bounded(
            &self,
            _key: &str,
            _max_bytes: u64,
        ) -> Result<Vec<u8>, DestError> {
            panic!("large payload verification must not load payload bytes")
        }

        async fn stat_object(&self, key: &str) -> Result<ReaderObjectMetadataV2, DestError> {
            assert_eq!(key, self.expected_key);
            self.stat_called.store(true, Ordering::Release);
            Ok(ReaderObjectMetadataV2 {
                bytes: self.bytes,
                version: Some("immutable-version-1".to_owned()),
            })
        }
    }

    #[tokio::test]
    async fn large_payload_startup_check_uses_metadata_without_reading_or_hashing_object() {
        const LARGE_PAYLOAD_BYTES: u64 = 8 * 1_024 * 1_024 * 1_024 * 1_024;
        let recovery_point_id = Uuid::new_v4();
        let occurrence_id = Uuid::new_v4();
        let target_id = Uuid::new_v4();
        let part_id = Uuid::new_v4();
        let prefix = format!("v2/{occurrence_id}/{target_id}/{recovery_point_id}");
        let payload_key = format!("{prefix}/parts/{part_id}.enc");
        let manifest_key = format!("{prefix}/set-manifest.json");
        let signing_key = SigningKey::from_bytes(&[17_u8; 32]);
        let signed = SignedSetManifestV2::create(
            SetManifestV2::new(
                RecoveryOriginV2::new(
                    recovery_point_id,
                    "instance-a",
                    occurrence_id,
                    Uuid::new_v4(),
                    target_id,
                    "source-a",
                    "large-db",
                )
                .expect("origin"),
                Engine::Postgresql,
                ConsistencyGrade::ApplicationConsistent,
                1,
                "signing-key",
                NativeToolIdentityV2::new("pg_dump", "17.4", "a".repeat(64)).expect("tool"),
                BTreeMap::new(),
                vec![ManifestPartV2::encoded(
                    PartSpec::new(
                        part_id,
                        ArtifactPartRole::DatabaseDump,
                        "large.dump",
                        true,
                        "custom",
                    )
                    .expect("part"),
                    EncodingEvidenceV2::new(
                        "b".repeat(64),
                        "c".repeat(64),
                        LARGE_PAYLOAD_BYTES,
                        LARGE_PAYLOAD_BYTES,
                        "zstd",
                        "chacha20poly1305_stream",
                        1,
                    )
                    .expect("encoding"),
                    payload_key.clone(),
                )
                .expect("manifest part")],
                vec![ReplicaCommitmentV2::new(
                    "local:vault-a",
                    "local",
                    ReplicaRequirementV2::Required,
                    ReaderStatusV2::Eligible,
                    "none",
                    manifest_key,
                )
                .expect("replica")],
            )
            .expect("manifest"),
            &signing_key,
        )
        .expect("signed manifest");
        let reader = MetadataOnlyReader {
            expected_key: payload_key,
            bytes: LARGE_PAYLOAD_BYTES,
            stat_called: AtomicBool::new(false),
        };

        let receipts = verify_payload_metadata(&reader, &signed)
            .await
            .expect("metadata proves the committed object exists at the exact size");

        assert!(reader.stat_called.load(Ordering::Acquire));
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].bytes, LARGE_PAYLOAD_BYTES);
        assert_eq!(receipts[0].version.as_deref(), Some("immutable-version-1"));
    }

    #[test]
    fn startup_recovery_budget_is_shared_and_does_not_reset_per_claim() {
        let budget = RecoveryVerificationBudget {
            deadline: Instant::now() - Duration::from_millis(1),
        };

        assert!(budget.remaining().is_none());
        assert!(budget.remaining().is_none());
    }

    #[test]
    fn periodic_recovery_runs_immediately_repeats_and_joins_on_stop() {
        let cycles = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&cycles);
        let runtime = ServiceRecoveryRuntimeHandle::spawn_with_cycle(
            Duration::from_millis(20),
            Arc::new(move || {
                observed.fetch_add(1, Ordering::AcqRel);
                Ok(StartupRecoveryReport::default())
            }),
        )
        .expect("spawn periodic recovery");

        let deadline = Instant::now() + Duration::from_secs(2);
        while cycles.load(Ordering::Acquire) < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(cycles.load(Ordering::Acquire) >= 2);

        runtime.request_stop();
        runtime.wait_stopped().expect("join recovery thread");
        let stopped_at = cycles.load(Ordering::Acquire);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(cycles.load(Ordering::Acquire), stopped_at);
    }

    #[test]
    fn fast_restart_periodic_pass_requeues_claim_that_expires_after_initial_pass() {
        let temp = tempfile::TempDir::new().expect("temporary catalog");
        let store =
            OperationalStore::open(temp.path().join("operational.db")).expect("operational store");
        let runtime_repository = store.runtime();
        let now_ms = unix_time_ms().expect("clock");
        let work_item_id = WorkItemId::new();
        runtime_repository
            .enqueue_operation_work(OperationWorkSpec::new(
                OperationRunId::new(),
                work_item_id.clone(),
                "tool_rescan",
                "service",
                "fast-restart-periodic-recovery",
                "{}",
                now_ms,
            ))
            .expect("enqueue work");
        let stale = runtime_repository
            .claim_next(ClaimRequest::new("old-worker", "old-boot", now_ms, 10_000))
            .expect("claim")
            .expect("due work");
        runtime_repository
            .start_claim(&stale, now_ms + 1)
            .expect("start old claim");

        let cycles = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&cycles);
        let cycle_store = store.clone();
        let periodic = ServiceRecoveryRuntimeHandle::spawn_with_cycle(
            Duration::from_millis(10),
            Arc::new(move || {
                let report =
                    reconcile_expired_work(&cycle_store, Arc::new(NoVerifiedRecoveryPoint))?;
                observed.fetch_add(1, Ordering::AcqRel);
                Ok(report)
            }),
        )
        .expect("spawn periodic recovery");

        let deadline = Instant::now() + Duration::from_secs(2);
        while cycles.load(Ordering::Acquire) < 1 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(cycles.load(Ordering::Acquire), 1);
        let expired_at_ms = unix_time_ms().expect("clock");
        runtime_repository
            .heartbeat_claim(&stale, expired_at_ms, expired_at_ms)
            .expect("expire the old claim after the initial recovery pass");

        let reclaimed = loop {
            if let Some(claim) = runtime_repository
                .claim_next(ClaimRequest::new(
                    "new-worker",
                    "new-boot",
                    unix_time_ms().expect("clock") + STARTUP_RETRY_DELAY_MS + 1,
                    1_000,
                ))
                .expect("claim after recovery")
            {
                break claim;
            }
            assert!(
                Instant::now() < deadline,
                "periodic recovery did not requeue"
            );
            std::thread::sleep(Duration::from_millis(10));
        };

        periodic.request_stop();
        periodic.wait_stopped().expect("join periodic recovery");
        assert!(cycles.load(Ordering::Acquire) >= 2);
        assert_eq!(reclaimed.work_item_id(), &work_item_id);
        assert_eq!(reclaimed.attempt_no(), 2);
    }
}
