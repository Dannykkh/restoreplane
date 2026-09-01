use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ed25519_dalek::{SigningKey, VerifyingKey};
use restoreplane_catalog::{
    rusqlite::Connection, ClaimRequest, CompleteRecoveryPointSpec, OperationalStore,
    ReplicaRepairSecretRefsV2, ReplicaRepairWorkSpec,
};
use restoreplane_core::{ArtifactPartRole, ConsistencyGrade, Engine, PartSpec};
use restoreplane_domain::{ProtectionCompliance, RecoveryPointId, SnapshotReplicaDurability};
use restoreplane_pipeline::{
    Destination, DestinationReaderV2, EncodingEvidenceV2, LocalDest, ManifestPartV2,
    NativeToolIdentityV2, PipelineCfg, ReaderStatusV2, RecoveryOriginV2, RepairSourceV2,
    ReplicaCommitmentV2, ReplicaEndpointV2, ReplicaPublishError, ReplicaPublishOutcomeV2,
    ReplicaRequirementV2, SetManifestV2, SignedSetManifestV2, UploadReceipt,
};
use restoreplane_runtime::{
    ReplicaRepairResolutionErrorV2, ReplicaRepairResolverV2, ReplicaRepairWorkerOutcomeV2,
    ReplicaRepairWorkerV2, ResolvedReplicaRepairV2,
    RetentionDeleteCancellationToken as ReplicaRepairCancellationTokenV2, RuntimeClock,
};
use tempfile::TempDir;
use uuid::Uuid;

#[tokio::test]
async fn claimed_repair_reuses_source_objects_and_atomically_restores_compliance() {
    let fixture = Fixture::new();
    let upsert = fixture
        .store
        .runtime()
        .enqueue_replica_repair(ReplicaRepairWorkSpec::new(
            fixture.recovery_point_id.clone(),
            &fixture.source_fingerprint,
            &fixture.target_fingerprint,
            ReplicaRepairSecretRefsV2::local(),
            500,
        ))
        .expect("enqueue repair");
    let claim = fixture
        .store
        .runtime()
        .claim_next(ClaimRequest::new("repair-worker", "boot-a", 500, 1_000))
        .expect("claim repair")
        .expect("repair work");
    assert_eq!(claim.work_item_id(), upsert.work_item_id());
    let clock = Arc::new(ManualClock::new(501));
    let worker = ReplicaRepairWorkerV2::new(
        fixture.store.runtime(),
        Arc::new(LocalRepairResolver::from_fixture(&fixture)),
        clock,
        1_000,
        10,
    )
    .expect("repair worker");

    let outcome = worker.execute(&claim).await.expect("execute repair");
    assert_eq!(
        outcome,
        ReplicaRepairWorkerOutcomeV2::Terminal(
            restoreplane_catalog::ClaimTerminalOutcome::Succeeded
        )
    );
    assert!(fixture
        .target_root
        .join(&fixture.signed.manifest.parts[0].object_key)
        .is_file());
    assert!(fixture
        .target_root
        .join(
            &fixture
                .signed
                .manifest
                .replicas
                .iter()
                .find(|replica| { replica.destination_fingerprint == fixture.target_fingerprint })
                .expect("target replica")
                .manifest_object_key
        )
        .is_file());

    let snapshot = fixture
        .store
        .catalog_projection()
        .snapshot(&fixture.recovery_point_id)
        .expect("repaired snapshot");
    assert_eq!(
        snapshot.replica_durability(),
        SnapshotReplicaDurability::Healthy
    );
    assert_eq!(
        snapshot.protection_compliance(),
        ProtectionCompliance::Compliant
    );
    let direct = Connection::open(&fixture.db_path).expect("open repair truth");
    let target: (String, String, i64) = direct
        .query_row(
            "SELECT replicas.state, replicas.reader_status,
                    COUNT(objects.id)
             FROM catalog_artifact_replicas AS replicas
             LEFT JOIN catalog_stored_objects AS objects
               ON objects.artifact_replica_id = replicas.id
              AND objects.state = 'present'
             WHERE replicas.recovery_point_id = ?1
               AND replicas.destination_fingerprint = ?2
             GROUP BY replicas.id",
            [
                fixture.recovery_point_id.as_str(),
                fixture.target_fingerprint.as_str(),
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("target projection");
    assert_eq!(target, ("committed".to_owned(), "eligible".to_owned(), 2));
}

#[tokio::test]
async fn unavailable_source_exhausts_bounded_retries_without_mutating_snapshot() {
    let fixture = Fixture::new();
    let upsert = fixture
        .store
        .runtime()
        .enqueue_replica_repair(ReplicaRepairWorkSpec::new(
            fixture.recovery_point_id.clone(),
            &fixture.source_fingerprint,
            &fixture.target_fingerprint,
            ReplicaRepairSecretRefsV2::local(),
            500,
        ))
        .expect("enqueue repair");
    let clock = Arc::new(ManualClock::new(501));
    let worker = ReplicaRepairWorkerV2::new(
        fixture.store.runtime(),
        Arc::new(UnavailableResolver),
        clock.clone(),
        1_000,
        10,
    )
    .expect("repair worker");
    let mut claim_at = 500;
    let mut outcomes = Vec::new();
    for attempt in 0..3 {
        clock.set(claim_at + 1);
        let claim = fixture
            .store
            .runtime()
            .claim_next(ClaimRequest::new(
                "repair-worker",
                format!("boot-{attempt}"),
                claim_at,
                1_000,
            ))
            .expect("claim repair")
            .expect("repair retry work");
        let outcome = worker.execute(&claim).await.expect("settle retry");
        outcomes.push(outcome);
        claim_at += 12;
    }
    assert_eq!(
        outcomes,
        vec![
            ReplicaRepairWorkerOutcomeV2::Retry(restoreplane_catalog::RetryDisposition::Requeued),
            ReplicaRepairWorkerOutcomeV2::Retry(restoreplane_catalog::RetryDisposition::Requeued),
            ReplicaRepairWorkerOutcomeV2::Retry(restoreplane_catalog::RetryDisposition::Exhausted),
        ]
    );

    let snapshot = fixture
        .store
        .catalog_projection()
        .snapshot(&fixture.recovery_point_id)
        .expect("unchanged snapshot");
    assert_eq!(
        snapshot.replica_durability(),
        SnapshotReplicaDurability::Degraded
    );
    assert_eq!(
        snapshot.protection_compliance(),
        ProtectionCompliance::NonCompliant
    );
    let direct = Connection::open(&fixture.db_path).expect("open retry truth");
    let states: (String, String, i64) = direct
        .query_row(
            "SELECT work_items.state, operation_runs.state,
                    (SELECT COUNT(*) FROM catalog_stored_objects AS objects
                     JOIN catalog_artifact_replicas AS replicas
                       ON replicas.id = objects.artifact_replica_id
                     WHERE replicas.destination_fingerprint = ?2)
             FROM work_items
             JOIN operation_runs ON operation_runs.id = work_items.operation_run_id
             WHERE work_items.id = ?1",
            [upsert.work_item_id().as_str(), &fixture.target_fingerprint],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("repair runtime states");
    assert_eq!(
        states,
        ("failed_terminal".to_owned(), "failed".to_owned(), 0)
    );
    assert!(!fixture.target_root.exists());
}

#[tokio::test]
async fn service_cancellation_interrupts_resolution_and_requeues_with_stable_error() {
    let fixture = Fixture::new();
    let upsert = fixture
        .store
        .runtime()
        .enqueue_replica_repair(ReplicaRepairWorkSpec::new(
            fixture.recovery_point_id.clone(),
            &fixture.source_fingerprint,
            &fixture.target_fingerprint,
            ReplicaRepairSecretRefsV2::local(),
            500,
        ))
        .expect("enqueue repair");
    let claim = fixture
        .store
        .runtime()
        .claim_next(
            ClaimRequest::new("repair-worker", "boot-cancel", 500, 1_000)
                .for_operation_kinds(["replica_repair"]),
        )
        .expect("claim repair")
        .expect("repair work");
    let cancellation = ReplicaRepairCancellationTokenV2::default();
    let trigger = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(25)).await;
        trigger.cancel();
    });
    let worker = ReplicaRepairWorkerV2::new(
        fixture.store.runtime(),
        Arc::new(HangingResolver),
        Arc::new(ManualClock::new(501)),
        1_000,
        10,
    )
    .expect("repair worker");

    let outcome = tokio::time::timeout(
        Duration::from_secs(1),
        worker.execute_with_control(
            &claim,
            &cancellation,
            tokio::time::Instant::now() + Duration::from_secs(30),
        ),
    )
    .await
    .expect("service cancellation must bound resolver wait")
    .expect("settle cancelled repair");

    assert_eq!(
        outcome,
        ReplicaRepairWorkerOutcomeV2::Retry(restoreplane_catalog::RetryDisposition::Requeued)
    );
    assert_eq!(
        settled_error_code(&fixture.db_path, upsert.work_item_id().as_str()),
        "REPLICA_REPAIR_SERVICE_CANCELLED"
    );
}

#[tokio::test]
async fn resolver_and_remote_pipeline_share_one_absolute_deadline() {
    let fixture = Fixture::new();
    let upsert = fixture
        .store
        .runtime()
        .enqueue_replica_repair(ReplicaRepairWorkSpec::new(
            fixture.recovery_point_id.clone(),
            &fixture.source_fingerprint,
            &fixture.target_fingerprint,
            ReplicaRepairSecretRefsV2::local(),
            500,
        ))
        .expect("enqueue repair");
    let claim = fixture
        .store
        .runtime()
        .claim_next(
            ClaimRequest::new("repair-worker", "boot-deadline", 500, 1_000)
                .for_operation_kinds(["replica_repair"]),
        )
        .expect("claim repair")
        .expect("repair work");
    let resolver = DelayedPipelineResolver {
        signed: fixture.signed.clone(),
        verifying_key: fixture.verifying_key,
        source_fingerprint: fixture.source_fingerprint.clone(),
        target_fingerprint: fixture.target_fingerprint.clone(),
        target_root: fixture.target_root.clone(),
        payload: b"already-encoded-database".to_vec(),
        delay: Duration::from_millis(40),
    };
    let worker = ReplicaRepairWorkerV2::new(
        fixture.store.runtime(),
        Arc::new(resolver),
        Arc::new(ManualClock::new(501)),
        1_000,
        10,
    )
    .expect("repair worker");
    let started = tokio::time::Instant::now();

    let outcome = tokio::time::timeout(
        Duration::from_secs(1),
        worker.execute_with_control(
            &claim,
            &ReplicaRepairCancellationTokenV2::default(),
            started + Duration::from_millis(65),
        ),
    )
    .await
    .expect("absolute repair deadline must bound remote work")
    .expect("settle deadline repair");

    assert_eq!(
        outcome,
        ReplicaRepairWorkerOutcomeV2::Retry(restoreplane_catalog::RetryDisposition::Requeued)
    );
    assert_eq!(
        settled_error_code(&fixture.db_path, upsert.work_item_id().as_str()),
        "REPLICA_REPAIR_DEADLINE_EXCEEDED"
    );
    assert!(!fixture.target_root.exists());
}

struct LocalRepairResolver {
    signed: SignedSetManifestV2,
    verifying_key: VerifyingKey,
    source_fingerprint: String,
    target_fingerprint: String,
    source_root: std::path::PathBuf,
    target_root: std::path::PathBuf,
}

impl LocalRepairResolver {
    fn from_fixture(fixture: &Fixture) -> Self {
        Self {
            signed: fixture.signed.clone(),
            verifying_key: fixture.verifying_key,
            source_fingerprint: fixture.source_fingerprint.clone(),
            target_fingerprint: fixture.target_fingerprint.clone(),
            source_root: fixture.source_root.clone(),
            target_root: fixture.target_root.clone(),
        }
    }
}

#[async_trait]
impl ReplicaRepairResolverV2 for LocalRepairResolver {
    async fn resolve(
        &self,
        request: &restoreplane_catalog::ClaimedReplicaRepairV2,
    ) -> Result<ResolvedReplicaRepairV2, ReplicaRepairResolutionErrorV2> {
        assert_eq!(
            request.recovery_point_id().as_str(),
            self.signed.manifest.recovery_point_id.to_string()
        );
        assert_eq!(
            request.source_destination_fingerprint(),
            self.source_fingerprint
        );
        assert_eq!(
            request.target_destination_fingerprint(),
            self.target_fingerprint
        );
        let source = Arc::new(LocalDest::new("repair-source", self.source_root.clone()));
        let target = Arc::new(LocalDest::new("repair-target", self.target_root.clone()));
        ResolvedReplicaRepairV2::new(
            self.signed.clone(),
            RepairSourceV2::new(
                self.source_fingerprint.clone(),
                source as Arc<dyn DestinationReaderV2>,
            )
            .expect("repair source"),
            ReplicaEndpointV2::new(
                self.target_fingerprint.clone(),
                target.clone() as Arc<dyn Destination>,
                target as Arc<dyn DestinationReaderV2>,
            )
            .expect("repair target"),
            PipelineCfg::default(),
            self.verifying_key,
        )
        .map_err(|error| {
            ReplicaRepairResolutionErrorV2::terminal(error.code()).expect("resolution error")
        })
    }
}

struct UnavailableResolver;

#[async_trait]
impl ReplicaRepairResolverV2 for UnavailableResolver {
    async fn resolve(
        &self,
        _request: &restoreplane_catalog::ClaimedReplicaRepairV2,
    ) -> Result<ResolvedReplicaRepairV2, ReplicaRepairResolutionErrorV2> {
        Err(
            ReplicaRepairResolutionErrorV2::retryable("REPLICA_REPAIR_SOURCE_UNAVAILABLE")
                .expect("retryable resolution"),
        )
    }
}

struct HangingResolver;

#[async_trait]
impl ReplicaRepairResolverV2 for HangingResolver {
    async fn resolve(
        &self,
        _request: &restoreplane_catalog::ClaimedReplicaRepairV2,
    ) -> Result<ResolvedReplicaRepairV2, ReplicaRepairResolutionErrorV2> {
        std::future::pending().await
    }
}

struct DelayedPipelineResolver {
    signed: SignedSetManifestV2,
    verifying_key: VerifyingKey,
    source_fingerprint: String,
    target_fingerprint: String,
    target_root: std::path::PathBuf,
    payload: Vec<u8>,
    delay: Duration,
}

#[async_trait]
impl ReplicaRepairResolverV2 for DelayedPipelineResolver {
    async fn resolve(
        &self,
        _request: &restoreplane_catalog::ClaimedReplicaRepairV2,
    ) -> Result<ResolvedReplicaRepairV2, ReplicaRepairResolutionErrorV2> {
        tokio::time::sleep(self.delay).await;
        let source: Arc<dyn DestinationReaderV2> = Arc::new(DelayedReader {
            signed_json: self.signed.to_json().expect("signed JSON").into_bytes(),
            manifest_key: self
                .signed
                .manifest
                .replicas
                .iter()
                .find(|replica| replica.destination_fingerprint == self.source_fingerprint)
                .expect("source replica")
                .manifest_object_key
                .clone(),
            payload: self.payload.clone(),
            delay: self.delay,
        });
        let target = Arc::new(LocalDest::new("repair-target", self.target_root.clone()));
        ResolvedReplicaRepairV2::new(
            self.signed.clone(),
            RepairSourceV2::new(self.source_fingerprint.clone(), source).expect("repair source"),
            ReplicaEndpointV2::new(
                self.target_fingerprint.clone(),
                target.clone() as Arc<dyn Destination>,
                target as Arc<dyn DestinationReaderV2>,
            )
            .expect("repair target"),
            PipelineCfg::default(),
            self.verifying_key,
        )
        .map_err(|error| {
            ReplicaRepairResolutionErrorV2::terminal(error.code()).expect("resolution error")
        })
    }
}

struct DelayedReader {
    signed_json: Vec<u8>,
    manifest_key: String,
    payload: Vec<u8>,
    delay: Duration,
}

#[async_trait]
impl DestinationReaderV2 for DelayedReader {
    async fn probe_object(
        &self,
        key: &str,
    ) -> Result<restoreplane_pipeline::ReaderObjectProbeV2, restoreplane_pipeline::DestError> {
        let bytes = self.read_bytes_bounded(key, u64::MAX).await?;
        Ok(restoreplane_pipeline::ReaderObjectProbeV2::from_bytes(
            &bytes,
        ))
    }

    async fn read_bytes_bounded(
        &self,
        key: &str,
        _max_bytes: u64,
    ) -> Result<Vec<u8>, restoreplane_pipeline::DestError> {
        tokio::time::sleep(self.delay).await;
        Ok(if key == self.manifest_key {
            self.signed_json.clone()
        } else {
            self.payload.clone()
        })
    }
}

struct ManualClock {
    next: AtomicI64,
}

impl ManualClock {
    const fn new(first: i64) -> Self {
        Self {
            next: AtomicI64::new(first),
        }
    }

    fn set(&self, value: i64) {
        self.next.store(value, Ordering::SeqCst);
    }
}

impl RuntimeClock for ManualClock {
    fn now_ms(&self) -> i64 {
        self.next.fetch_add(1, Ordering::SeqCst)
    }
}

struct Fixture {
    _temp_dir: TempDir,
    db_path: std::path::PathBuf,
    store: OperationalStore,
    recovery_point_id: RecoveryPointId,
    source_fingerprint: String,
    target_fingerprint: String,
    source_root: std::path::PathBuf,
    target_root: std::path::PathBuf,
    signed: SignedSetManifestV2,
    verifying_key: VerifyingKey,
}

impl Fixture {
    fn new() -> Self {
        let temp_dir = TempDir::new().expect("temp directory");
        let db_path = temp_dir.path().join("operational.db");
        let source_root = temp_dir.path().join("source");
        let target_root = temp_dir.path().join("target");
        let store = OperationalStore::open(db_path.clone()).expect("operational store");
        let source_fingerprint = "local:repair-source".to_owned();
        let target_fingerprint = "local:repair-target".to_owned();
        let (signed, payload, signing_key) =
            repair_fixture(&source_fingerprint, &target_fingerprint);
        let verifying_key = signing_key.verifying_key();
        let recovery_point_id =
            RecoveryPointId::from_str(&signed.manifest.recovery_point_id.to_string())
                .expect("recovery point ID");
        seed_source(&source_root, &signed, &payload, &source_fingerprint);
        let signed_json = signed.to_json().expect("signed JSON");
        let source_outcome = ReplicaPublishOutcomeV2 {
            destination_fingerprint: source_fingerprint.clone(),
            requirement: ReplicaRequirementV2::Required,
            reader_status: ReaderStatusV2::Eligible,
            payload_receipts: vec![UploadReceipt {
                key: signed.manifest.parts[0].object_key.clone(),
                bytes: signed.manifest.parts[0].ciphertext_bytes,
                version: None,
            }],
            manifest_receipt: Some(UploadReceipt {
                key: signed
                    .manifest
                    .replicas
                    .iter()
                    .find(|replica| replica.destination_fingerprint == source_fingerprint)
                    .expect("source replica")
                    .manifest_object_key
                    .clone(),
                bytes: u64::try_from(signed_json.len()).expect("manifest size"),
                version: None,
            }),
            attempts: 2,
            result: Ok(()),
        };
        let target_outcome = ReplicaPublishOutcomeV2 {
            destination_fingerprint: target_fingerprint.clone(),
            requirement: ReplicaRequirementV2::Required,
            reader_status: ReaderStatusV2::Unavailable,
            payload_receipts: Vec::new(),
            manifest_receipt: None,
            attempts: 1,
            result: Err(ReplicaPublishError::ReaderChecksumMismatch),
        };
        let spec = CompleteRecoveryPointSpec::from_verified_publish(
            &signed,
            &[source_outcome, target_outcome],
            &verifying_key,
            100,
        )
        .expect("catalog projection spec");
        store
            .catalog_projection()
            .ingest_complete(spec)
            .expect("ingest degraded snapshot");
        let snapshot = store
            .catalog_projection()
            .snapshot(&recovery_point_id)
            .expect("initial snapshot");
        assert_eq!(
            snapshot.replica_durability(),
            SnapshotReplicaDurability::Degraded
        );
        assert_eq!(
            snapshot.protection_compliance(),
            ProtectionCompliance::NonCompliant
        );
        Self {
            _temp_dir: temp_dir,
            db_path,
            store,
            recovery_point_id,
            source_fingerprint,
            target_fingerprint,
            source_root,
            target_root,
            signed,
            verifying_key,
        }
    }
}

fn repair_fixture(
    source_fingerprint: &str,
    target_fingerprint: &str,
) -> (SignedSetManifestV2, Vec<u8>, SigningKey) {
    let recovery_point_id = uuid("11111111-1111-4111-8111-111111111111");
    let occurrence_id = uuid("22222222-2222-4222-8222-222222222222");
    let target_id = uuid("33333333-3333-4333-8333-333333333333");
    let prefix = format!("v2/{occurrence_id}/{target_id}/{recovery_point_id}");
    let payload = b"already-encoded-database".to_vec();
    let part = ManifestPartV2::encoded(
        PartSpec::new(
            uuid("44444444-4444-4444-8444-444444444444"),
            ArtifactPartRole::DatabaseDump,
            "orders.dump",
            true,
            "custom",
        )
        .expect("part"),
        EncodingEvidenceV2::new(
            "a".repeat(64),
            blake3::hash(&payload).to_hex().to_string(),
            128,
            u64::try_from(payload.len()).expect("payload size"),
            "zstd",
            "chacha20poly1305-stream",
            1,
        )
        .expect("evidence"),
        format!("{prefix}/parts/orders.dump.enc"),
    )
    .expect("encoded part");
    let replicas = [source_fingerprint, target_fingerprint]
        .into_iter()
        .map(|fingerprint| {
            ReplicaCommitmentV2::new(
                fingerprint,
                "local",
                ReplicaRequirementV2::Required,
                ReaderStatusV2::Eligible,
                "none",
                format!("{prefix}/set-manifest.json"),
            )
            .expect("replica")
        })
        .collect();
    let manifest = SetManifestV2::new(
        RecoveryOriginV2::new(
            recovery_point_id,
            "instance-a",
            occurrence_id,
            uuid("55555555-5555-4555-8555-555555555555"),
            target_id,
            "oid:100",
            "orders",
        )
        .expect("origin"),
        Engine::Postgresql,
        ConsistencyGrade::ApplicationConsistent,
        100,
        "signing-key-01",
        NativeToolIdentityV2::new("pg_dump", "17.4", "b".repeat(64)).expect("tool"),
        BTreeMap::new(),
        vec![part],
        replicas,
    )
    .expect("manifest");
    let signing_key = SigningKey::from_bytes(&[3_u8; 32]);
    let signed = SignedSetManifestV2::create(manifest, &signing_key).expect("signed");
    (signed, payload, signing_key)
}

fn seed_source(
    root: &std::path::Path,
    signed: &SignedSetManifestV2,
    payload: &[u8],
    source_fingerprint: &str,
) {
    let part_path = root.join(&signed.manifest.parts[0].object_key);
    std::fs::create_dir_all(part_path.parent().expect("part parent")).expect("source parent");
    std::fs::write(part_path, payload).expect("source payload");
    let manifest_path = root.join(
        &signed
            .manifest
            .replicas
            .iter()
            .find(|replica| replica.destination_fingerprint == source_fingerprint)
            .expect("source replica")
            .manifest_object_key,
    );
    std::fs::write(manifest_path, signed.to_json().expect("signed JSON")).expect("source Manifest");
}

fn settled_error_code(database_path: &std::path::Path, work_item_id: &str) -> String {
    let connection = Connection::open(database_path).expect("open repair ledger");
    connection
        .query_row(
            "SELECT error_code
             FROM work_attempts
             WHERE work_item_id = ?1
             ORDER BY attempt_no DESC
             LIMIT 1",
            [work_item_id],
            |row| row.get(0),
        )
        .expect("repair attempt error code")
}

fn uuid(value: &str) -> Uuid {
    Uuid::parse_str(value).expect("fixture UUID")
}
