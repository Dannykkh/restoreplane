use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use dbbackup_core::{ArtifactPartRole, ArtifactSpec, ConsistencyGrade, Engine, PartSpec};
use dbbackup_pipeline::{
    publish_set_manifest_v2, Destination, DestinationReaderV2, EncodedPartSourceV2,
    EncodingEvidenceV2, LocalDest, ManifestPartV2, NativeToolIdentityV2, PipelineCfg,
    ReaderObjectProbeV2, ReaderStatusV2, RecoveryOriginV2, ReplicaCommitmentV2, ReplicaEndpointV2,
    ReplicaPublishError, ReplicaRequirementV2, SetManifestV2, SignedSetManifestV2,
};
use ed25519_dalek::SigningKey;
use uuid::Uuid;

#[test]
fn provider_neutral_artifact_contract_requires_a_database_part_and_unique_identity() {
    let globals = PartSpec::new(
        uuid("55555555-5555-4555-8555-555555555555"),
        ArtifactPartRole::Globals,
        "globals.sql",
        true,
        "plain",
    )
    .expect("valid globals part");
    let missing_database = ArtifactSpec::new(
        ConsistencyGrade::ApplicationConsistent,
        vec![globals.clone()],
    )
    .expect_err("a recovery point requires a database dump");
    assert_eq!(missing_database.code(), "ARTIFACT_DATABASE_PART_REQUIRED");

    let database = PartSpec::new(
        uuid("66666666-6666-4666-8666-666666666666"),
        ArtifactPartRole::DatabaseDump,
        "orders.dump",
        true,
        "custom",
    )
    .expect("valid database part");
    let duplicate_name = PartSpec::new(
        uuid("77777777-7777-4777-8777-777777777777"),
        ArtifactPartRole::Metadata,
        "orders.dump",
        false,
        "json",
    )
    .expect("valid metadata part");
    let duplicate = ArtifactSpec::new(
        ConsistencyGrade::ApplicationConsistent,
        vec![database, duplicate_name],
    )
    .expect_err("logical names must be unique");
    assert_eq!(duplicate.code(), "ARTIFACT_PART_IDENTITY_DUPLICATE");
}

#[test]
fn manifest_v2_has_a_stable_canonical_vector_and_signature_covers_every_restore_fact() {
    let manifest = fixture_manifest();
    let canonical = String::from_utf8(manifest.canonical_bytes().expect("canonical bytes"))
        .expect("canonical UTF-8");
    assert_eq!(canonical, expected_canonical_json());

    let signing_key = SigningKey::from_bytes(&[7_u8; 32]);
    let signed =
        SignedSetManifestV2::create(manifest.clone(), &signing_key).expect("sign manifest");
    signed
        .verify(&signing_key.verifying_key())
        .expect("verify manifest");

    let mut tampered = signed.clone();
    tampered.manifest.parts[0].ciphertext_bytes += 1;
    let error = tampered
        .verify(&signing_key.verifying_key())
        .expect_err("part mutation must invalidate the signature");
    assert_eq!(error.code(), "MANIFEST_V2_SIGNATURE_INVALID");

    let mut destination_tampered = signed;
    destination_tampered.manifest.replicas[1].requirement = ReplicaRequirementV2::Required;
    let error = destination_tampered
        .verify(&signing_key.verifying_key())
        .expect_err("destination role mutation must invalidate the signature");
    assert_eq!(error.code(), "MANIFEST_V2_SIGNATURE_INVALID");
}

#[test]
fn manifest_v2_rejects_unknown_fields_versions_and_non_deterministic_object_prefixes() {
    let manifest = fixture_manifest();
    let mut unknown = serde_json::to_value(&manifest).expect("manifest JSON");
    unknown["unexpected"] = serde_json::json!("must fail closed");
    assert!(
        serde_json::from_value::<SetManifestV2>(unknown).is_err(),
        "unknown fields must not be silently accepted"
    );

    let mut future = manifest.clone();
    future.schema_version = 3;
    let error = future
        .validate()
        .expect_err("unsupported schema version must fail closed");
    assert_eq!(error.code(), "MANIFEST_V2_SCHEMA_UNSUPPORTED");

    let globals = fixture_part(
        "55555555-5555-4555-8555-555555555555",
        ArtifactPartRole::Globals,
        "globals.sql",
        "v2/wrong-prefix/globals.sql.enc",
        "b",
        "c",
    );
    let database = fixture_part(
        "66666666-6666-4666-8666-666666666666",
        ArtifactPartRole::DatabaseDump,
        "orders.dump",
        "v2/wrong-prefix/orders.dump.enc",
        "d",
        "e",
    );
    let error = SetManifestV2::new(
        fixture_origin(),
        Engine::Postgresql,
        ConsistencyGrade::ApplicationConsistent,
        1_700_000_000_000,
        "signing-key-01",
        fixture_tool(),
        BTreeMap::new(),
        vec![database, globals],
        fixture_replicas(),
    )
    .expect_err("object keys outside the deterministic prefix must fail");
    assert_eq!(error.code(), "MANIFEST_V2_OBJECT_PREFIX_INVALID");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_manifest_is_published_last_only_after_every_part_reader_round_trip() {
    let temp = tempfile::tempdir().expect("temp directory");
    let spool = temp.path().join("spool");
    let destination = temp.path().join("destination");
    std::fs::create_dir_all(&spool).expect("create spool");
    let (manifest, sources) = publish_fixture(&spool);
    let signing_key = SigningKey::from_bytes(&[9_u8; 32]);
    let signed =
        SignedSetManifestV2::create(manifest.clone(), &signing_key).expect("sign set manifest");
    let local = Arc::new(LocalDest::new("local-writer", destination.clone()));
    let endpoint = ReplicaEndpointV2::new(
        "local:vault-a",
        local.clone() as Arc<dyn Destination>,
        local as Arc<dyn DestinationReaderV2>,
    )
    .expect("local endpoint");

    let outcomes = publish_set_manifest_v2(
        &signed,
        sources,
        vec![endpoint],
        &PipelineCfg::default(),
        &signing_key.verifying_key(),
    )
    .await
    .expect("publish set manifest");

    assert_eq!(outcomes.len(), 1);
    assert!(outcomes[0].result.is_ok());
    assert_eq!(outcomes[0].reader_status, ReaderStatusV2::Eligible);
    assert!(outcomes[0].manifest_receipt.is_some());
    for part in &manifest.parts {
        assert!(
            destination.join(&part.object_key).is_file(),
            "payload must be committed before the set Manifest"
        );
    }
    let manifest_path = destination.join(&manifest.replicas[0].manifest_object_key);
    let landed: SignedSetManifestV2 =
        serde_json::from_slice(&std::fs::read(manifest_path).expect("read landed manifest"))
            .expect("parse landed manifest");
    landed
        .verify(&signing_key.verifying_key())
        .expect("reader round-trip signature");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corrupt_reader_probe_leaves_payload_orphans_without_a_commit_manifest() {
    let temp = tempfile::tempdir().expect("temp directory");
    let spool = temp.path().join("spool");
    let destination = temp.path().join("destination");
    std::fs::create_dir_all(&spool).expect("create spool");
    let (manifest, sources) = publish_fixture(&spool);
    let signing_key = SigningKey::from_bytes(&[9_u8; 32]);
    let signed =
        SignedSetManifestV2::create(manifest.clone(), &signing_key).expect("sign set manifest");
    let endpoint = ReplicaEndpointV2::new(
        "local:vault-a",
        Arc::new(LocalDest::new("local-writer", destination.clone())) as Arc<dyn Destination>,
        Arc::new(CorruptingReader {
            root: destination.clone(),
        }) as Arc<dyn DestinationReaderV2>,
    )
    .expect("fault endpoint");

    let outcomes = publish_set_manifest_v2(
        &signed,
        sources,
        vec![endpoint],
        &PipelineCfg::default(),
        &signing_key.verifying_key(),
    )
    .await
    .expect("fan-out itself remains a per-replica result");

    let failure = outcomes[0]
        .result
        .as_ref()
        .expect_err("corrupt reader probe must reject the replica");
    assert_eq!(
        failure.code(),
        ReplicaPublishError::ReaderChecksumMismatch.code()
    );
    assert_eq!(outcomes[0].reader_status, ReaderStatusV2::Unavailable);
    assert!(outcomes[0].manifest_receipt.is_none());
    assert!(
        !destination
            .join(&manifest.replicas[0].manifest_object_key)
            .exists(),
        "set Manifest is the final marker and must not be published"
    );
    assert!(
        manifest
            .parts
            .iter()
            .any(|part| destination.join(&part.object_key).exists()),
        "already uploaded payload is an orphan diagnostic, not a committed replica"
    );
}

fn fixture_manifest() -> SetManifestV2 {
    let prefix = "v2/22222222-2222-4222-8222-222222222222/44444444-4444-4444-8444-444444444444/11111111-1111-4111-8111-111111111111";
    let globals = fixture_part(
        "55555555-5555-4555-8555-555555555555",
        ArtifactPartRole::Globals,
        "globals.sql",
        &format!("{prefix}/parts/globals.sql.enc"),
        "b",
        "c",
    );
    let database = fixture_part(
        "66666666-6666-4666-8666-666666666666",
        ArtifactPartRole::DatabaseDump,
        "orders.dump",
        &format!("{prefix}/parts/orders.dump.enc"),
        "d",
        "e",
    );
    let mut options = BTreeMap::new();
    options.insert("jobs".to_owned(), "2".to_owned());
    options.insert("no-owner".to_owned(), "true".to_owned());
    SetManifestV2::new(
        fixture_origin(),
        Engine::Postgresql,
        ConsistencyGrade::ApplicationConsistent,
        1_700_000_000_000,
        "signing-key-01",
        fixture_tool(),
        options,
        vec![database, globals],
        fixture_replicas(),
    )
    .expect("valid v2 manifest")
}

fn fixture_origin() -> RecoveryOriginV2 {
    RecoveryOriginV2::new(
        uuid("11111111-1111-4111-8111-111111111111"),
        "instance-a",
        uuid("22222222-2222-4222-8222-222222222222"),
        uuid("33333333-3333-4333-8333-333333333333"),
        uuid("44444444-4444-4444-8444-444444444444"),
        "oid:100",
        "orders",
    )
    .expect("valid origin")
}

fn fixture_tool() -> NativeToolIdentityV2 {
    NativeToolIdentityV2::new("pg_dump", "17.4", "a".repeat(64)).expect("valid tool identity")
}

fn fixture_part(
    id: &str,
    role: ArtifactPartRole,
    logical_name: &str,
    object_key: &str,
    plaintext_seed: &str,
    ciphertext_seed: &str,
) -> ManifestPartV2 {
    let spec = PartSpec::new(
        uuid(id),
        role,
        logical_name,
        true,
        match role {
            ArtifactPartRole::DatabaseDump => "custom",
            ArtifactPartRole::Globals => "plain",
            _ => "json",
        },
    )
    .expect("valid part spec");
    let evidence = EncodingEvidenceV2::new(
        plaintext_seed.repeat(64),
        ciphertext_seed.repeat(64),
        128,
        96,
        "zstd",
        "chacha20poly1305-stream",
        1,
    )
    .expect("valid encoding evidence");
    ManifestPartV2::encoded(spec, evidence, object_key).expect("valid encoded part")
}

fn fixture_replicas() -> Vec<ReplicaCommitmentV2> {
    let prefix = "v2/22222222-2222-4222-8222-222222222222/44444444-4444-4444-8444-444444444444/11111111-1111-4111-8111-111111111111";
    vec![
        ReplicaCommitmentV2::new(
            "nas:vault-b",
            "smb",
            ReplicaRequirementV2::BestEffort,
            ReaderStatusV2::Eligible,
            "none",
            format!("{prefix}/set-manifest.json"),
        )
        .expect("best-effort replica"),
        ReplicaCommitmentV2::new(
            "local:vault-a",
            "local",
            ReplicaRequirementV2::Required,
            ReaderStatusV2::Eligible,
            "none",
            format!("{prefix}/set-manifest.json"),
        )
        .expect("required replica"),
    ]
}

fn publish_fixture(spool: &std::path::Path) -> (SetManifestV2, Vec<EncodedPartSourceV2>) {
    let prefix = "v2/22222222-2222-4222-8222-222222222222/44444444-4444-4444-8444-444444444444/11111111-1111-4111-8111-111111111111";
    let definitions = [
        (
            "55555555-5555-4555-8555-555555555555",
            ArtifactPartRole::Globals,
            "globals.sql",
            b"encoded-globals".as_slice(),
            "plain",
        ),
        (
            "66666666-6666-4666-8666-666666666666",
            ArtifactPartRole::DatabaseDump,
            "orders.dump",
            b"encoded-database".as_slice(),
            "custom",
        ),
    ];
    let mut parts = Vec::new();
    let mut sources = Vec::new();
    for (id, role, logical_name, bytes, dump_format) in definitions {
        let part_id = uuid(id);
        let path = spool.join(format!("{part_id}.enc"));
        std::fs::write(&path, bytes).expect("write encoded part");
        let spec =
            PartSpec::new(part_id, role, logical_name, true, dump_format).expect("part contract");
        let evidence = EncodingEvidenceV2::new(
            "f".repeat(64),
            blake3::hash(bytes).to_hex().to_string(),
            128,
            u64::try_from(bytes.len()).expect("part size"),
            "zstd",
            "chacha20poly1305-stream",
            1,
        )
        .expect("encoding evidence");
        parts.push(
            ManifestPartV2::encoded(spec, evidence, format!("{prefix}/parts/{logical_name}.enc"))
                .expect("manifest part"),
        );
        sources.push(EncodedPartSourceV2::new(part_id, path).expect("part source"));
    }
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
        fixture_origin(),
        Engine::Postgresql,
        ConsistencyGrade::ApplicationConsistent,
        1_700_000_000_000,
        "signing-key-02",
        fixture_tool(),
        BTreeMap::new(),
        parts,
        vec![replica],
    )
    .expect("publish manifest");
    (manifest, sources)
}

struct CorruptingReader {
    root: std::path::PathBuf,
}

#[async_trait]
impl DestinationReaderV2 for CorruptingReader {
    async fn probe_object(
        &self,
        key: &str,
    ) -> Result<ReaderObjectProbeV2, dbbackup_pipeline::DestError> {
        let path = self.root.join(key);
        let mut bytes =
            std::fs::read(&path).map_err(|source| dbbackup_pipeline::DestError::Io {
                key: key.to_owned(),
                source,
            })?;
        if key.contains("/parts/") && !bytes.is_empty() {
            bytes[0] ^= 0xff;
        }
        Ok(ReaderObjectProbeV2::from_bytes(&bytes))
    }

    async fn read_bytes_bounded(
        &self,
        key: &str,
        max_bytes: u64,
    ) -> Result<Vec<u8>, dbbackup_pipeline::DestError> {
        let path = self.root.join(key);
        let bytes = std::fs::read(&path).map_err(|source| dbbackup_pipeline::DestError::Io {
            key: key.to_owned(),
            source,
        })?;
        if u64::try_from(bytes.len()).expect("reader byte count") > max_bytes {
            return Err(dbbackup_pipeline::DestError::ReadLimitExceeded {
                key: key.to_owned(),
                max_bytes,
            });
        }
        Ok(bytes)
    }
}

fn expected_canonical_json() -> &'static str {
    concat!(
        "{\"schema_version\":2,",
        "\"recovery_point_id\":\"11111111-1111-4111-8111-111111111111\",",
        "\"origin_instance_id\":\"instance-a\",",
        "\"occurrence_id\":\"22222222-2222-4222-8222-222222222222\",",
        "\"backup_run_id\":\"33333333-3333-4333-8333-333333333333\",",
        "\"target_id\":\"44444444-4444-4444-8444-444444444444\",",
        "\"source_identity\":\"oid:100\",",
        "\"source_display_name\":\"orders\",",
        "\"engine\":\"postgresql\",",
        "\"consistency_grade\":\"application_consistent\",",
        "\"committed_at_ms\":1700000000000,",
        "\"signing_key_id\":\"signing-key-01\",",
        "\"tool\":{\"name\":\"pg_dump\",\"version\":\"17.4\",",
        "\"executable_sha256\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"},",
        "\"options\":{\"jobs\":\"2\",\"no-owner\":\"true\"},",
        "\"parts\":[",
        "{\"part_id\":\"55555555-5555-4555-8555-555555555555\",",
        "\"role\":\"globals\",\"logical_name\":\"globals.sql\",\"required\":true,",
        "\"dump_format\":\"plain\",",
        "\"plaintext_blake3\":\"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\",",
        "\"ciphertext_blake3\":\"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc\",",
        "\"plaintext_bytes\":128,\"ciphertext_bytes\":96,",
        "\"compression_format\":\"zstd\",\"encryption_format\":\"chacha20poly1305-stream\",",
        "\"dek_wrap_generation\":1,",
        "\"object_key\":\"v2/22222222-2222-4222-8222-222222222222/",
        "44444444-4444-4444-8444-444444444444/",
        "11111111-1111-4111-8111-111111111111/parts/globals.sql.enc\"},",
        "{\"part_id\":\"66666666-6666-4666-8666-666666666666\",",
        "\"role\":\"database_dump\",\"logical_name\":\"orders.dump\",\"required\":true,",
        "\"dump_format\":\"custom\",",
        "\"plaintext_blake3\":\"dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd\",",
        "\"ciphertext_blake3\":\"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee\",",
        "\"plaintext_bytes\":128,\"ciphertext_bytes\":96,",
        "\"compression_format\":\"zstd\",\"encryption_format\":\"chacha20poly1305-stream\",",
        "\"dek_wrap_generation\":1,",
        "\"object_key\":\"v2/22222222-2222-4222-8222-222222222222/",
        "44444444-4444-4444-8444-444444444444/",
        "11111111-1111-4111-8111-111111111111/parts/orders.dump.enc\"}],",
        "\"replicas\":[",
        "{\"destination_fingerprint\":\"local:vault-a\",\"destination_kind\":\"local\",",
        "\"requirement\":\"required\",\"reader_status\":\"eligible\",",
        "\"immutability_capability\":\"none\",",
        "\"manifest_object_key\":\"v2/22222222-2222-4222-8222-222222222222/",
        "44444444-4444-4444-8444-444444444444/",
        "11111111-1111-4111-8111-111111111111/set-manifest.json\"},",
        "{\"destination_fingerprint\":\"nas:vault-b\",\"destination_kind\":\"smb\",",
        "\"requirement\":\"best_effort\",\"reader_status\":\"eligible\",",
        "\"immutability_capability\":\"none\",",
        "\"manifest_object_key\":\"v2/22222222-2222-4222-8222-222222222222/",
        "44444444-4444-4444-8444-444444444444/",
        "11111111-1111-4111-8111-111111111111/set-manifest.json\"}]}"
    )
}

fn uuid(value: &str) -> Uuid {
    Uuid::parse_str(value).expect("fixture UUID")
}
