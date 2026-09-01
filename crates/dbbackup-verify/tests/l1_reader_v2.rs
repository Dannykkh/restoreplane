use std::collections::BTreeMap;
use std::sync::Arc;

use dbbackup_core::{ArtifactPartRole, ConsistencyGrade, Engine, PartSpec};
use dbbackup_pipeline::{
    publish_set_manifest_v2, run_artifact, BackupSource, Destination, DestinationReaderV2,
    EncodedPartSourceV2, EncodingEvidenceV2, LocalDest, ManifestPartV2, NativeToolIdentityV2,
    PipelineCfg, ReaderStatusV2, RecoveryOriginV2, ReplicaCommitmentV2, ReplicaEndpointV2,
    ReplicaRequirementV2, SetManifestV2, SignedSetManifestV2, StubSource,
};
use dbbackup_secrets::Dek;
use dbbackup_verify::{verify_l1_from_reader_v2, L1V2Error, L1VerifyLimitsV2, SingleDekProviderV2};
use ed25519_dalek::SigningKey;
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn l1_v2_reads_the_destination_and_proves_cipher_and_plaintext_round_trip() {
    let fixture = Fixture::publish().await;
    let report = verify_l1_from_reader_v2(
        fixture.reader.as_ref(),
        &fixture.manifest_key,
        "local:vault-a",
        &fixture.signing_key.verifying_key(),
        &SingleDekProviderV2::new(fixture.dek.clone(), 1),
        L1VerifyLimitsV2::new(8 * 1024 * 1024, 8 * 1024 * 1024).expect("verification limits"),
        2_000,
    )
    .await
    .expect("L1 v2 verification");

    assert_eq!(
        report.recovery_point_id(),
        fixture.signed.manifest.recovery_point_id
    );
    assert_eq!(report.destination_fingerprint(), "local:vault-a");
    assert_eq!(report.verified_part_count(), 1);
    assert_eq!(report.verified_at_ms(), 2_000);
    assert_eq!(
        report.parts()[0].plaintext_blake3(),
        fixture.signed.manifest.parts[0].plaintext_blake3
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn l1_v2_fails_closed_for_destination_corruption_or_the_wrong_dek() {
    let fixture = Fixture::publish().await;
    let payload_path = fixture
        .destination_root
        .join(&fixture.signed.manifest.parts[0].object_key);
    let mut corrupt = std::fs::read(&payload_path).expect("read destination payload");
    let midpoint = corrupt.len() / 2;
    corrupt[midpoint] ^= 0xff;
    std::fs::write(&payload_path, corrupt).expect("corrupt destination payload");

    let error = verify_l1_from_reader_v2(
        fixture.reader.as_ref(),
        &fixture.manifest_key,
        "local:vault-a",
        &fixture.signing_key.verifying_key(),
        &SingleDekProviderV2::new(fixture.dek.clone(), 1),
        L1VerifyLimitsV2::new(8 * 1024 * 1024, 8 * 1024 * 1024).expect("verification limits"),
        2_000,
    )
    .await
    .expect_err("ciphertext corruption must fail L1");
    assert_eq!(error.code(), L1V2Error::CiphertextMismatch.code());

    let fresh = Fixture::publish().await;
    let error = verify_l1_from_reader_v2(
        fresh.reader.as_ref(),
        &fresh.manifest_key,
        "local:vault-a",
        &fresh.signing_key.verifying_key(),
        &SingleDekProviderV2::new(Dek::generate(), 1),
        L1VerifyLimitsV2::new(8 * 1024 * 1024, 8 * 1024 * 1024).expect("verification limits"),
        2_000,
    )
    .await
    .expect_err("wrong DEK must fail L1");
    assert_eq!(error.code(), L1V2Error::DecryptFailed.code());
}

struct Fixture {
    _temp_dir: tempfile::TempDir,
    destination_root: std::path::PathBuf,
    reader: Arc<LocalDest>,
    signed: SignedSetManifestV2,
    manifest_key: String,
    signing_key: SigningKey,
    dek: Dek,
}

impl Fixture {
    async fn publish() -> Self {
        let temp_dir = tempfile::tempdir().expect("temp directory");
        let spool_root = temp_dir.path().join("spool");
        let destination_root = temp_dir.path().join("destination");
        std::fs::create_dir_all(&spool_root).expect("create spool root");
        let plaintext = (0..256 * 1024)
            .map(|index| ((index * 31 + 7) % 256) as u8)
            .collect::<Vec<_>>();
        let source = StubSource::ok("job-v2", "orders", plaintext.clone());
        let plan_part = source.plan().await.expect("source plan").db_parts.remove(0);
        let dek = Dek::generate();
        let spool_path = spool_root.join("orders.enc");
        let _commit = run_artifact(
            source.spawn(&plan_part).expect("spawn source"),
            spool_path.clone(),
            dek.clone(),
            &PipelineCfg::default(),
        )
        .await
        .expect("encode payload");
        let ciphertext = std::fs::read(&spool_path).expect("read encoded payload");

        let recovery_point_id = Uuid::new_v4();
        let occurrence_id = Uuid::new_v4();
        let backup_run_id = Uuid::new_v4();
        let target_id = Uuid::new_v4();
        let part_id = Uuid::new_v4();
        let prefix = format!("v2/{occurrence_id}/{target_id}/{recovery_point_id}");
        let part_spec = PartSpec::new(
            part_id,
            ArtifactPartRole::DatabaseDump,
            "orders.dump",
            true,
            "custom",
        )
        .expect("part spec");
        let evidence = EncodingEvidenceV2::new(
            blake3::hash(&plaintext).to_hex().to_string(),
            blake3::hash(&ciphertext).to_hex().to_string(),
            plaintext.len() as u64,
            ciphertext.len() as u64,
            "zstd",
            "chacha20poly1305-stream",
            1,
        )
        .expect("encoding evidence");
        let manifest_part = ManifestPartV2::encoded(
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
        .expect("replica");
        let manifest = SetManifestV2::new(
            RecoveryOriginV2::new(
                recovery_point_id,
                "instance-a",
                occurrence_id,
                backup_run_id,
                target_id,
                "target:orders",
                "orders",
            )
            .expect("origin"),
            Engine::Postgresql,
            ConsistencyGrade::ApplicationConsistent,
            1_000,
            "l1-signing-key",
            NativeToolIdentityV2::new("pg_dump", "17.4", "f".repeat(64)).expect("tool identity"),
            BTreeMap::new(),
            vec![manifest_part],
            vec![replica],
        )
        .expect("manifest");
        let signing_key = SigningKey::from_bytes(&[61_u8; 32]);
        let signed = SignedSetManifestV2::create(manifest, &signing_key).expect("signed manifest");
        let reader = Arc::new(LocalDest::new("local-vault", destination_root.clone()));
        let endpoint = ReplicaEndpointV2::new(
            "local:vault-a",
            reader.clone() as Arc<dyn Destination>,
            reader.clone() as Arc<dyn DestinationReaderV2>,
        )
        .expect("replica endpoint");
        let outcomes = publish_set_manifest_v2(
            &signed,
            vec![EncodedPartSourceV2::new(part_id, spool_path).expect("encoded source")],
            vec![endpoint],
            &PipelineCfg::default(),
            &signing_key.verifying_key(),
        )
        .await
        .expect("publish v2 replica");
        assert!(outcomes[0].result.is_ok());
        let manifest_key = signed.manifest.replicas[0].manifest_object_key.clone();

        Self {
            _temp_dir: temp_dir,
            destination_root,
            reader,
            signed,
            manifest_key,
            signing_key,
            dek,
        }
    }
}
