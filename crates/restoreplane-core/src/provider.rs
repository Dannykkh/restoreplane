use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::Engine;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCapability {
    Probe,
    Discovery,
    ToolPreflight,
    Backup,
    VerifyL1,
    VerifyL2,
    RestoreCreateNew,
    Validation,
    RestoreL3,
}

impl ProviderCapability {
    pub const REQUIRED: [Self; 9] = [
        Self::Probe,
        Self::Discovery,
        Self::ToolPreflight,
        Self::Backup,
        Self::VerifyL1,
        Self::VerifyL2,
        Self::RestoreCreateNew,
        Self::Validation,
        Self::RestoreL3,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Probe => "probe",
            Self::Discovery => "discovery",
            Self::ToolPreflight => "tool_preflight",
            Self::Backup => "backup",
            Self::VerifyL1 => "verify_l1",
            Self::VerifyL2 => "verify_l2",
            Self::RestoreCreateNew => "restore_create_new",
            Self::Validation => "validation",
            Self::RestoreL3 => "restore_l3",
        }
    }
}

impl std::fmt::Display for ProviderCapability {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityOutcome {
    Passed,
    Limited,
    Unsupported,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityEvidence {
    capability: ProviderCapability,
    outcome: CapabilityOutcome,
    evidence_ref: String,
    limitation: Option<String>,
}

impl CapabilityEvidence {
    pub fn new(
        capability: ProviderCapability,
        outcome: CapabilityOutcome,
        evidence_ref: impl Into<String>,
        limitation: Option<String>,
    ) -> Result<Self, ProviderContractError> {
        let evidence_ref = evidence_ref.into();
        if !valid_text(&evidence_ref, 1, 1_024)
            || limitation
                .as_ref()
                .is_some_and(|value| !valid_text(value, 1, 2_048))
            || matches!(
                outcome,
                CapabilityOutcome::Limited | CapabilityOutcome::Unsupported
            ) && limitation.is_none()
        {
            return Err(ProviderContractError::EvidenceInvalid);
        }
        Ok(Self {
            capability,
            outcome,
            evidence_ref,
            limitation,
        })
    }

    pub const fn capability(&self) -> ProviderCapability {
        self.capability
    }

    pub const fn outcome(&self) -> CapabilityOutcome {
        self.outcome
    }

    pub fn evidence_ref(&self) -> &str {
        &self.evidence_ref
    }

    pub fn limitation(&self) -> Option<&str> {
        self.limitation.as_deref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolTrust {
    AuthenticodeValid {
        publisher: String,
    },
    PackageSignatureValid {
        publisher: String,
    },
    ImageDigestPinned {
        image_digest: String,
    },
    LockedDependency {
        package: String,
        version: String,
        registry_checksum: String,
    },
    Unverified,
}

impl ToolTrust {
    pub fn is_verified(&self) -> bool {
        match self {
            Self::AuthenticodeValid { publisher } | Self::PackageSignatureValid { publisher } => {
                valid_text(publisher, 1, 512)
            }
            Self::ImageDigestPinned { image_digest } => {
                image_digest.strip_prefix("sha256:").is_some_and(is_sha256)
            }
            Self::LockedDependency {
                package,
                version,
                registry_checksum,
            } => {
                valid_tool_name(package)
                    && valid_text(version, 1, 128)
                    && is_sha256(registry_checksum)
            }
            Self::Unverified => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeToolIdentity {
    name: String,
    canonical_path: String,
    version: String,
    sha256: String,
    trust: ToolTrust,
}

impl NativeToolIdentity {
    pub fn new(
        name: impl Into<String>,
        canonical_path: impl Into<String>,
        version: impl Into<String>,
        sha256: impl Into<String>,
        trust: ToolTrust,
    ) -> Result<Self, ProviderContractError> {
        let value = Self {
            name: name.into(),
            canonical_path: canonical_path.into(),
            version: version.into(),
            sha256: sha256.into(),
            trust,
        };
        if !valid_tool_name(&value.name)
            || !valid_text(&value.canonical_path, 1, 2_048)
            || !looks_canonical(&value.canonical_path)
            || !valid_text(&value.version, 1, 256)
            || !is_sha256(&value.sha256)
        {
            return Err(ProviderContractError::ToolIdentityInvalid);
        }
        Ok(value)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn canonical_path(&self) -> &str {
        &self.canonical_path
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    pub const fn trust(&self) -> &ToolTrust {
        &self.trust
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CertificationIdentity {
    source_revision: String,
    build_id: String,
    os: String,
    architecture: String,
    fixture_id: String,
    recorded_at_ms: i64,
}

impl CertificationIdentity {
    pub fn new(
        source_revision: impl Into<String>,
        build_id: impl Into<String>,
        os: impl Into<String>,
        architecture: impl Into<String>,
        fixture_id: impl Into<String>,
        recorded_at_ms: i64,
    ) -> Result<Self, ProviderContractError> {
        let value = Self {
            source_revision: source_revision.into(),
            build_id: build_id.into(),
            os: os.into(),
            architecture: architecture.into(),
            fixture_id: fixture_id.into(),
            recorded_at_ms,
        };
        if !(7..=64).contains(&value.source_revision.len())
            || !value
                .source_revision
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || !is_sha256(&value.build_id)
            || !valid_text(&value.os, 1, 128)
            || !valid_text(&value.architecture, 1, 128)
            || !valid_text(&value.fixture_id, 1, 512)
            || value.recorded_at_ms < 0
        {
            return Err(ProviderContractError::CertificationIdentityInvalid);
        }
        Ok(value)
    }

    pub fn source_revision(&self) -> &str {
        &self.source_revision
    }

    pub fn build_id(&self) -> &str {
        &self.build_id
    }

    pub fn fixture_id(&self) -> &str {
        &self.fixture_id
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderLimitation {
    code: String,
    detail: String,
}

impl ProviderLimitation {
    pub fn new(
        code: impl Into<String>,
        detail: impl Into<String>,
    ) -> Result<Self, ProviderContractError> {
        let value = Self {
            code: code.into(),
            detail: detail.into(),
        };
        if !valid_code(&value.code) || !valid_text(&value.detail, 1, 2_048) {
            return Err(ProviderContractError::EvidenceInvalid);
        }
        Ok(value)
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupportStatus {
    Supported,
    Candidate,
    Experimental,
    Limited,
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCertification {
    identity: CertificationIdentity,
    engine: Engine,
    server_version: String,
    server_stable_id: String,
    tools: Vec<NativeToolIdentity>,
    capabilities: Vec<CapabilityEvidence>,
    limitations: Vec<ProviderLimitation>,
    missing_capabilities: Vec<ProviderCapability>,
    status: SupportStatus,
}

impl ProviderCertification {
    #[allow(clippy::too_many_arguments)]
    pub fn evaluate(
        identity: CertificationIdentity,
        engine: Engine,
        server_version: impl Into<String>,
        server_stable_id: impl Into<String>,
        tools: Vec<NativeToolIdentity>,
        capabilities: Vec<CapabilityEvidence>,
        limitations: Vec<ProviderLimitation>,
    ) -> Result<Self, ProviderContractError> {
        let server_version = server_version.into();
        let server_stable_id = server_stable_id.into();
        if !valid_text(&server_version, 1, 512)
            || !valid_text(&server_stable_id, 1, 512)
            || tools.len() > 32
            || capabilities.len() > 64
            || limitations.len() > 128
        {
            return Err(ProviderContractError::EvidenceInvalid);
        }

        let mut by_capability = BTreeMap::new();
        for evidence in &capabilities {
            if by_capability
                .insert(evidence.capability, evidence.outcome)
                .is_some()
            {
                return Err(ProviderContractError::CapabilityDuplicate);
            }
        }
        let missing_capabilities = ProviderCapability::REQUIRED
            .into_iter()
            .filter(|capability| !by_capability.contains_key(capability))
            .collect::<Vec<_>>();

        let status = if by_capability
            .values()
            .any(|outcome| *outcome == CapabilityOutcome::Failed)
        {
            SupportStatus::Experimental
        } else if by_capability
            .values()
            .any(|outcome| *outcome == CapabilityOutcome::Unsupported)
        {
            SupportStatus::Unsupported
        } else if !missing_capabilities.is_empty()
            || tools.is_empty()
            || tools.iter().any(|tool| !tool.trust.is_verified())
        {
            SupportStatus::Candidate
        } else if by_capability
            .values()
            .any(|outcome| *outcome == CapabilityOutcome::Limited)
        {
            SupportStatus::Limited
        } else {
            SupportStatus::Supported
        };

        Ok(Self {
            identity,
            engine,
            server_version,
            server_stable_id,
            tools,
            capabilities,
            limitations,
            missing_capabilities,
            status,
        })
    }

    pub const fn status(&self) -> SupportStatus {
        self.status
    }

    pub fn missing_capabilities(&self) -> &[ProviderCapability] {
        &self.missing_capabilities
    }

    pub const fn is_publishable_supported(&self) -> bool {
        matches!(self.status, SupportStatus::Supported)
    }

    pub const fn identity(&self) -> &CertificationIdentity {
        &self.identity
    }

    pub const fn engine(&self) -> Engine {
        self.engine
    }

    pub fn tools(&self) -> &[NativeToolIdentity] {
        &self.tools
    }

    pub fn capabilities(&self) -> &[CapabilityEvidence] {
        &self.capabilities
    }

    pub fn limitations(&self) -> &[ProviderLimitation] {
        &self.limitations
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreMode {
    CreateNew,
    Overwrite,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreTargetSnapshot {
    database_name: String,
    target_exists: bool,
    bound_fingerprint: Option<String>,
    observed_fingerprint: Option<String>,
    confirmation_valid: bool,
    confirmation_consumed: bool,
}

impl RestoreTargetSnapshot {
    pub fn new(
        database_name: impl Into<String>,
        target_exists: bool,
        bound_fingerprint: Option<&str>,
        observed_fingerprint: Option<&str>,
        confirmation_valid: bool,
        confirmation_consumed: bool,
    ) -> Self {
        Self {
            database_name: database_name.into(),
            target_exists,
            bound_fingerprint: bound_fingerprint.map(str::to_owned),
            observed_fingerprint: observed_fingerprint.map(str::to_owned),
            confirmation_valid,
            confirmation_consumed,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreSafetyPolicy {
    overwrite_supported: bool,
    forbidden_databases: BTreeSet<String>,
    overwrite_requires_confirmation: bool,
    overwrite_requires_target_fingerprint: bool,
}

impl RestoreSafetyPolicy {
    pub fn new(
        overwrite_supported: bool,
        forbidden_databases: impl IntoIterator<Item = impl Into<String>>,
        overwrite_requires_confirmation: bool,
        overwrite_requires_target_fingerprint: bool,
    ) -> Result<Self, ProviderContractError> {
        let forbidden_databases = forbidden_databases
            .into_iter()
            .map(|name| name.into().to_ascii_lowercase())
            .collect::<BTreeSet<_>>();
        if forbidden_databases
            .iter()
            .any(|database| !valid_database_name(database))
        {
            return Err(ProviderContractError::RestorePolicyInvalid);
        }
        Ok(Self {
            overwrite_supported,
            forbidden_databases,
            overwrite_requires_confirmation,
            overwrite_requires_target_fingerprint,
        })
    }

    pub fn validate(
        &self,
        mode: RestoreMode,
        target: &RestoreTargetSnapshot,
    ) -> Result<(), RestoreSafetyError> {
        if !valid_database_name(&target.database_name) {
            return Err(RestoreSafetyError::TargetInvalid);
        }
        if self
            .forbidden_databases
            .contains(&target.database_name.to_ascii_lowercase())
        {
            return Err(RestoreSafetyError::SystemDatabaseForbidden);
        }
        match mode {
            RestoreMode::CreateNew if target.target_exists => {
                Err(RestoreSafetyError::CreateNewTargetExists)
            }
            RestoreMode::CreateNew => Ok(()),
            RestoreMode::Overwrite if !self.overwrite_supported => {
                Err(RestoreSafetyError::OverwriteUnsupported)
            }
            RestoreMode::Overwrite if !target.target_exists => {
                Err(RestoreSafetyError::OverwriteTargetMissing)
            }
            RestoreMode::Overwrite
                if self.overwrite_requires_confirmation
                    && (!target.confirmation_valid || target.confirmation_consumed) =>
            {
                Err(RestoreSafetyError::ConfirmationInvalidOrReplayed)
            }
            RestoreMode::Overwrite if self.overwrite_requires_target_fingerprint => {
                match (
                    target.bound_fingerprint.as_deref(),
                    target.observed_fingerprint.as_deref(),
                ) {
                    (Some(bound), Some(observed)) if bound == observed => Ok(()),
                    (Some(_), Some(_)) => Err(RestoreSafetyError::TargetFingerprintChanged),
                    _ => Err(RestoreSafetyError::TargetFingerprintMissing),
                }
            }
            RestoreMode::Overwrite => Ok(()),
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RestoreSafetyError {
    #[error("RESTORE_TARGET_INVALID")]
    TargetInvalid,
    #[error("RESTORE_SYSTEM_DATABASE_FORBIDDEN")]
    SystemDatabaseForbidden,
    #[error("RESTORE_CREATE_NEW_TARGET_EXISTS")]
    CreateNewTargetExists,
    #[error("RESTORE_OVERWRITE_UNSUPPORTED")]
    OverwriteUnsupported,
    #[error("RESTORE_OVERWRITE_TARGET_MISSING")]
    OverwriteTargetMissing,
    #[error("RESTORE_CONFIRMATION_INVALID_OR_REPLAYED")]
    ConfirmationInvalidOrReplayed,
    #[error("RESTORE_TARGET_FINGERPRINT_CHANGED")]
    TargetFingerprintChanged,
    #[error("RESTORE_TARGET_FINGERPRINT_MISSING")]
    TargetFingerprintMissing,
}

impl RestoreSafetyError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::TargetInvalid => "RESTORE_TARGET_INVALID",
            Self::SystemDatabaseForbidden => "RESTORE_SYSTEM_DATABASE_FORBIDDEN",
            Self::CreateNewTargetExists => "RESTORE_CREATE_NEW_TARGET_EXISTS",
            Self::OverwriteUnsupported => "RESTORE_OVERWRITE_UNSUPPORTED",
            Self::OverwriteTargetMissing => "RESTORE_OVERWRITE_TARGET_MISSING",
            Self::ConfirmationInvalidOrReplayed => "RESTORE_CONFIRMATION_INVALID_OR_REPLAYED",
            Self::TargetFingerprintChanged => "RESTORE_TARGET_FINGERPRINT_CHANGED",
            Self::TargetFingerprintMissing => "RESTORE_TARGET_FINGERPRINT_MISSING",
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProviderContractError {
    #[error("PROVIDER_CERTIFICATION_IDENTITY_INVALID")]
    CertificationIdentityInvalid,
    #[error("PROVIDER_TOOL_IDENTITY_INVALID")]
    ToolIdentityInvalid,
    #[error("PROVIDER_CAPABILITY_EVIDENCE_INVALID")]
    EvidenceInvalid,
    #[error("PROVIDER_CAPABILITY_DUPLICATE")]
    CapabilityDuplicate,
    #[error("PROVIDER_RESTORE_POLICY_INVALID")]
    RestorePolicyInvalid,
}

impl ProviderContractError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::CertificationIdentityInvalid => "PROVIDER_CERTIFICATION_IDENTITY_INVALID",
            Self::ToolIdentityInvalid => "PROVIDER_TOOL_IDENTITY_INVALID",
            Self::EvidenceInvalid => "PROVIDER_CAPABILITY_EVIDENCE_INVALID",
            Self::CapabilityDuplicate => "PROVIDER_CAPABILITY_DUPLICATE",
            Self::RestorePolicyInvalid => "PROVIDER_RESTORE_POLICY_INVALID",
        }
    }
}

fn valid_text(value: &str, min_len: usize, max_len: usize) -> bool {
    (min_len..=max_len).contains(&value.len()) && !value.contains(['\0', '\r', '\n'])
}

fn valid_tool_name(value: &str) -> bool {
    valid_text(value, 1, 128)
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-' | b'.')
        })
}

fn valid_code(value: &str) -> bool {
    valid_text(value, 1, 128)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

fn valid_database_name(value: &str) -> bool {
    valid_text(value, 1, 128) && !value.contains(['/', '\\', ':'])
}

fn looks_canonical(value: &str) -> bool {
    value.starts_with(r"\\")
        || value.as_bytes().get(1).is_some_and(|byte| *byte == b':')
        || value.starts_with('/')
        || value.starts_with("docker://")
        || value.starts_with("embedded://")
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
