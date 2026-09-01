use std::collections::BTreeMap;
use std::sync::Arc;

use ed25519_dalek::SigningKey;
use restoreplane_core::{ArtifactPartRole, ConsistencyGrade, Engine, PartSpec};
use restoreplane_pipeline::{
    repair_replica_v2, Destination, DestinationReaderV2, EncodingEvidenceV2, LocalDest,
    ManifestPartV2, NativeToolIdentityV2, PipelineCfg, ReaderStatusV2, RecoveryOriginV2,
    RepairSourceV2, ReplicaCommitmentV2, ReplicaEndpointV2, ReplicaRequirementV2, SetManifestV2,
    SignedSetManifestV2,
};
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repair_reuses_verified_encoded_objects_and_commits_the_target_manifest_last() {
    let temp = tempfile::tempdir().expect("temp directory");
    let source_root = temp.path().join("source");
    let target_root = temp.path().join("target");
    let (signed, payload) = repair_fixture();
    seed_source(&source_root, &signed, &payload);
    let source_local = Arc::new(LocalDest::new("source-reader", source_root));
    let target_local = Arc::new(LocalDest::new("target-writer", target_root.clone()));

    let outcome = repair_replica_v2(
        &signed,
        RepairSourceV2::new("local:source", source_local as Arc<dyn DestinationReaderV2>)
            .expect("source"),
        ReplicaEndpointV2::new(
            "local:target",
            target_local.clone() as Arc<dyn Destination>,
            target_local as Arc<dyn DestinationReaderV2>,
        )
        .expect("target"),
        &PipelineCfg::default(),
        &SigningKey::from_bytes(&[3_u8; 32]).verifying_key(),
    )
    .await
    .expect("valid repair request");

    assert!(outcome.result.is_ok());
    assert_eq!(outcome.payload_receipts.len(), 1);
    assert!(outcome.manifest_receipt.is_some());
    assert!(target_root
        .join(&signed.manifest.parts[0].object_key)
        .is_file());
    assert!(
        target_root
            .join(&signed.manifest.replicas[1].manifest_object_key)
            .is_file(),
        "the target set Manifest is the final commit marker"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corrupt_repair_source_is_rejected_before_any_target_object_is_committed() {
    let temp = tempfile::tempdir().expect("temp directory");
    let source_root = temp.path().join("source");
    let target_root = temp.path().join("target");
    let (signed, mut payload) = repair_fixture();
    payload[0] ^= 0xff;
    seed_source(&source_root, &signed, &payload);
    let source_local = Arc::new(LocalDest::new("source-reader", source_root));
    let target_local = Arc::new(LocalDest::new("target-writer", target_root.clone()));

    let outcome = repair_replica_v2(
        &signed,
        RepairSourceV2::new("local:source", source_local as Arc<dyn DestinationReaderV2>)
            .expect("source"),
        ReplicaEndpointV2::new(
            "local:target",
            target_local.clone() as Arc<dyn Destination>,
            target_local as Arc<dyn DestinationReaderV2>,
        )
        .expect("target"),
        &PipelineCfg::default(),
        &SigningKey::from_bytes(&[3_u8; 32]).verifying_key(),
    )
    .await
    .expect("repair produces a per-replica outcome");

    assert!(outcome.result.is_err());
    assert!(outcome.payload_receipts.is_empty());
    assert!(outcome.manifest_receipt.is_none());
    assert!(
        !target_root
            .join(&signed.manifest.parts[0].object_key)
            .exists(),
        "a corrupt source must abort the target writer before commit"
    );
}

fn repair_fixture() -> (SignedSetManifestV2, Vec<u8>) {
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
    let replicas = ["local:source", "local:target"]
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
        1_000,
        "signing-key-01",
        NativeToolIdentityV2::new("pg_dump", "17.4", "b".repeat(64)).expect("tool"),
        BTreeMap::new(),
        vec![part],
        replicas,
    )
    .expect("manifest");
    let signed = SignedSetManifestV2::create(manifest, &SigningKey::from_bytes(&[3_u8; 32]))
        .expect("signed");
    (signed, payload)
}

fn seed_source(root: &std::path::Path, signed: &SignedSetManifestV2, payload: &[u8]) {
    let part_path = root.join(&signed.manifest.parts[0].object_key);
    std::fs::create_dir_all(part_path.parent().expect("part parent")).expect("source parent");
    std::fs::write(part_path, payload).expect("source payload");
    let manifest_path = root.join(&signed.manifest.replicas[0].manifest_object_key);
    std::fs::write(manifest_path, signed.to_json().expect("signed JSON")).expect("source Manifest");
}

fn uuid(value: &str) -> Uuid {
    Uuid::parse_str(value).expect("fixture UUID")
}
