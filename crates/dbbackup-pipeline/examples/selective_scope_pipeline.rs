use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};
use dbbackup_core::{ArtifactPartRole, ConsistencyGrade, Engine, PartSpec};
use dbbackup_pipeline::{
    decrypt_and_decompress, publish_set_manifest_v2, run_artifact, Destination,
    DestinationReaderV2, DumpArtifact, DumpExit, DumpStream, EncodedPartSourceV2,
    EncodingEvidenceV2, LocalDest, ManifestPartV2, NativeToolIdentityV2, PipelineCfg,
    ReaderStatusV2, RecoveryOriginV2, ReplicaCommitmentV2, ReplicaEndpointV2, ReplicaRequirementV2,
    SetManifestV2, SignedSetManifestV2, MAX_SET_MANIFEST_BYTES_V2, SET_MANIFEST_SCHEMA_VERSION,
};
use dbbackup_secrets::{
    generate_signing_keypair, unwrap_dek_with_kek, wrap_dek_with_kek, Dek, WrapSet,
};
use serde::Serialize;
use uuid::Uuid;

pub const MAX_SELECTIVE_EVIDENCE_PLAINTEXT_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct EvidenceRequest {
    pub input_path: PathBuf,
    pub destination_root: PathBuf,
    pub recovered_path: PathBuf,
    pub evidence_path: PathBuf,
    pub engine: Engine,
    pub dump_format: String,
    pub scope_identity: String,
    pub tool_name: String,
    pub tool_version: String,
    pub tool_sha256: String,
    pub source_revision: String,
    pub source_tree_digest: String,
    pub build_id: String,
    pub max_plaintext_bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectivePipelineEvidence {
    pub schema_version: u32,
    pub engine: Engine,
    pub source_revision: String,
    pub source_tree_digest: String,
    pub build_id: String,
    pub scope_mode: String,
    pub scope_identity: String,
    pub dump_format: String,
    pub tool_name: String,
    pub tool_version: String,
    pub tool_executable_sha256: String,
    pub plaintext_bytes: u64,
    pub ciphertext_bytes: u64,
    pub plaintext_blake3: String,
    pub ciphertext_blake3: String,
    pub compression_format: String,
    pub encryption_format: String,
    pub dek_wrap_generation: u32,
    pub set_manifest_schema_version: u32,
    pub signed_set_manifest_verified: bool,
    pub dek_wrap_round_trip_verified: bool,
    pub required_replica_reader_eligible: bool,
    pub payload_reader_round_trip_verified: bool,
    pub manifest_reader_round_trip_verified: bool,
    pub required_replica_requirement: String,
    pub required_replica_reader_status: String,
    pub manifest_blake3: String,
    pub manifest_object_key: String,
    pub payload_object_key: String,
}

pub async fn execute_selective_pipeline(
    request: EvidenceRequest,
) -> anyhow::Result<SelectivePipelineEvidence> {
    validate_request(&request)?;
    let input_metadata = std::fs::metadata(&request.input_path).with_context(|| {
        format!(
            "read native dump metadata: {}",
            request.input_path.display()
        )
    })?;
    if !input_metadata.is_file() || input_metadata.len() == 0 {
        bail!("SELECTIVE_EVIDENCE_INPUT_INVALID");
    }
    if input_metadata.len() > request.max_plaintext_bytes {
        bail!("SELECTIVE_EVIDENCE_INPUT_TOO_LARGE");
    }
    assert_empty_destination(&request.destination_root)?;

    let recovery_point_id = Uuid::new_v4();
    let occurrence_id = Uuid::new_v4();
    let backup_run_id = Uuid::new_v4();
    let scope_id = Uuid::new_v4();
    let part_id = Uuid::new_v4();
    let prefix = format!("v2/{occurrence_id}/{scope_id}/{recovery_point_id}");
    let payload_object_key = format!("{prefix}/parts/{part_id}.enc");
    let manifest_object_key = format!("{prefix}/set-manifest.json");
    let destination_fingerprint = "local:g2-selective-pipeline";

    let spool = tempfile::tempdir().context("create bounded pipeline spool")?;
    let spool_path = spool.path().join(format!("{part_id}.enc"));
    let input = tokio::fs::File::open(&request.input_path)
        .await
        .context("open selected native dump")?;
    let artifact = DumpArtifact::Stream(DumpStream {
        reader: Box::new(input),
        wait: Box::pin(async {
            Ok(DumpExit {
                code: Some(0),
                success: true,
                stderr_tail: String::new(),
            })
        }),
        abort: None,
    });
    let dek = Dek::generate();
    let kek = Dek::generate();
    let commit = run_artifact(
        artifact,
        spool_path.clone(),
        dek.clone(),
        &PipelineCfg::default(),
    )
    .await
    .context("encode selected native dump")?;
    if commit.plaintext_bytes != input_metadata.len() {
        bail!("SELECTIVE_EVIDENCE_PLAINTEXT_LENGTH_MISMATCH");
    }

    let wrap_generation = 1;
    let wrap_set =
        wrap_dek_with_kek(&kek, &dek, wrap_generation).context("wrap selective evidence DEK")?;
    let mut options = BTreeMap::new();
    options.insert("backupScopeMode".to_owned(), "explicit_tables".to_owned());
    options.insert(
        "backupScopeIdentity".to_owned(),
        request.scope_identity.clone(),
    );
    options.insert("sourceRevision".to_owned(), request.source_revision.clone());
    options.insert(
        "sourceTreeDigest".to_owned(),
        request.source_tree_digest.clone(),
    );
    options.insert("buildId".to_owned(), request.build_id.clone());
    options.insert(
        "encryptionKeyRef".to_owned(),
        "ephemeral-g2-selective-kek".to_owned(),
    );
    options.insert(
        format!("dekWrap.{part_id}"),
        serde_json::to_string(&wrap_set).context("serialize DEK wrap")?,
    );

    let part_spec = PartSpec::new(
        part_id,
        ArtifactPartRole::DatabaseDump,
        format!("{}-selected-scope", engine_name(request.engine)),
        true,
        request.dump_format.clone(),
    )
    .context("selected database part contract")?;
    let part = ManifestPartV2::encoded(
        part_spec,
        EncodingEvidenceV2::new(
            blake3::Hash::from_bytes(commit.plaintext_blake3)
                .to_hex()
                .to_string(),
            blake3::Hash::from_bytes(commit.ciphertext_blake3)
                .to_hex()
                .to_string(),
            commit.plaintext_bytes,
            commit.ciphertext_bytes,
            "zstd",
            "chacha20poly1305_stream",
            wrap_generation,
        )
        .context("selected pipeline encoding evidence")?,
        payload_object_key.clone(),
    )
    .context("selected pipeline manifest part")?;
    let replica = ReplicaCommitmentV2::new(
        destination_fingerprint,
        "local",
        ReplicaRequirementV2::Required,
        ReaderStatusV2::Eligible,
        "none",
        manifest_object_key.clone(),
    )
    .context("required evidence replica")?;
    let tool = NativeToolIdentityV2::new(
        request.tool_name.clone(),
        request.tool_version.clone(),
        request.tool_sha256.clone(),
    )
    .context("native dump tool identity")?;
    let committed_at_ms = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock before Unix epoch")?
            .as_millis(),
    )
    .context("commit time overflow")?;
    let manifest = SetManifestV2::new(
        RecoveryOriginV2::new(
            recovery_point_id,
            "g2-selective-evidence",
            occurrence_id,
            backup_run_id,
            scope_id,
            request.scope_identity.clone(),
            format!("{} explicit table scope", engine_name(request.engine)),
        )
        .context("selective recovery origin")?,
        request.engine,
        ConsistencyGrade::TransactionConsistent,
        committed_at_ms,
        "ephemeral-g2-selective-signing-key",
        tool,
        options,
        vec![part],
        vec![replica],
    )
    .context("selective signed Set Manifest")?;
    let (signing_key, verifying_key) = generate_signing_keypair();
    let signed = SignedSetManifestV2::create(manifest, &signing_key)
        .context("sign selective Set Manifest")?;
    signed
        .verify(&verifying_key)
        .context("verify selective Set Manifest before publication")?;

    let local = Arc::new(LocalDest::new(
        "g2-selective-required",
        request.destination_root.clone(),
    ));
    let endpoint = ReplicaEndpointV2::new(
        destination_fingerprint,
        local.clone() as Arc<dyn Destination>,
        local.clone() as Arc<dyn DestinationReaderV2>,
    )
    .context("selective pipeline replica endpoint")?;
    let outcomes = publish_set_manifest_v2(
        &signed,
        vec![EncodedPartSourceV2::new(part_id, spool_path)
            .context("selected encoded part source")?],
        vec![endpoint],
        &PipelineCfg::default(),
        &verifying_key,
    )
    .await
    .context("publish selected pipeline Set Manifest")?;
    let outcome = outcomes
        .first()
        .filter(|_| outcomes.len() == 1)
        .context("selective required replica outcome missing")?;
    if outcome.requirement != ReplicaRequirementV2::Required
        || outcome.reader_status != ReaderStatusV2::Eligible
        || outcome.result.is_err()
        || outcome.payload_receipts.len() != 1
        || outcome.manifest_receipt.is_none()
    {
        bail!("SELECTIVE_EVIDENCE_REQUIRED_REPLICA_FAILED");
    }

    let payload_probe = local
        .probe_object(&payload_object_key)
        .await
        .context("probe published selective payload")?;
    let landed_manifest_bytes = local
        .read_bytes_bounded(&manifest_object_key, MAX_SET_MANIFEST_BYTES_V2)
        .await
        .context("read published selective Set Manifest")?;
    let landed: SignedSetManifestV2 = serde_json::from_slice(&landed_manifest_bytes)
        .context("parse published selective Set Manifest")?;
    landed
        .verify(&verifying_key)
        .context("verify published selective Set Manifest")?;
    if landed != signed {
        bail!("SELECTIVE_EVIDENCE_MANIFEST_ROUND_TRIP_MISMATCH");
    }
    let landed_part = landed
        .manifest
        .parts
        .first()
        .context("published database part missing")?;
    if payload_probe.bytes != landed_part.ciphertext_bytes
        || payload_probe.blake3 != landed_part.ciphertext_blake3
    {
        bail!("SELECTIVE_EVIDENCE_PAYLOAD_ROUND_TRIP_MISMATCH");
    }
    let landed_wrap: WrapSet = serde_json::from_str(
        landed
            .manifest
            .options
            .get(&format!("dekWrap.{part_id}"))
            .context("published DEK wrap missing")?,
    )
    .context("parse published DEK wrap")?;
    let recovered_dek = unwrap_dek_with_kek(&kek, &landed_wrap)
        .context("unwrap DEK from published Set Manifest")?;
    let ciphertext_path = request.destination_root.join(&payload_object_key);
    let recovered = decrypt_and_decompress(&ciphertext_path, &recovered_dek)
        .context("decrypt selected pipeline payload")?;
    if u64::try_from(recovered.len()).unwrap_or(u64::MAX) != commit.plaintext_bytes
        || blake3::hash(&recovered).as_bytes() != &commit.plaintext_blake3
    {
        bail!("SELECTIVE_EVIDENCE_RECOVERED_PAYLOAD_MISMATCH");
    }
    if let Some(parent) = request.recovered_path.parent() {
        std::fs::create_dir_all(parent).context("create recovered dump parent")?;
    }
    std::fs::write(&request.recovered_path, &recovered).context("write recovered native dump")?;

    let evidence = SelectivePipelineEvidence {
        schema_version: 1,
        engine: request.engine,
        source_revision: request.source_revision,
        source_tree_digest: request.source_tree_digest,
        build_id: request.build_id,
        scope_mode: "explicit_tables".to_owned(),
        scope_identity: request.scope_identity,
        dump_format: request.dump_format,
        tool_name: request.tool_name,
        tool_version: request.tool_version,
        tool_executable_sha256: request.tool_sha256,
        plaintext_bytes: commit.plaintext_bytes,
        ciphertext_bytes: commit.ciphertext_bytes,
        plaintext_blake3: blake3::Hash::from_bytes(commit.plaintext_blake3)
            .to_hex()
            .to_string(),
        ciphertext_blake3: blake3::Hash::from_bytes(commit.ciphertext_blake3)
            .to_hex()
            .to_string(),
        compression_format: "zstd".to_owned(),
        encryption_format: "chacha20poly1305_stream".to_owned(),
        dek_wrap_generation: wrap_generation,
        set_manifest_schema_version: SET_MANIFEST_SCHEMA_VERSION,
        signed_set_manifest_verified: true,
        dek_wrap_round_trip_verified: true,
        required_replica_reader_eligible: true,
        payload_reader_round_trip_verified: true,
        manifest_reader_round_trip_verified: true,
        required_replica_requirement: "required".to_owned(),
        required_replica_reader_status: "eligible".to_owned(),
        manifest_blake3: blake3::hash(&landed_manifest_bytes).to_hex().to_string(),
        manifest_object_key,
        payload_object_key,
    };
    write_evidence_atomically(&request.evidence_path, &evidence)?;
    Ok(evidence)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let request = parse_request(std::env::args().skip(1))?;
    let evidence = execute_selective_pipeline(request).await?;
    println!("{}", serde_json::to_string(&evidence)?);
    Ok(())
}

fn parse_request(arguments: impl Iterator<Item = String>) -> anyhow::Result<EvidenceRequest> {
    let mut values = HashMap::new();
    let mut arguments = arguments;
    while let Some(flag) = arguments.next() {
        if !flag.starts_with("--") {
            bail!("SELECTIVE_EVIDENCE_ARGUMENT_INVALID: {flag}");
        }
        let value = arguments
            .next()
            .with_context(|| format!("SELECTIVE_EVIDENCE_ARGUMENT_MISSING: {flag}"))?;
        if values.insert(flag.clone(), value).is_some() {
            bail!("SELECTIVE_EVIDENCE_ARGUMENT_DUPLICATE: {flag}");
        }
    }
    let engine = match required(&mut values, "--engine")?.as_str() {
        "postgresql" => Engine::Postgresql,
        "mysql" => Engine::Mysql,
        "mariadb" => Engine::Mariadb,
        _ => bail!("SELECTIVE_EVIDENCE_ENGINE_UNSUPPORTED"),
    };
    let request = EvidenceRequest {
        input_path: required(&mut values, "--input")?.into(),
        destination_root: required(&mut values, "--destination")?.into(),
        recovered_path: required(&mut values, "--recovered")?.into(),
        evidence_path: required(&mut values, "--evidence")?.into(),
        engine,
        dump_format: required(&mut values, "--dump-format")?,
        scope_identity: required(&mut values, "--scope-identity")?,
        tool_name: required(&mut values, "--tool-name")?,
        tool_version: required(&mut values, "--tool-version")?,
        tool_sha256: required(&mut values, "--tool-sha256")?,
        source_revision: required(&mut values, "--source-revision")?,
        source_tree_digest: required(&mut values, "--source-tree-digest")?,
        build_id: required(&mut values, "--build-id")?,
        max_plaintext_bytes: MAX_SELECTIVE_EVIDENCE_PLAINTEXT_BYTES,
    };
    if !values.is_empty() {
        bail!("SELECTIVE_EVIDENCE_ARGUMENT_UNKNOWN");
    }
    Ok(request)
}

fn required(values: &mut HashMap<String, String>, flag: &str) -> anyhow::Result<String> {
    values
        .remove(flag)
        .with_context(|| format!("SELECTIVE_EVIDENCE_ARGUMENT_MISSING: {flag}"))
}

fn validate_request(request: &EvidenceRequest) -> anyhow::Result<()> {
    if request.max_plaintext_bytes == 0
        || request.max_plaintext_bytes > MAX_SELECTIVE_EVIDENCE_PLAINTEXT_BYTES
        || request.scope_identity.is_empty()
        || request.scope_identity.len() > 512
        || request.scope_identity.contains(['\0', '\r', '\n'])
        || !is_lower_hex(&request.tool_sha256, 64)
        || !is_lower_hex(&request.source_revision, 40)
        || !is_lower_hex(&request.source_tree_digest, 64)
        || !is_lower_hex(&request.build_id, 64)
    {
        bail!("SELECTIVE_EVIDENCE_REQUEST_INVALID");
    }
    Ok(())
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn assert_empty_destination(path: &std::path::Path) -> anyhow::Result<()> {
    if path.exists() {
        if !path.is_dir() || std::fs::read_dir(path)?.next().is_some() {
            bail!("SELECTIVE_EVIDENCE_DESTINATION_NOT_EMPTY");
        }
    } else {
        std::fs::create_dir_all(path).context("create selective evidence destination")?;
    }
    Ok(())
}

fn engine_name(engine: Engine) -> &'static str {
    match engine {
        Engine::Postgresql => "postgresql",
        Engine::Mysql => "mysql",
        Engine::Mariadb => "mariadb",
        Engine::Sqlserver => "sqlserver",
        Engine::Sqlite => "sqlite",
    }
}

fn write_evidence_atomically(
    path: &std::path::Path,
    evidence: &SelectivePipelineEvidence,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("create evidence parent")?;
    }
    let part_path = path.with_extension("json.part");
    let serialized = serde_json::to_vec_pretty(evidence).context("serialize pipeline evidence")?;
    std::fs::write(&part_path, serialized).context("write pipeline evidence part")?;
    std::fs::rename(&part_path, path).context("commit pipeline evidence")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(temp: &tempfile::TempDir, max_plaintext_bytes: u64) -> EvidenceRequest {
        EvidenceRequest {
            input_path: temp.path().join("selected.dump"),
            destination_root: temp.path().join("destination"),
            recovered_path: temp.path().join("recovered.dump"),
            evidence_path: temp.path().join("pipeline-evidence.json"),
            engine: Engine::Postgresql,
            dump_format: "custom".to_owned(),
            scope_identity: "public.selected_records".to_owned(),
            tool_name: "pg_dump".to_owned(),
            tool_version: "pg_dump 18.4".to_owned(),
            tool_sha256: "a".repeat(64),
            source_revision: "b".repeat(40),
            source_tree_digest: "c".repeat(64),
            build_id: "d".repeat(64),
            max_plaintext_bytes,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_dump_is_encoded_published_verified_unwrapped_and_recovered() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let request = request(&temp, 1024 * 1024);
        let mut payload = (0..(192 * 1024))
            .map(|index| (index * 31 + 7) as u8)
            .collect::<Vec<_>>();
        let secret_marker = b"fixture-secret-must-not-escape";
        payload[..secret_marker.len()].copy_from_slice(secret_marker);
        std::fs::write(&request.input_path, &payload).expect("native dump fixture");

        let evidence = execute_selective_pipeline(request.clone())
            .await
            .expect("selective pipeline evidence");

        assert_eq!(
            std::fs::read(&request.recovered_path).expect("recovered dump"),
            payload
        );
        assert_eq!(evidence.compression_format, "zstd");
        assert_eq!(evidence.encryption_format, "chacha20poly1305_stream");
        assert!(evidence.signed_set_manifest_verified);
        assert!(evidence.dek_wrap_round_trip_verified);
        assert!(evidence.required_replica_reader_eligible);
        assert!(evidence.payload_reader_round_trip_verified);
        assert!(evidence.manifest_reader_round_trip_verified);
        assert_eq!(evidence.required_replica_requirement, "required");
        assert_eq!(evidence.required_replica_reader_status, "eligible");
        assert_eq!(evidence.source_revision, "b".repeat(40));
        assert_eq!(evidence.source_tree_digest, "c".repeat(64));
        assert_eq!(evidence.build_id, "d".repeat(64));

        let serialized =
            std::fs::read_to_string(&request.evidence_path).expect("bounded metadata evidence");
        assert!(!serialized.contains("signatureEd25519"));
        assert!(!serialized.contains("verifyingKey"));
        assert!(!serialized.contains("fixture-secret-must-not-escape"));
        assert!(!serialized.contains(&request.input_path.to_string_lossy().to_string()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn oversized_input_is_rejected_before_destination_or_evidence_is_created() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let request = request(&temp, 1024);
        std::fs::write(&request.input_path, vec![0x5a; 1025]).expect("oversized fixture");

        let error = execute_selective_pipeline(request.clone())
            .await
            .expect_err("oversized native dump must fail closed");

        assert!(error
            .to_string()
            .contains("SELECTIVE_EVIDENCE_INPUT_TOO_LARGE"));
        assert!(!request.destination_root.exists());
        assert!(!request.recovered_path.exists());
        assert!(!request.evidence_path.exists());
    }
}
