use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path};

use base64::Engine as _;
use ed25519_dalek::VerifyingKey;
use restoreplane_application::BuildIdentityV1;
use restoreplane_catalog::StandaloneAuthorityBootstrapSpec;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use thiserror::Error;

use crate::ipc::WindowsSid;
use crate::secrets::audit_acl_sddl;

const IDENTITY_CONFIG_VERSION: u32 = 1;
const MAX_IDENTITY_CONFIG_BYTES: u64 = 64 * 1024;
const MAX_APPROVED_ADMINISTRATORS: usize = 32;
const MAX_TRUSTED_SIGNING_KEYS: usize = 32;

#[derive(Clone, Debug)]
pub struct ServiceIdentityConfig {
    owner_sid: WindowsSid,
    approved_administrator_sids: Vec<WindowsSid>,
    cutover_verifier: Option<CutoverVerifierConfig>,
    standalone_bootstrap: Option<StandaloneBootstrapConfig>,
    trusted_signing_keys: BTreeMap<String, VerifyingKey>,
}

#[derive(Clone, Debug)]
pub struct CutoverVerifierConfig {
    evidence_directory: String,
    build_identity: BuildIdentityV1,
}

#[derive(Clone, Debug)]
pub struct StandaloneBootstrapConfig {
    installation_id: uuid::Uuid,
    build_identity: BuildIdentityV1,
    service_binary_sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct IdentityConfigDocument {
    version: u32,
    owner_sid: String,
    approved_administrator_sids: Vec<String>,
    #[serde(default)]
    cutover_verifier: Option<CutoverVerifierConfigDocument>,
    #[serde(default)]
    standalone_bootstrap: Option<StandaloneBootstrapConfigDocument>,
    #[serde(default)]
    trusted_signing_keys: Vec<TrustedSigningKeyDocument>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct CutoverVerifierConfigDocument {
    evidence_directory: String,
    source_revision: String,
    source_tree_digest: String,
    build_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct StandaloneBootstrapConfigDocument {
    installation_id: String,
    source_revision: String,
    package_manifest_digest_sha256: String,
    build_id: String,
    service_binary_sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct TrustedSigningKeyDocument {
    key_id: String,
    ed25519_public_key_base64: String,
}

impl ServiceIdentityConfig {
    pub fn load(path: &Path, service_sid: &WindowsSid) -> Result<Self, ServiceIdentityError> {
        let metadata = fs::symlink_metadata(path).map_err(ServiceIdentityError::Io)?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() == 0
            || metadata.len() > MAX_IDENTITY_CONFIG_BYTES
        {
            return Err(ServiceIdentityError::Invalid);
        }
        let bytes = fs::read(path).map_err(ServiceIdentityError::Io)?;
        let document: IdentityConfigDocument =
            serde_json::from_slice(&bytes).map_err(|_| ServiceIdentityError::Invalid)?;
        let config = Self::from_document(document, service_sid)?;
        let sddl = audit_acl_sddl(path).map_err(|_| ServiceIdentityError::AccessControlInvalid)?;
        if !acl_is_exactly_protected(&sddl, service_sid, &config.owner_sid) {
            return Err(ServiceIdentityError::AccessControlInvalid);
        }
        Ok(config)
    }

    pub const fn owner_sid(&self) -> &WindowsSid {
        &self.owner_sid
    }

    pub fn approved_administrator_sids(&self) -> &[WindowsSid] {
        &self.approved_administrator_sids
    }

    pub const fn cutover_verifier(&self) -> Option<&CutoverVerifierConfig> {
        self.cutover_verifier.as_ref()
    }

    pub const fn standalone_bootstrap(&self) -> Option<&StandaloneBootstrapConfig> {
        self.standalone_bootstrap.as_ref()
    }

    pub const fn trusted_signing_keys(&self) -> &BTreeMap<String, VerifyingKey> {
        &self.trusted_signing_keys
    }

    fn from_document(
        document: IdentityConfigDocument,
        service_sid: &WindowsSid,
    ) -> Result<Self, ServiceIdentityError> {
        if document.version != IDENTITY_CONFIG_VERSION
            || document.approved_administrator_sids.len() > MAX_APPROVED_ADMINISTRATORS
            || document.trusted_signing_keys.len() > MAX_TRUSTED_SIGNING_KEYS
        {
            return Err(ServiceIdentityError::Invalid);
        }
        let owner_sid =
            WindowsSid::parse(&document.owner_sid).map_err(|_| ServiceIdentityError::Invalid)?;
        let mut approved_administrator_sids = document
            .approved_administrator_sids
            .iter()
            .map(|sid| WindowsSid::parse(sid).map_err(|_| ServiceIdentityError::Invalid))
            .collect::<Result<Vec<_>, _>>()?;
        if owner_sid == *service_sid
            || approved_administrator_sids
                .iter()
                .any(|sid| sid == service_sid || sid == &owner_sid)
        {
            return Err(ServiceIdentityError::Invalid);
        }
        approved_administrator_sids.sort();
        let original_length = approved_administrator_sids.len();
        approved_administrator_sids.dedup();
        if approved_administrator_sids.len() != original_length {
            return Err(ServiceIdentityError::Invalid);
        }
        let cutover_verifier = document
            .cutover_verifier
            .map(CutoverVerifierConfig::from_document)
            .transpose()?;
        let standalone_bootstrap = document
            .standalone_bootstrap
            .map(StandaloneBootstrapConfig::from_document)
            .transpose()?;
        if cutover_verifier.is_some() && standalone_bootstrap.is_some() {
            return Err(ServiceIdentityError::Invalid);
        }
        let mut trusted_signing_keys = BTreeMap::new();
        for document in document.trusted_signing_keys {
            if document.key_id.trim().is_empty()
                || document.key_id.len() > 256
                || document.key_id.contains(['\0', '\r', '\n'])
            {
                return Err(ServiceIdentityError::Invalid);
            }
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(document.ed25519_public_key_base64)
                .map_err(|_| ServiceIdentityError::Invalid)?;
            let bytes: [u8; 32] = bytes
                .try_into()
                .map_err(|_| ServiceIdentityError::Invalid)?;
            let key =
                VerifyingKey::from_bytes(&bytes).map_err(|_| ServiceIdentityError::Invalid)?;
            if trusted_signing_keys.insert(document.key_id, key).is_some() {
                return Err(ServiceIdentityError::Invalid);
            }
        }
        Ok(Self {
            owner_sid,
            approved_administrator_sids,
            cutover_verifier,
            standalone_bootstrap,
            trusted_signing_keys,
        })
    }
}

impl StandaloneBootstrapConfig {
    fn from_document(
        document: StandaloneBootstrapConfigDocument,
    ) -> Result<Self, ServiceIdentityError> {
        let installation_id = uuid::Uuid::parse_str(&document.installation_id)
            .map_err(|_| ServiceIdentityError::Invalid)?;
        if installation_id.is_nil() || installation_id.to_string() != document.installation_id {
            return Err(ServiceIdentityError::Invalid);
        }
        let build_identity = BuildIdentityV1::new(
            document.source_revision,
            document.package_manifest_digest_sha256,
            document.build_id,
        )
        .map_err(|_| ServiceIdentityError::Invalid)?;
        if !is_digest(&document.service_binary_sha256) {
            return Err(ServiceIdentityError::Invalid);
        }
        Ok(Self {
            installation_id,
            build_identity,
            service_binary_sha256: document.service_binary_sha256,
        })
    }

    pub fn authority_spec(&self, committed_at_ms: i64) -> StandaloneAuthorityBootstrapSpec {
        StandaloneAuthorityBootstrapSpec::new(
            self.installation_id,
            self.build_identity.source_revision(),
            self.build_identity.source_tree_digest(),
            self.build_identity.build_id(),
            &self.service_binary_sha256,
            committed_at_ms,
        )
    }

    pub fn verify_current_executable(&self) -> Result<(), ServiceIdentityError> {
        let executable = std::env::current_exe().map_err(ServiceIdentityError::Io)?;
        let metadata = fs::symlink_metadata(&executable).map_err(ServiceIdentityError::Io)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(ServiceIdentityError::BinaryIdentityMismatch);
        }
        let mut file = fs::File::open(executable).map_err(ServiceIdentityError::Io)?;
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer).map_err(ServiceIdentityError::Io)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        let actual = format!("{:x}", hasher.finalize());
        if actual
            .as_bytes()
            .ct_eq(self.service_binary_sha256.as_bytes())
            .unwrap_u8()
            != 1
        {
            return Err(ServiceIdentityError::BinaryIdentityMismatch);
        }
        Ok(())
    }
}

impl CutoverVerifierConfig {
    fn from_document(
        document: CutoverVerifierConfigDocument,
    ) -> Result<Self, ServiceIdentityError> {
        if !is_safe_relative_directory(&document.evidence_directory) {
            return Err(ServiceIdentityError::Invalid);
        }
        let build_identity = BuildIdentityV1::new(
            document.source_revision,
            document.source_tree_digest,
            document.build_id,
        )
        .map_err(|_| ServiceIdentityError::Invalid)?;
        Ok(Self {
            evidence_directory: document.evidence_directory,
            build_identity,
        })
    }

    pub fn evidence_directory(&self) -> &str {
        &self.evidence_directory
    }

    pub const fn build_identity(&self) -> &BuildIdentityV1 {
        &self.build_identity
    }
}

#[derive(Debug, Error)]
pub enum ServiceIdentityError {
    #[error("SERVICE_IDENTITY_CONFIG_IO")]
    Io(#[source] io::Error),
    #[error("SERVICE_IDENTITY_CONFIG_INVALID")]
    Invalid,
    #[error("SERVICE_IDENTITY_CONFIG_ACL_INVALID")]
    AccessControlInvalid,
    #[error("SERVICE_IDENTITY_BINARY_MISMATCH")]
    BinaryIdentityMismatch,
}

impl ServiceIdentityError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Io(_) => "SERVICE_IDENTITY_CONFIG_IO",
            Self::Invalid => "SERVICE_IDENTITY_CONFIG_INVALID",
            Self::AccessControlInvalid => "SERVICE_IDENTITY_CONFIG_ACL_INVALID",
            Self::BinaryIdentityMismatch => "SERVICE_IDENTITY_BINARY_MISMATCH",
        }
    }
}

fn acl_is_exactly_protected(sddl: &str, service_sid: &WindowsSid, owner_sid: &WindowsSid) -> bool {
    if !sddl.starts_with("D:P") {
        return false;
    }
    let mut saw_system = false;
    let mut saw_service = false;
    let mut saw_owner = false;
    let mut ace_count = 0_usize;

    for encoded_ace in sddl.split('(').skip(1) {
        let Some(ace) = encoded_ace.strip_suffix(')') else {
            return false;
        };
        let fields: Vec<_> = ace.split(';').collect();
        if fields.len() != 6 || fields[0] != "A" || fields[2] != "FA" {
            return false;
        }
        let trustee = fields[5];
        let is_system = trustee_matches(trustee, "S-1-5-18");
        let is_service = trustee_matches(trustee, service_sid.as_str());
        let is_owner = trustee_matches(trustee, owner_sid.as_str());
        if !(is_system || is_service || is_owner) {
            return false;
        }
        saw_system |= is_system;
        saw_service |= is_service;
        saw_owner |= is_owner;
        ace_count += 1;
    }

    ace_count >= 2 && saw_system && saw_service && saw_owner
}

fn trustee_matches(trustee: &str, sid: &str) -> bool {
    trustee_sids_are_equal(trustee, sid)
}

fn is_safe_relative_directory(value: &str) -> bool {
    let path = Path::new(value);
    !value.is_empty()
        && value.len() <= 128
        && !path.is_absolute()
        && path.components().all(|component| {
            matches!(component, Component::Normal(_))
                && component
                    .as_os_str()
                    .to_str()
                    .is_some_and(|part| !part.is_empty())
        })
}

fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(windows)]
fn trustee_sids_are_equal(left: &str, right: &str) -> bool {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::ConvertStringSidToSidW;
    use windows::Win32::Security::{EqualSid, PSID};

    fn convert(value: &str) -> Option<PSID> {
        let wide: Vec<u16> = value.encode_utf16().chain(Some(0)).collect();
        let mut sid = PSID::default();
        unsafe { ConvertStringSidToSidW(PCWSTR(wide.as_ptr()), &mut sid) }
            .ok()
            .map(|()| sid)
    }

    let Some(left) = convert(left) else {
        return false;
    };
    let Some(right) = convert(right) else {
        let _ = unsafe { LocalFree(HLOCAL(left.0)) };
        return false;
    };
    let equal = unsafe { EqualSid(left, right) }.is_ok();
    let _ = unsafe { LocalFree(HLOCAL(left.0)) };
    let _ = unsafe { LocalFree(HLOCAL(right.0)) };
    equal
}

#[cfg(not(windows))]
fn trustee_sids_are_equal(left: &str, right: &str) -> bool {
    left == right
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standalone_bootstrap_is_explicit_and_mutually_exclusive_with_legacy_cutover() {
        let service_sid = WindowsSid::parse("S-1-5-18").expect("SYSTEM SID");
        let standalone: IdentityConfigDocument = serde_json::from_value(serde_json::json!({
            "version": 1,
            "ownerSid": "S-1-5-19",
            "approvedAdministratorSids": [],
            "standaloneBootstrap": {
                "installationId": "01234567-89ab-4cde-8fab-0123456789ab",
                "sourceRevision": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "packageManifestDigestSha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "buildId": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
                "serviceBinarySha256": "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
            }
        }))
        .expect("standalone document");
        let parsed = ServiceIdentityConfig::from_document(standalone, &service_sid)
            .expect("standalone config");
        assert!(parsed.standalone_bootstrap().is_some());
        assert!(parsed.cutover_verifier().is_none());

        let ambiguous: IdentityConfigDocument = serde_json::from_value(serde_json::json!({
            "version": 1,
            "ownerSid": "S-1-5-19",
            "approvedAdministratorSids": [],
            "standaloneBootstrap": {
                "installationId": "01234567-89ab-4cde-8fab-0123456789ab",
                "sourceRevision": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "packageManifestDigestSha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "buildId": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
                "serviceBinarySha256": "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
            },
            "cutoverVerifier": {
                "evidenceDirectory": "cutover-evidence",
                "sourceRevision": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "sourceTreeDigest": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "buildId": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
            }
        }))
        .expect("ambiguous document");
        assert!(matches!(
            ServiceIdentityConfig::from_document(ambiguous, &service_sid),
            Err(ServiceIdentityError::Invalid)
        ));
    }

    #[test]
    fn standalone_bootstrap_verifies_the_running_binary_before_authority_creation() {
        let executable = std::env::current_exe().expect("current test executable");
        let bytes = fs::read(executable).expect("read current test executable");
        let service_binary_sha256 = format!("{:x}", Sha256::digest(bytes));
        let mut bootstrap = StandaloneBootstrapConfig {
            installation_id: uuid::Uuid::parse_str("01234567-89ab-4cde-8fab-0123456789ab")
                .expect("installation ID"),
            build_identity: BuildIdentityV1::new(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            )
            .expect("build identity"),
            service_binary_sha256,
        };
        bootstrap
            .verify_current_executable()
            .expect("matching current executable");

        bootstrap.service_binary_sha256 = "0".repeat(64);
        assert!(matches!(
            bootstrap.verify_current_executable(),
            Err(ServiceIdentityError::BinaryIdentityMismatch)
        ));
    }
}
