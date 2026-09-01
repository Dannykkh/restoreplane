use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use async_trait::async_trait;
use dbbackup_catalog::{
    prepare_catalog_rebuild_v2,
    rusqlite::{params, Connection},
    CatalogRebuildApplyModeV2, CatalogRebuildDispositionV2, CatalogRebuildRejectReasonV2,
    CatalogRebuildRunModeV2, CatalogRebuildSourceKindV2, CatalogRebuildWorkSpec, ClaimRequest,
    OperationWorkSpec, OperationalStore,
};
use dbbackup_core::{ArtifactPartRole, ConsistencyGrade, Engine, PartSpec};
use dbbackup_domain::{CatalogRebuildRunId, OperationRunId, WorkItemId};
use dbbackup_pipeline::{
    DestError, DestinationReaderV2, EncodingEvidenceV2, ManifestPartV2, NativeToolIdentityV2,
    ReaderObjectProbeV2, ReaderStatusV2, RecoveryOriginV2, ReplicaCommitmentV2,
    ReplicaRequirementV2, SetManifestV2, SignedSetManifestV2,
};
use ed25519_dalek::SigningKey;
use tempfile::TempDir;
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_dry_run_separates_accepted_rejected_and_unknown_key_before_atomic_replace() {
    let fixture = Fixture::new();
    let trusted_signing_key = SigningKey::from_bytes(&[41_u8; 32]);
    let unknown_signing_key = SigningKey::from_bytes(&[42_u8; 32]);
    let accepted = manifest_fixture(100, "trusted-key", &trusted_signing_key);
    let unknown = manifest_fixture(200, "unknown-key", &unknown_signing_key);
    let mut tampered = manifest_fixture(300, "trusted-key", &trusted_signing_key);
    tampered.manifest.source_display_name = "tampered-orders".to_owned();
    let corrupt_payload = manifest_fixture(400, "trusted-key", &trusted_signing_key);

    let reader = MemoryReader::default();
    reader.insert_fixture(&accepted, false);
    reader.insert_fixture(&unknown, false);
    reader.insert_fixture(&tampered, false);
    reader.insert_fixture(&corrupt_payload, true);
    let keys = vec![
        manifest_key(&accepted),
        manifest_key(&unknown),
        manifest_key(&tampered),
        manifest_key(&corrupt_payload),
    ];
    let trusted_keys = BTreeMap::from([(
        "trusted-key".to_owned(),
        trusted_signing_key.verifying_key(),
    )]);

    let plan = prepare_catalog_rebuild_v2(&reader, "local:vault-a", keys, &trusted_keys, 2_000)
        .await
        .expect("prepare rebuild dry-run");

    assert_eq!(plan.accepted_count(), 1);
    assert_eq!(plan.rejected_count(), 2);
    assert_eq!(plan.unknown_key_count(), 1);
    assert!(plan.candidates().iter().any(|candidate| {
        candidate.disposition() == CatalogRebuildDispositionV2::UnknownKey
            && candidate.signing_key_id() == Some("unknown-key")
    }));
    assert!(plan.candidates().iter().any(|candidate| {
        candidate.reject_reason() == Some(CatalogRebuildRejectReasonV2::SignatureInvalid)
    }));
    assert!(plan.candidates().iter().any(|candidate| {
        candidate.reject_reason() == Some(CatalogRebuildRejectReasonV2::PayloadMismatch)
    }));
    assert_eq!(
        fixture
            .store
            .catalog_projection()
            .recovery_point_count()
            .expect("dry-run count"),
        0,
        "dry-run must not mutate catalog projection"
    );

    let verification_run_id = Uuid::new_v4().to_string();
    Connection::open(&fixture.db_path)
        .expect("open verification control writer")
        .execute(
            "INSERT INTO verification_runs(
                 id, recovery_point_id, requested_level, achieved_level,
                 trigger_kind, state, evidence_json,
                 started_at_ms, completed_at_ms, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, 2, 2, 'manual', 'passed', ?3, 1500, 1510, 1400, 1510)",
            params![
                verification_run_id,
                accepted.manifest.recovery_point_id.to_string(),
                r#"{"nativeValidator":"pg_restore --list","result":"passed"}"#,
            ],
        )
        .expect("seed terminal local verification control truth");

    let before = fixture
        .store
        .catalog_projection()
        .control_state_probe()
        .expect("control probe before rebuild");
    let applied = fixture
        .store
        .catalog_projection()
        .apply_rebuild_v2(plan, CatalogRebuildApplyModeV2::Replace)
        .expect("apply accepted rebuild plan");
    let after = fixture
        .store
        .catalog_projection()
        .control_state_probe()
        .expect("control probe after rebuild");
    assert_eq!(applied, 1);
    assert_eq!(
        before, after,
        "catalog replace must not touch control state"
    );
    assert_eq!(
        fixture
            .store
            .catalog_projection()
            .recovery_point_count()
            .expect("applied recovery points"),
        1
    );
    let replayed_verification: (i64, String, i64, i64, Option<i64>, Option<i64>) =
        Connection::open(&fixture.db_path)
            .expect("open rebuilt verification reader")
            .query_row(
                "SELECT evidence.verify_level, evidence.result,
                        evidence.verified_at_ms, evidence.ingested_at_ms,
                        snapshot.highest_verified_level, snapshot.last_verified_at_ms
                 FROM catalog_verification_projection AS evidence
                 JOIN catalog_snapshot_projection AS snapshot
                   ON snapshot.recovery_point_id = evidence.recovery_point_id
                 WHERE evidence.origin_verification_run_id = ?1",
                [verification_run_id],
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
            .expect("terminal verification replayed during replace");
    assert_eq!(
        replayed_verification,
        (2, "passed".to_owned(), 1_510, 2_000, Some(2), Some(1_510))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replace_refuses_an_empty_accepted_set_and_noncanonical_or_wrong_destination_manifests() {
    let fixture = Fixture::new();
    let signing_key = SigningKey::from_bytes(&[51_u8; 32]);
    let signed = manifest_fixture(500, "trusted-key", &signing_key);
    let reader = MemoryReader::default();
    let key = manifest_key(&signed);
    reader.insert(
        key.clone(),
        serde_json::to_vec(&signed).expect("compact noncanonical envelope"),
    );
    let trusted_keys = BTreeMap::from([("trusted-key".to_owned(), signing_key.verifying_key())]);

    let plan = prepare_catalog_rebuild_v2(
        &reader,
        "local:another-vault",
        vec![key],
        &trusted_keys,
        3_000,
    )
    .await
    .expect("classify invalid source");
    assert_eq!(plan.accepted_count(), 0);
    assert_eq!(
        plan.candidates()[0].reject_reason(),
        Some(CatalogRebuildRejectReasonV2::NonCanonicalEnvelope)
    );

    let error = fixture
        .store
        .catalog_projection()
        .apply_rebuild_v2(plan, CatalogRebuildApplyModeV2::Replace)
        .expect_err("empty replace must never wipe catalog truth");
    assert_eq!(error.code(), "CATALOG_PROJECTION_INPUT_INVALID");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_rebuild_atomically_settles_work_projection_and_candidate_audit() {
    let fixture = Fixture::new();
    let trusted_signing_key = SigningKey::from_bytes(&[71_u8; 32]);
    let unknown_signing_key = SigningKey::from_bytes(&[72_u8; 32]);
    let accepted = manifest_fixture(600, "trusted-key", &trusted_signing_key);
    let unknown = manifest_fixture(700, "unknown-key", &unknown_signing_key);
    let mut tampered = manifest_fixture(800, "trusted-key", &trusted_signing_key);
    tampered.manifest.source_display_name = "tampered-orders".to_owned();
    let reader = MemoryReader::default();
    reader.insert_fixture(&accepted, false);
    reader.insert_fixture(&unknown, false);
    reader.insert_fixture(&tampered, false);
    let plan = prepare_catalog_rebuild_v2(
        &reader,
        "local:vault-a",
        vec![
            manifest_key(&accepted),
            manifest_key(&unknown),
            manifest_key(&tampered),
        ],
        &BTreeMap::from([(
            "trusted-key".to_owned(),
            trusted_signing_key.verifying_key(),
        )]),
        4_000,
    )
    .await
    .expect("prepare durable rebuild");
    let verification_run_id = Uuid::new_v4().to_string();
    Connection::open(&fixture.db_path)
        .expect("open durable verification control writer")
        .execute(
            "INSERT INTO verification_runs(
                 id, recovery_point_id, requested_level, achieved_level,
                 trigger_kind, state, evidence_json,
                 started_at_ms, completed_at_ms, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, 2, 2, 'manual', 'passed', '{}', 3900, 3910, 3800, 3910)",
            params![
                verification_run_id,
                accepted.manifest.recovery_point_id.to_string(),
            ],
        )
        .expect("seed durable terminal verification truth");

    let rebuild_run_id = CatalogRebuildRunId::new();
    let work_item_id = WorkItemId::new();
    let runtime = fixture.store.runtime();
    runtime
        .enqueue_catalog_rebuild(CatalogRebuildWorkSpec::new(
            rebuild_run_id.clone(),
            work_item_id.clone(),
            None,
            CatalogRebuildSourceKindV2::Local,
            CatalogRebuildRunModeV2::ReplaceProjection,
            4_000,
        ))
        .expect("enqueue catalog rebuild");
    let claim = runtime
        .claim_next(ClaimRequest::new(
            "rebuild-worker",
            "rebuild-boot",
            4_000,
            1_000,
        ))
        .expect("claim rebuild")
        .expect("due rebuild");
    runtime.start_claim(&claim, 4_001).expect("start rebuild");
    let applied = runtime
        .settle_catalog_rebuild_v2(
            &claim,
            plan,
            CatalogRebuildApplyModeV2::Replace,
            "local:vault-a",
            4_010,
        )
        .expect("settle durable rebuild");
    assert_eq!(applied, 1);

    let direct = Connection::open(&fixture.db_path).expect("open audit reader");
    let state: (String, i64, i64, String, String) = direct
        .query_row(
            "SELECT rebuild.status, rebuild.accepted_count,
                    rebuild.rejected_count, work.state, attempt.state
             FROM catalog_rebuild_runs AS rebuild
             JOIN work_items AS work
               ON work.catalog_rebuild_run_id = rebuild.id
             JOIN work_attempts AS attempt
               ON attempt.work_item_id = work.id
             WHERE rebuild.id = ?1",
            [rebuild_run_id.as_str()],
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
        .expect("query durable rebuild state");
    assert_eq!(
        state,
        (
            "succeeded".to_owned(),
            1,
            2,
            "succeeded".to_owned(),
            "succeeded".to_owned(),
        )
    );
    let audit: (i64, i64, i64) = direct
        .query_row(
            "SELECT
                 SUM(CASE WHEN result = 'accepted' THEN 1 ELSE 0 END),
                 SUM(CASE WHEN result = 'invalid_signature' THEN 1 ELSE 0 END),
                 SUM(CASE WHEN result = 'unknown_key' THEN 1 ELSE 0 END)
             FROM catalog_manifest_ingest
             WHERE catalog_rebuild_run_id = ?1",
            [rebuild_run_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("query candidate audit");
    assert_eq!(audit, (1, 1, 1));
    let replayed: (String, i64, Option<i64>, Option<i64>) = direct
        .query_row(
            "SELECT evidence.result, evidence.ingested_at_ms,
                    snapshot.highest_verified_level, snapshot.last_verified_at_ms
             FROM catalog_verification_projection AS evidence
             JOIN catalog_snapshot_projection AS snapshot
               ON snapshot.recovery_point_id = evidence.recovery_point_id
             WHERE evidence.origin_verification_run_id = ?1",
            [verification_run_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("runtime rebuild replayed verification truth");
    assert_eq!(replayed, ("passed".to_owned(), 4_010, Some(2), Some(3_910)));

    let stale = runtime
        .settle_catalog_rebuild_v2(
            &claim,
            prepare_catalog_rebuild_v2(
                &reader,
                "local:vault-a",
                vec![manifest_key(&accepted)],
                &BTreeMap::from([(
                    "trusted-key".to_owned(),
                    trusted_signing_key.verifying_key(),
                )]),
                4_011,
            )
            .await
            .expect("prepare stale replay"),
            CatalogRebuildApplyModeV2::Merge,
            "local:vault-a",
            4_012,
        )
        .expect_err("terminal claim replay must be fenced");
    assert_eq!(stale.code(), "WORK_ITEM_FENCE_STALE");
}

fn manifest_fixture(
    seed: u128,
    signing_key_id: &str,
    signing_key: &SigningKey,
) -> SignedSetManifestV2 {
    let recovery_point_id = uuid(seed);
    let occurrence_id = uuid(seed + 1);
    let backup_run_id = uuid(seed + 2);
    let target_id = uuid(seed + 3);
    let part_id = uuid(seed + 4);
    let origin = RecoveryOriginV2::new(
        recovery_point_id,
        format!("instance-{seed}"),
        occurrence_id,
        backup_run_id,
        target_id,
        format!("target:{seed}"),
        format!("orders-{seed}"),
    )
    .expect("recovery origin");
    let prefix = format!("v2/{occurrence_id}/{target_id}/{recovery_point_id}");
    let payload = payload_bytes(seed);
    let part_spec = PartSpec::new(
        part_id,
        ArtifactPartRole::DatabaseDump,
        "orders.dump",
        true,
        "custom",
    )
    .expect("part spec");
    let evidence = EncodingEvidenceV2::new(
        "a".repeat(64),
        blake3::hash(&payload).to_hex().to_string(),
        128,
        payload.len() as u64,
        "zstd",
        "chacha20poly1305-stream",
        1,
    )
    .expect("encoding evidence");
    let part = ManifestPartV2::encoded(
        part_spec,
        evidence,
        format!("{prefix}/parts/orders.dump.enc"),
    )
    .expect("manifest part");
    let replica = ReplicaCommitmentV2::new(
        "local:vault-a",
        "local",
        ReplicaRequirementV2::Required,
        ReaderStatusV2::Eligible,
        "none",
        format!("{prefix}/set-manifest.json"),
    )
    .expect("replica commitment");
    let manifest = SetManifestV2::new(
        origin,
        Engine::Postgresql,
        ConsistencyGrade::ApplicationConsistent,
        1_000,
        signing_key_id,
        NativeToolIdentityV2::new("pg_dump", "17.4", "f".repeat(64)).expect("tool identity"),
        BTreeMap::new(),
        vec![part],
        vec![replica],
    )
    .expect("set manifest");
    SignedSetManifestV2::create(manifest, signing_key).expect("signed set manifest")
}

fn manifest_key(signed: &SignedSetManifestV2) -> String {
    signed.manifest.replicas[0].manifest_object_key.clone()
}

fn payload_bytes(seed: u128) -> Vec<u8> {
    format!("encoded-payload-{seed}").into_bytes()
}

fn uuid(seed: u128) -> Uuid {
    Uuid::from_u128(seed)
}

#[derive(Default)]
struct MemoryReader {
    objects: Mutex<HashMap<String, Vec<u8>>>,
}

impl MemoryReader {
    fn insert(&self, key: String, bytes: Vec<u8>) {
        self.objects
            .lock()
            .expect("memory reader lock")
            .insert(key, bytes);
    }

    fn insert_fixture(&self, signed: &SignedSetManifestV2, corrupt_payload: bool) {
        let seed = signed.manifest.recovery_point_id.as_u128();
        let mut payload = payload_bytes(seed);
        if corrupt_payload {
            payload[0] ^= 0xff;
        }
        self.insert(signed.manifest.parts[0].object_key.clone(), payload);
        self.insert(
            manifest_key(signed),
            signed
                .to_json()
                .expect("canonical signed envelope")
                .into_bytes(),
        );
    }
}

#[async_trait]
impl DestinationReaderV2 for MemoryReader {
    async fn probe_object(&self, key: &str) -> Result<ReaderObjectProbeV2, DestError> {
        let bytes = self
            .objects
            .lock()
            .expect("memory reader lock")
            .get(key)
            .cloned()
            .ok_or_else(|| DestError::backend(key, "missing object"))?;
        Ok(ReaderObjectProbeV2::from_bytes(&bytes))
    }

    async fn read_bytes_bounded(&self, key: &str, max_bytes: u64) -> Result<Vec<u8>, DestError> {
        let bytes = self
            .objects
            .lock()
            .expect("memory reader lock")
            .get(key)
            .cloned()
            .ok_or_else(|| DestError::backend(key, "missing object"))?;
        if bytes.len() as u64 > max_bytes {
            return Err(DestError::ReadLimitExceeded {
                key: key.to_owned(),
                max_bytes,
            });
        }
        Ok(bytes)
    }
}

struct Fixture {
    _temp_dir: TempDir,
    db_path: std::path::PathBuf,
    store: OperationalStore,
}

impl Fixture {
    fn new() -> Self {
        let temp_dir = TempDir::new().expect("temp directory");
        let db_path = temp_dir.path().join("operational.db");
        let store = OperationalStore::open(db_path.clone()).expect("open operational store");
        store
            .runtime()
            .enqueue_operation_work(OperationWorkSpec::new(
                OperationRunId::new(),
                WorkItemId::new(),
                "tool_rescan",
                "service",
                "local",
                r#"{"inventory":"native-tools"}"#,
                100_000,
            ))
            .expect("seed nonempty control state");
        Self {
            _temp_dir: temp_dir,
            db_path,
            store,
        }
    }
}
