use std::collections::{BTreeMap, BTreeSet};

use dbbackup_core::{ArtifactPartRole, ArtifactSpec, ConsistencyGrade, Engine, PartSpec};
use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

pub const SET_MANIFEST_SCHEMA_VERSION: u32 = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryOriginV2 {
    recovery_point_id: Uuid,
    origin_instance_id: String,
    occurrence_id: Uuid,
    backup_run_id: Uuid,
    target_id: Uuid,
    source_identity: String,
    source_display_name: String,
}

impl RecoveryOriginV2 {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        recovery_point_id: Uuid,
        origin_instance_id: impl Into<String>,
        occurrence_id: Uuid,
        backup_run_id: Uuid,
        target_id: Uuid,
        source_identity: impl Into<String>,
        source_display_name: impl Into<String>,
    ) -> Result<Self, ManifestV2Error> {
        let value = Self {
            recovery_point_id,
            origin_instance_id: origin_instance_id.into(),
            occurrence_id,
            backup_run_id,
            target_id,
            source_identity: source_identity.into(),
            source_display_name: source_display_name.into(),
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), ManifestV2Error> {
        if self.recovery_point_id.is_nil()
            || self.occurrence_id.is_nil()
            || self.backup_run_id.is_nil()
            || self.target_id.is_nil()
            || !is_bounded_text(&self.origin_instance_id, 256)
            || !is_bounded_text(&self.source_identity, 1_024)
            || !is_bounded_text(&self.source_display_name, 512)
        {
            return Err(ManifestV2Error::InputInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeToolIdentityV2 {
    pub name: String,
    pub version: String,
    pub executable_sha256: String,
}

impl NativeToolIdentityV2 {
    pub fn new(
        name: impl Into<String>,
        version: impl Into<String>,
        executable_sha256: impl Into<String>,
    ) -> Result<Self, ManifestV2Error> {
        let value = Self {
            name: name.into(),
            version: version.into(),
            executable_sha256: executable_sha256.into(),
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), ManifestV2Error> {
        if !is_bounded_text(&self.name, 128)
            || !is_bounded_text(&self.version, 256)
            || !is_lower_hex_digest(&self.executable_sha256)
        {
            return Err(ManifestV2Error::ToolIdentityInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodingEvidenceV2 {
    plaintext_blake3: String,
    ciphertext_blake3: String,
    plaintext_bytes: u64,
    ciphertext_bytes: u64,
    compression_format: String,
    encryption_format: String,
    dek_wrap_generation: u32,
}

impl EncodingEvidenceV2 {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        plaintext_blake3: impl Into<String>,
        ciphertext_blake3: impl Into<String>,
        plaintext_bytes: u64,
        ciphertext_bytes: u64,
        compression_format: impl Into<String>,
        encryption_format: impl Into<String>,
        dek_wrap_generation: u32,
    ) -> Result<Self, ManifestV2Error> {
        let value = Self {
            plaintext_blake3: plaintext_blake3.into(),
            ciphertext_blake3: ciphertext_blake3.into(),
            plaintext_bytes,
            ciphertext_bytes,
            compression_format: compression_format.into(),
            encryption_format: encryption_format.into(),
            dek_wrap_generation,
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), ManifestV2Error> {
        if !is_lower_hex_digest(&self.plaintext_blake3)
            || !is_lower_hex_digest(&self.ciphertext_blake3)
            || self.plaintext_bytes == 0
            || self.ciphertext_bytes == 0
            || !is_format(&self.compression_format)
            || !is_format(&self.encryption_format)
            || self.dek_wrap_generation == 0
        {
            return Err(ManifestV2Error::EncodingEvidenceInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestPartV2 {
    pub part_id: Uuid,
    pub role: ArtifactPartRole,
    pub logical_name: String,
    pub required: bool,
    pub dump_format: String,
    pub plaintext_blake3: String,
    pub ciphertext_blake3: String,
    pub plaintext_bytes: u64,
    pub ciphertext_bytes: u64,
    pub compression_format: String,
    pub encryption_format: String,
    pub dek_wrap_generation: u32,
    pub object_key: String,
}

impl ManifestPartV2 {
    pub fn encoded(
        spec: PartSpec,
        evidence: EncodingEvidenceV2,
        object_key: impl Into<String>,
    ) -> Result<Self, ManifestV2Error> {
        evidence.validate()?;
        let value = Self {
            part_id: spec.part_id(),
            role: spec.role(),
            logical_name: spec.logical_name().to_owned(),
            required: spec.required(),
            dump_format: spec.dump_format().to_owned(),
            plaintext_blake3: evidence.plaintext_blake3,
            ciphertext_blake3: evidence.ciphertext_blake3,
            plaintext_bytes: evidence.plaintext_bytes,
            ciphertext_bytes: evidence.ciphertext_bytes,
            compression_format: evidence.compression_format,
            encryption_format: evidence.encryption_format,
            dek_wrap_generation: evidence.dek_wrap_generation,
            object_key: object_key.into(),
        };
        value.validate()?;
        Ok(value)
    }

    fn part_spec(&self) -> Result<PartSpec, ManifestV2Error> {
        PartSpec::new(
            self.part_id,
            self.role,
            self.logical_name.clone(),
            self.required,
            self.dump_format.clone(),
        )
        .map_err(|_| ManifestV2Error::PartInvalid)
    }

    fn validate(&self) -> Result<(), ManifestV2Error> {
        self.part_spec()?;
        EncodingEvidenceV2 {
            plaintext_blake3: self.plaintext_blake3.clone(),
            ciphertext_blake3: self.ciphertext_blake3.clone(),
            plaintext_bytes: self.plaintext_bytes,
            ciphertext_bytes: self.ciphertext_bytes,
            compression_format: self.compression_format.clone(),
            encryption_format: self.encryption_format.clone(),
            dek_wrap_generation: self.dek_wrap_generation,
        }
        .validate()?;
        if !is_bounded_text(&self.object_key, 2_048) {
            return Err(ManifestV2Error::PartInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplicaRequirementV2 {
    Required,
    BestEffort,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReaderStatusV2 {
    Eligible,
    Unavailable,
    Unverified,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplicaCommitmentV2 {
    pub destination_fingerprint: String,
    pub destination_kind: String,
    pub requirement: ReplicaRequirementV2,
    pub reader_status: ReaderStatusV2,
    pub immutability_capability: String,
    pub manifest_object_key: String,
}

impl ReplicaCommitmentV2 {
    pub fn new(
        destination_fingerprint: impl Into<String>,
        destination_kind: impl Into<String>,
        requirement: ReplicaRequirementV2,
        reader_status: ReaderStatusV2,
        immutability_capability: impl Into<String>,
        manifest_object_key: impl Into<String>,
    ) -> Result<Self, ManifestV2Error> {
        let value = Self {
            destination_fingerprint: destination_fingerprint.into(),
            destination_kind: destination_kind.into(),
            requirement,
            reader_status,
            immutability_capability: immutability_capability.into(),
            manifest_object_key: manifest_object_key.into(),
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), ManifestV2Error> {
        if !is_bounded_text(&self.destination_fingerprint, 512)
            || !matches!(
                self.destination_kind.as_str(),
                "local" | "s3" | "ssh_sftp" | "azure_blob" | "smb" | "rclone"
            )
            || !matches!(
                self.immutability_capability.as_str(),
                "none" | "governance" | "compliance" | "server_append_only" | "unknown"
            )
            || !is_bounded_text(&self.manifest_object_key, 2_048)
        {
            return Err(ManifestV2Error::ReplicaInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetManifestV2 {
    pub schema_version: u32,
    pub recovery_point_id: Uuid,
    pub origin_instance_id: String,
    pub occurrence_id: Uuid,
    pub backup_run_id: Uuid,
    pub target_id: Uuid,
    pub source_identity: String,
    pub source_display_name: String,
    pub engine: Engine,
    pub consistency_grade: ConsistencyGrade,
    pub committed_at_ms: i64,
    pub signing_key_id: String,
    pub tool: NativeToolIdentityV2,
    pub options: BTreeMap<String, String>,
    pub parts: Vec<ManifestPartV2>,
    pub replicas: Vec<ReplicaCommitmentV2>,
}

impl SetManifestV2 {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        origin: RecoveryOriginV2,
        engine: Engine,
        consistency_grade: ConsistencyGrade,
        committed_at_ms: i64,
        signing_key_id: impl Into<String>,
        tool: NativeToolIdentityV2,
        options: BTreeMap<String, String>,
        mut parts: Vec<ManifestPartV2>,
        mut replicas: Vec<ReplicaCommitmentV2>,
    ) -> Result<Self, ManifestV2Error> {
        origin.validate()?;
        parts.sort_by_key(|part| part.part_id);
        replicas.sort_by(|left, right| {
            left.destination_fingerprint
                .cmp(&right.destination_fingerprint)
        });
        let value = Self {
            schema_version: SET_MANIFEST_SCHEMA_VERSION,
            recovery_point_id: origin.recovery_point_id,
            origin_instance_id: origin.origin_instance_id,
            occurrence_id: origin.occurrence_id,
            backup_run_id: origin.backup_run_id,
            target_id: origin.target_id,
            source_identity: origin.source_identity,
            source_display_name: origin.source_display_name,
            engine,
            consistency_grade,
            committed_at_ms,
            signing_key_id: signing_key_id.into(),
            tool,
            options,
            parts,
            replicas,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn deterministic_prefix(&self) -> String {
        format!(
            "v2/{}/{}/{}",
            self.occurrence_id, self.target_id, self.recovery_point_id
        )
    }

    pub fn validate(&self) -> Result<(), ManifestV2Error> {
        if self.schema_version != SET_MANIFEST_SCHEMA_VERSION {
            return Err(ManifestV2Error::SchemaUnsupported);
        }
        RecoveryOriginV2 {
            recovery_point_id: self.recovery_point_id,
            origin_instance_id: self.origin_instance_id.clone(),
            occurrence_id: self.occurrence_id,
            backup_run_id: self.backup_run_id,
            target_id: self.target_id,
            source_identity: self.source_identity.clone(),
            source_display_name: self.source_display_name.clone(),
        }
        .validate()?;
        if self.committed_at_ms < 0
            || !is_bounded_text(&self.signing_key_id, 256)
            || self.options.len() > 128
        {
            return Err(ManifestV2Error::InputInvalid);
        }
        self.tool.validate()?;
        for (key, value) in &self.options {
            if !is_bounded_text(key, 128) || !is_bounded_text(value, 1_024) || key.contains('=') {
                return Err(ManifestV2Error::InputInvalid);
            }
        }
        if self.parts.is_empty() || self.parts.len() > 1_024 {
            return Err(ManifestV2Error::PartInvalid);
        }
        if !is_sorted_unique_by(&self.parts, |part| part.part_id) {
            return Err(ManifestV2Error::PartInvalid);
        }
        let mut specs = Vec::with_capacity(self.parts.len());
        let mut object_keys = BTreeSet::new();
        let part_prefix = format!("{}/parts/", self.deterministic_prefix());
        for part in &self.parts {
            part.validate()?;
            if !part.object_key.starts_with(&part_prefix)
                || part.object_key.contains("/../")
                || !object_keys.insert(part.object_key.as_str())
            {
                return Err(ManifestV2Error::ObjectPrefixInvalid);
            }
            specs.push(part.part_spec()?);
        }
        ArtifactSpec::new(self.consistency_grade, specs)
            .map_err(|_| ManifestV2Error::PartInvalid)?;

        if self.replicas.is_empty()
            || self.replicas.len() > 1_024
            || !self
                .replicas
                .iter()
                .any(|replica| replica.requirement == ReplicaRequirementV2::Required)
            || !self
                .replicas
                .windows(2)
                .all(|pair| pair[0].destination_fingerprint < pair[1].destination_fingerprint)
        {
            return Err(ManifestV2Error::ReplicaInvalid);
        }
        let manifest_key = format!("{}/set-manifest.json", self.deterministic_prefix());
        for replica in &self.replicas {
            replica.validate()?;
            if replica.manifest_object_key != manifest_key {
                return Err(ManifestV2Error::ObjectPrefixInvalid);
            }
        }
        Ok(())
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, ManifestV2Error> {
        self.validate()?;
        serde_json::to_vec(self).map_err(|_| ManifestV2Error::SerializationFailed)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedSetManifestV2 {
    pub manifest: SetManifestV2,
    pub signature_ed25519: String,
    pub verifying_key: String,
}

impl SignedSetManifestV2 {
    pub fn create(
        manifest: SetManifestV2,
        signing_key: &SigningKey,
    ) -> Result<Self, ManifestV2Error> {
        let canonical = manifest.canonical_bytes()?;
        let signature = dbbackup_secrets::sign_manifest(signing_key, &canonical);
        Ok(Self {
            manifest,
            signature_ed25519: data_encoding::HEXLOWER.encode(&signature.to_bytes()),
            verifying_key: data_encoding::HEXLOWER.encode(signing_key.verifying_key().as_bytes()),
        })
    }

    pub fn verify(&self, verifying_key: &VerifyingKey) -> Result<(), ManifestV2Error> {
        let canonical = self.manifest.canonical_bytes()?;
        let embedded_key = data_encoding::HEXLOWER
            .decode(self.verifying_key.as_bytes())
            .map_err(|_| ManifestV2Error::SignatureInvalid)?;
        if embedded_key.as_slice() != verifying_key.as_bytes() {
            return Err(ManifestV2Error::SignatureInvalid);
        }
        let signature_bytes = data_encoding::HEXLOWER
            .decode(self.signature_ed25519.as_bytes())
            .map_err(|_| ManifestV2Error::SignatureInvalid)?;
        let signature_array: [u8; 64] = signature_bytes
            .as_slice()
            .try_into()
            .map_err(|_| ManifestV2Error::SignatureInvalid)?;
        let signature = Signature::from_bytes(&signature_array);
        dbbackup_secrets::verify_manifest(verifying_key, &canonical, &signature)
            .map_err(|_| ManifestV2Error::SignatureInvalid)
    }

    pub fn to_json(&self) -> Result<String, ManifestV2Error> {
        serde_json::to_string_pretty(self).map_err(|_| ManifestV2Error::SerializationFailed)
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ManifestV2Error {
    #[error("MANIFEST_V2_INPUT_INVALID")]
    InputInvalid,
    #[error("MANIFEST_V2_SCHEMA_UNSUPPORTED")]
    SchemaUnsupported,
    #[error("MANIFEST_V2_TOOL_IDENTITY_INVALID")]
    ToolIdentityInvalid,
    #[error("MANIFEST_V2_PART_INVALID")]
    PartInvalid,
    #[error("MANIFEST_V2_ENCODING_EVIDENCE_INVALID")]
    EncodingEvidenceInvalid,
    #[error("MANIFEST_V2_REPLICA_INVALID")]
    ReplicaInvalid,
    #[error("MANIFEST_V2_OBJECT_PREFIX_INVALID")]
    ObjectPrefixInvalid,
    #[error("MANIFEST_V2_SIGNATURE_INVALID")]
    SignatureInvalid,
    #[error("MANIFEST_V2_SERIALIZATION_FAILED")]
    SerializationFailed,
}

impl ManifestV2Error {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InputInvalid => "MANIFEST_V2_INPUT_INVALID",
            Self::SchemaUnsupported => "MANIFEST_V2_SCHEMA_UNSUPPORTED",
            Self::ToolIdentityInvalid => "MANIFEST_V2_TOOL_IDENTITY_INVALID",
            Self::PartInvalid => "MANIFEST_V2_PART_INVALID",
            Self::EncodingEvidenceInvalid => "MANIFEST_V2_ENCODING_EVIDENCE_INVALID",
            Self::ReplicaInvalid => "MANIFEST_V2_REPLICA_INVALID",
            Self::ObjectPrefixInvalid => "MANIFEST_V2_OBJECT_PREFIX_INVALID",
            Self::SignatureInvalid => "MANIFEST_V2_SIGNATURE_INVALID",
            Self::SerializationFailed => "MANIFEST_V2_SERIALIZATION_FAILED",
        }
    }
}

fn is_lower_hex_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_bounded_text(value: &str, max_len: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_len
        && !value
            .chars()
            .any(|character| matches!(character, '\0' | '\r' | '\n'))
}

fn is_format(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

fn is_sorted_unique_by<T, K: Ord>(values: &[T], key: impl Fn(&T) -> K) -> bool {
    values.windows(2).all(|pair| key(&pair[0]) < key(&pair[1]))
}
