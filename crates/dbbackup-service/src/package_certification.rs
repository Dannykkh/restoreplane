use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use dbbackup_catalog::{
    CertificationCapability, EngineCertificationSpec, NativeToolSpec, OperationalStore,
    PackageCertificationImportOutcome, RepositoryError, VerifiedPackageCertificationSpec,
};
use dbbackup_core::ProviderCapability;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

const MAX_PACKAGE_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
const MAX_CERTIFICATION_DOCUMENT_BYTES: u64 = 256 * 1024;
const MAX_NATIVE_AUDIT_BYTES: u64 = 2 * 1024 * 1024;
const MAX_NATIVE_FILE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_PACKAGE_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_NATIVE_TREE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_PACKAGE_TREE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_VERSION_OUTPUT_BYTES: usize = 8 * 1024;
const DEFAULT_VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CERTIFICATION_TTL_MS: i64 = 90 * 24 * 60 * 60 * 1_000;
const MAX_CLOCK_SKEW_MS: i64 = 5 * 60 * 1_000;
const PROVIDER_CERTIFICATION_PATH: &str = "provider-certifications.json";
const NATIVE_AUDIT_PATH: &str = "tools/native-tools-audit.json";

const REQUIRED_CAPABILITIES: [&str; 9] = [
    "probe",
    "discovery",
    "tool_preflight",
    "backup",
    "verify_l1",
    "verify_l2",
    "restore_create_new",
    "validation",
    "restore_l3",
];
const REQUIRED_SCOPE_MODES: [&str; 2] = ["whole_database", "explicit_tables"];
const POSTGRESQL_EXECUTABLES: [&str; 4] = [
    "pg/pg18/pg_dump.exe",
    "pg/pg18/pg_dumpall.exe",
    "pg/pg18/pg_restore.exe",
    "pg/pg18/psql.exe",
];
const MYSQL_EXECUTABLES: [&str; 2] = ["mysql/mysql8/mysqldump.exe", "mysql/mysql8/mysql.exe"];
const MARIADB_EXECUTABLES: [&str; 2] = [
    "mysql/mariadb11/mariadb-dump.exe",
    "mysql/mariadb11/mariadb.exe",
];

#[derive(Debug, Error)]
pub enum PackageCertificationBootstrapError {
    #[error("PACKAGE_INSTALL_ROOT_INVALID")]
    InstallRootInvalid,
    #[error("PACKAGE_INSTALL_ROOT_ACL_INVALID")]
    InstallRootAclInvalid,
    #[error("PACKAGE_MANIFEST_MISSING")]
    PackageManifestMissing,
    #[error("PACKAGE_MANIFEST_INVALID")]
    PackageManifestInvalid,
    #[error("PACKAGE_JSON_LIMIT_EXCEEDED")]
    JsonLimitExceeded,
    #[error("PACKAGE_FILE_MISSING")]
    PackageFileMissing,
    #[error("PACKAGE_FILE_PATH_INVALID")]
    PackageFilePathInvalid,
    #[error("PACKAGE_FILE_SIZE_MISMATCH")]
    PackageFileSizeMismatch,
    #[error("PACKAGE_FILE_DIGEST_MISMATCH")]
    PackageFileDigestMismatch,
    #[error("PACKAGE_REPARSE_POINT_BLOCKED")]
    ReparsePointBlocked,
    #[error("PACKAGE_NATIVE_AUDIT_INVALID")]
    NativeAuditInvalid,
    #[error("PACKAGE_NATIVE_TOOL_CONTRACT_INVALID")]
    NativeToolContractInvalid,
    #[error("PACKAGE_CERTIFICATION_DOCUMENT_INVALID")]
    CertificationDocumentInvalid,
    #[error("PACKAGE_CERTIFICATION_CONTRACT_INVALID")]
    CertificationContractInvalid,
    #[error("PACKAGE_TOOL_VERSION_PROBE_FAILED")]
    ToolVersionProbeFailed,
    #[error("PACKAGE_CERTIFICATION_IMPORT_FAILED")]
    CertificationImportFailed(#[source] RepositoryError),
}

impl PackageCertificationBootstrapError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InstallRootInvalid => "PACKAGE_INSTALL_ROOT_INVALID",
            Self::InstallRootAclInvalid => "PACKAGE_INSTALL_ROOT_ACL_INVALID",
            Self::PackageManifestMissing => "PACKAGE_MANIFEST_MISSING",
            Self::PackageManifestInvalid => "PACKAGE_MANIFEST_INVALID",
            Self::JsonLimitExceeded => "PACKAGE_JSON_LIMIT_EXCEEDED",
            Self::PackageFileMissing => "PACKAGE_FILE_MISSING",
            Self::PackageFilePathInvalid => "PACKAGE_FILE_PATH_INVALID",
            Self::PackageFileSizeMismatch => "PACKAGE_FILE_SIZE_MISMATCH",
            Self::PackageFileDigestMismatch => "PACKAGE_FILE_DIGEST_MISMATCH",
            Self::ReparsePointBlocked => "PACKAGE_REPARSE_POINT_BLOCKED",
            Self::NativeAuditInvalid => "PACKAGE_NATIVE_AUDIT_INVALID",
            Self::NativeToolContractInvalid => "PACKAGE_NATIVE_TOOL_CONTRACT_INVALID",
            Self::CertificationDocumentInvalid => "PACKAGE_CERTIFICATION_DOCUMENT_INVALID",
            Self::CertificationContractInvalid => "PACKAGE_CERTIFICATION_CONTRACT_INVALID",
            Self::ToolVersionProbeFailed => "PACKAGE_TOOL_VERSION_PROBE_FAILED",
            Self::CertificationImportFailed(_) => "PACKAGE_CERTIFICATION_IMPORT_FAILED",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageCertificationBootstrapOutcome {
    deduplicated: bool,
    tool_count: usize,
    certification_count: usize,
}

impl PackageCertificationBootstrapOutcome {
    pub const fn deduplicated(&self) -> bool {
        self.deduplicated
    }

    pub const fn tool_count(&self) -> usize {
        self.tool_count
    }

    pub const fn certification_count(&self) -> usize {
        self.certification_count
    }
}

impl From<PackageCertificationImportOutcome> for PackageCertificationBootstrapOutcome {
    fn from(value: PackageCertificationImportOutcome) -> Self {
        Self {
            deduplicated: value.deduplicated(),
            tool_count: value.tool_count(),
            certification_count: value.certification_count(),
        }
    }
}

pub trait ToolVersionProbe {
    fn probe(
        &self,
        executable: &Path,
        expected_sha256: [u8; 32],
        trusted_dependencies: &[(PathBuf, [u8; 32])],
    ) -> Result<String, PackageCertificationBootstrapError>;
}

#[derive(Clone, Copy, Debug)]
pub struct ProcessToolVersionProbe {
    timeout: Duration,
}

impl Default for ProcessToolVersionProbe {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_VERSION_PROBE_TIMEOUT,
        }
    }
}

impl ToolVersionProbe for ProcessToolVersionProbe {
    fn probe(
        &self,
        executable: &Path,
        expected_sha256: [u8; 32],
        trusted_dependencies: &[(PathBuf, [u8; 32])],
    ) -> Result<String, PackageCertificationBootstrapError> {
        probe_version_process(
            executable,
            expected_sha256,
            trusted_dependencies,
            self.timeout,
        )
    }
}

#[derive(Clone, Copy)]
struct PackageAclPolicy<'a> {
    service_sid: &'a str,
    owner_sid: &'a str,
}

pub fn bootstrap_verified_package(
    store: &OperationalStore,
    install_root: &Path,
    observed_at_ms: i64,
    service_sid: &str,
    owner_sid: &str,
) -> Result<PackageCertificationBootstrapOutcome, PackageCertificationBootstrapError> {
    bootstrap_verified_package_with_probe(
        store,
        install_root,
        observed_at_ms,
        service_sid,
        owner_sid,
        &ProcessToolVersionProbe::default(),
    )
}

pub fn bootstrap_verified_package_with_probe(
    store: &OperationalStore,
    install_root: &Path,
    observed_at_ms: i64,
    service_sid: &str,
    owner_sid: &str,
    version_probe: &dyn ToolVersionProbe,
) -> Result<PackageCertificationBootstrapOutcome, PackageCertificationBootstrapError> {
    if observed_at_ms < 0 {
        return Err(PackageCertificationBootstrapError::CertificationContractInvalid);
    }
    let install_root = verified_install_root(install_root)?;
    let acl_policy = PackageAclPolicy {
        service_sid,
        owner_sid,
    };
    verify_package_path_acl(&install_root, acl_policy)?;
    let manifest_path = verified_child_file(&install_root, "package-manifest.json").map_err(
        |error| match error {
            PackageCertificationBootstrapError::PackageFileMissing => {
                PackageCertificationBootstrapError::PackageManifestMissing
            }
            other => other,
        },
    )?;
    verify_certified_path_acl(&install_root, &manifest_path, acl_policy)?;
    let manifest_bytes = read_bounded(&manifest_path, MAX_PACKAGE_MANIFEST_BYTES)?;
    let manifest_sha256 = sha256_bytes(&manifest_bytes);
    let manifest: PackageManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|_| PackageCertificationBootstrapError::PackageManifestInvalid)?;
    let verified_files = validate_package_manifest(&install_root, &manifest, acl_policy)?;

    let audit_bytes =
        read_verified_json(&verified_files, NATIVE_AUDIT_PATH, MAX_NATIVE_AUDIT_BYTES)?;
    let audit: NativeToolAudit = serde_json::from_slice(&audit_bytes)
        .map_err(|_| PackageCertificationBootstrapError::NativeAuditInvalid)?;
    let tools = validate_native_audit(
        &install_root,
        &manifest,
        &verified_files,
        &audit,
        observed_at_ms,
        acl_policy,
        version_probe,
    )?;

    let certification_bytes = read_verified_json(
        &verified_files,
        PROVIDER_CERTIFICATION_PATH,
        MAX_CERTIFICATION_DOCUMENT_BYTES,
    )?;
    let certification_document: ProviderCertificationDocument =
        serde_json::from_slice(&certification_bytes)
            .map_err(|_| PackageCertificationBootstrapError::CertificationDocumentInvalid)?;
    let certifications =
        validate_certification_document(&manifest, &certification_document, observed_at_ms)?;

    let mut spec = VerifiedPackageCertificationSpec::new(manifest_sha256, observed_at_ms);
    for verified_tool in tools {
        spec = spec.with_tool(verified_tool.spec, verified_tool.publisher);
    }
    for verified_certification in certifications {
        spec = spec.with_certification(
            verified_certification.spec,
            verified_certification.certified_at_ms,
            verified_certification.expires_at_ms,
        );
    }
    store
        .certification()
        .import_verified_package(spec)
        .map(PackageCertificationBootstrapOutcome::from)
        .map_err(PackageCertificationBootstrapError::CertificationImportFailed)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VerifiedLaunchDependency {
    path: PathBuf,
    sha256: [u8; 32],
}

impl VerifiedLaunchDependency {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) const fn sha256(&self) -> &[u8; 32] {
        &self.sha256
    }
}

/// Rebinds a launch to the exact package manifest that was imported at service start and returns
/// the audited dependency closure for the selected executable. The supervisor locks and hashes
/// these files before process creation and retains the handles for the child lifetime.
pub(crate) fn verified_launch_dependencies(
    store: &OperationalStore,
    install_root: &Path,
    executable: &Path,
    executable_sha256: &[u8; 32],
) -> Result<Vec<VerifiedLaunchDependency>, PackageCertificationBootstrapError> {
    let install_root = verified_install_root(install_root)?;
    let manifest_path = verified_child_file(&install_root, "package-manifest.json")?;
    let manifest_bytes = read_bounded(&manifest_path, MAX_PACKAGE_MANIFEST_BYTES)?;
    let manifest_sha256 = sha256_bytes(&manifest_bytes);
    let imported_manifest_sha256 = store
        .certification()
        .verified_package_manifest_sha256()
        .map_err(PackageCertificationBootstrapError::CertificationImportFailed)?;
    if manifest_sha256 != imported_manifest_sha256 {
        return Err(PackageCertificationBootstrapError::PackageFileDigestMismatch);
    }
    let manifest: PackageManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|_| PackageCertificationBootstrapError::PackageManifestInvalid)?;

    let executable = fs::canonicalize(executable)
        .map_err(|_| PackageCertificationBootstrapError::PackageFileMissing)?;
    let executable_relative = executable
        .strip_prefix(&install_root)
        .map_err(|_| PackageCertificationBootstrapError::PackageFilePathInvalid)?
        .to_string_lossy()
        .replace('\\', "/");
    let executable_manifest = manifest
        .files
        .iter()
        .find(|entry| path_identity(&entry.path) == path_identity(&executable_relative))
        .ok_or(PackageCertificationBootstrapError::NativeToolContractInvalid)?;
    if executable_manifest.sha256 != encode_sha256(executable_sha256) {
        return Err(PackageCertificationBootstrapError::PackageFileDigestMismatch);
    }

    let audit_manifest = manifest
        .files
        .iter()
        .find(|entry| path_identity(&entry.path) == path_identity(NATIVE_AUDIT_PATH))
        .ok_or(PackageCertificationBootstrapError::NativeToolContractInvalid)?;
    let audit_path = verified_child_file(&install_root, NATIVE_AUDIT_PATH)?;
    let audit_bytes = read_bounded(&audit_path, MAX_NATIVE_AUDIT_BYTES)?;
    let audit_sha256 = sha256_bytes(&audit_bytes);
    if audit_sha256 != audit_manifest.sha256
        || audit_sha256 != manifest.native_tools.audit_manifest_sha256
    {
        return Err(PackageCertificationBootstrapError::PackageFileDigestMismatch);
    }
    let audit: NativeToolAudit = serde_json::from_slice(&audit_bytes)
        .map_err(|_| PackageCertificationBootstrapError::NativeAuditInvalid)?;
    let executable_audit_path = executable_relative
        .strip_prefix("tools/")
        .ok_or(PackageCertificationBootstrapError::NativeToolContractInvalid)?;
    let bundle = audit
        .bundles
        .iter()
        .find(|bundle| {
            bundle
                .required_executables
                .iter()
                .any(|path| path_identity(path) == path_identity(executable_audit_path))
        })
        .ok_or(PackageCertificationBootstrapError::NativeToolContractInvalid)?;

    let expected_tool_paths = audit
        .files
        .iter()
        .map(|entry| path_identity(&format!("tools/{}", entry.path)))
        .chain(std::iter::once(path_identity(NATIVE_AUDIT_PATH)))
        .collect::<BTreeSet<_>>();
    validate_actual_tools_tree(&install_root, &expected_tool_paths, None)?;

    let audit_files = audit
        .files
        .iter()
        .map(|entry| (path_identity(&entry.path), entry))
        .collect::<BTreeMap<_, _>>();
    let manifest_files = manifest
        .files
        .iter()
        .map(|entry| (path_identity(&entry.path), entry))
        .collect::<BTreeMap<_, _>>();
    let mut dependencies = Vec::with_capacity(bundle.dependencies.len());
    for relative in &bundle.dependencies {
        let audit_entry = audit_files
            .get(&path_identity(relative))
            .filter(|entry| entry.role == "dependency")
            .ok_or(PackageCertificationBootstrapError::NativeToolContractInvalid)?;
        let package_relative = format!("tools/{relative}");
        let manifest_entry = manifest_files
            .get(&path_identity(&package_relative))
            .ok_or(PackageCertificationBootstrapError::NativeToolContractInvalid)?;
        if manifest_entry.sha256 != audit_entry.sha256 {
            return Err(PackageCertificationBootstrapError::PackageFileDigestMismatch);
        }
        dependencies.push(VerifiedLaunchDependency {
            path: verified_child_file(&install_root, &package_relative)?,
            sha256: decode_sha256(&audit_entry.sha256)
                .ok_or(PackageCertificationBootstrapError::NativeToolContractInvalid)?,
        });
    }
    Ok(dependencies)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PackageManifest {
    schema_version: u32,
    channel: String,
    version: String,
    minimum_reader_version: String,
    build_id: String,
    revision: String,
    source_tree_digest: String,
    files: Vec<PackageFile>,
    native_tools: NativeToolsManifest,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PackageFile {
    path: String,
    sha256: String,
    size_bytes: u64,
    authenticode_required: bool,
    signer_role: String,
    signer_thumbprint: String,
    publisher: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NativeToolsManifest {
    audit_manifest_path: String,
    audit_manifest_sha256: String,
    file_count: usize,
}

#[derive(Debug)]
struct VerifiedPackageFile {
    path: PathBuf,
    sha256: String,
    size_bytes: u64,
    manifest: PackageFile,
}

fn validate_package_manifest(
    install_root: &Path,
    manifest: &PackageManifest,
    acl_policy: PackageAclPolicy<'_>,
) -> Result<BTreeMap<String, VerifiedPackageFile>, PackageCertificationBootstrapError> {
    if manifest.schema_version != 1
        || manifest.channel != "production"
        || parse_semver(&manifest.version).is_none()
        || parse_semver(&manifest.minimum_reader_version).is_none()
        || parse_semver(&manifest.minimum_reader_version) > parse_semver(&manifest.version)
        || !is_lower_hex(&manifest.build_id, 64, 64)
        || !is_lower_hex(&manifest.revision, 40, 64)
        || !is_lower_hex(&manifest.source_tree_digest, 64, 64)
        || manifest.files.is_empty()
        || manifest.files.len() > 8_192
        || manifest.native_tools.audit_manifest_path != NATIVE_AUDIT_PATH
        || !is_lower_hex(&manifest.native_tools.audit_manifest_sha256, 64, 64)
        || manifest.native_tools.file_count == 0
        || manifest.native_tools.file_count > 4_096
    {
        return Err(PackageCertificationBootstrapError::PackageManifestInvalid);
    }

    let mut verified = BTreeMap::new();
    let mut total_bytes = 0_u64;
    for entry in &manifest.files {
        validate_package_file_contract(entry)?;
        let key = path_identity(&entry.path);
        if verified.contains_key(&key) {
            return Err(PackageCertificationBootstrapError::PackageManifestInvalid);
        }
        total_bytes = total_bytes
            .checked_add(entry.size_bytes)
            .filter(|value| *value <= MAX_PACKAGE_TREE_BYTES)
            .ok_or(PackageCertificationBootstrapError::JsonLimitExceeded)?;
        let path = verified_child_file(install_root, &entry.path)?;
        verify_certified_path_acl(install_root, &path, acl_policy)?;
        let metadata = fs::metadata(&path)
            .map_err(|_| PackageCertificationBootstrapError::PackageFileMissing)?;
        if metadata.len() != entry.size_bytes {
            return Err(PackageCertificationBootstrapError::PackageFileSizeMismatch);
        }
        let actual_sha256 = sha256_file(&path)?;
        if actual_sha256 != entry.sha256 {
            return Err(PackageCertificationBootstrapError::PackageFileDigestMismatch);
        }
        verified.insert(
            key,
            VerifiedPackageFile {
                path,
                sha256: actual_sha256,
                size_bytes: metadata.len(),
                manifest: entry.clone(),
            },
        );
    }

    for required in [
        "dbbackup-service.exe",
        "dbbackup.exe",
        PROVIDER_CERTIFICATION_PATH,
        NATIVE_AUDIT_PATH,
    ] {
        if !verified.contains_key(&path_identity(required)) {
            return Err(PackageCertificationBootstrapError::PackageManifestInvalid);
        }
    }
    for path in verified.keys() {
        if !matches!(
            path.as_str(),
            "dbbackup-service.exe" | "dbbackup.exe" | PROVIDER_CERTIFICATION_PATH
        ) && !path.starts_with("tools/")
        {
            return Err(PackageCertificationBootstrapError::PackageManifestInvalid);
        }
    }
    for executable in ["dbbackup-service.exe", "dbbackup.exe"] {
        let entry = &verified[&path_identity(executable)].manifest;
        if !entry.authenticode_required || entry.signer_role != "release" {
            return Err(PackageCertificationBootstrapError::PackageManifestInvalid);
        }
    }
    for document in [PROVIDER_CERTIFICATION_PATH, NATIVE_AUDIT_PATH] {
        let entry = &verified[&path_identity(document)].manifest;
        if entry.authenticode_required || entry.signer_role != "hash_only" {
            return Err(PackageCertificationBootstrapError::PackageManifestInvalid);
        }
    }
    let audit_file = &verified[&path_identity(NATIVE_AUDIT_PATH)];
    if audit_file.sha256 != manifest.native_tools.audit_manifest_sha256 {
        return Err(PackageCertificationBootstrapError::PackageFileDigestMismatch);
    }
    Ok(verified)
}

fn validate_package_file_contract(
    entry: &PackageFile,
) -> Result<(), PackageCertificationBootstrapError> {
    validate_relative_path(&entry.path)?;
    if !is_lower_hex(&entry.sha256, 64, 64)
        || entry.size_bytes == 0
        || entry.size_bytes > MAX_PACKAGE_FILE_BYTES
        || !valid_text(&entry.publisher, 1, 256)
        || !matches!(
            entry.signer_role.as_str(),
            "release" | "vendor" | "hash_only"
        )
        || !is_optional_thumbprint(&entry.signer_thumbprint)
        || (entry.authenticode_required && entry.signer_role == "hash_only")
        || (!entry.authenticode_required && entry.signer_role != "hash_only")
        || (entry.signer_role == "vendor" && entry.signer_thumbprint.is_empty())
        || (entry.signer_role != "vendor" && !entry.signer_thumbprint.is_empty())
        || (!entry.signer_thumbprint.is_empty()
            && !is_lower_hex(
                &entry.signer_thumbprint,
                entry.signer_thumbprint.len(),
                entry.signer_thumbprint.len(),
            ))
    {
        return Err(PackageCertificationBootstrapError::PackageManifestInvalid);
    }
    Ok(())
}

fn read_verified_json(
    files: &BTreeMap<String, VerifiedPackageFile>,
    relative: &str,
    maximum_bytes: u64,
) -> Result<Vec<u8>, PackageCertificationBootstrapError> {
    let file = files
        .get(&path_identity(relative))
        .ok_or(PackageCertificationBootstrapError::PackageFileMissing)?;
    if file.size_bytes > maximum_bytes {
        return Err(PackageCertificationBootstrapError::JsonLimitExceeded);
    }
    read_bounded(&file.path, maximum_bytes)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NativeToolAudit {
    schema_version: u32,
    audit_id: String,
    approved_at: String,
    bundles: Vec<NativeToolBundle>,
    files: Vec<NativeToolAuditFile>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NativeToolBundle {
    id: String,
    root: String,
    major: u32,
    required_executables: Vec<String>,
    dependencies: Vec<String>,
    licenses: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NativeToolAuditFile {
    path: String,
    sha256: String,
    role: String,
    authenticode_required: bool,
    signer_thumbprint: String,
    publisher: String,
}

struct NativeBundleContract {
    id: &'static str,
    root: &'static str,
    major: u32,
    executables: &'static [&'static str],
}

const NATIVE_BUNDLE_CONTRACTS: [NativeBundleContract; 3] = [
    NativeBundleContract {
        id: "postgresql-18",
        root: "pg/pg18",
        major: 18,
        executables: &POSTGRESQL_EXECUTABLES,
    },
    NativeBundleContract {
        id: "mysql-8",
        root: "mysql/mysql8",
        major: 8,
        executables: &MYSQL_EXECUTABLES,
    },
    NativeBundleContract {
        id: "mariadb-11",
        root: "mysql/mariadb11",
        major: 11,
        executables: &MARIADB_EXECUTABLES,
    },
];

struct VerifiedNativeTool {
    spec: NativeToolSpec,
    publisher: String,
}

fn validate_native_audit(
    install_root: &Path,
    manifest: &PackageManifest,
    package_files: &BTreeMap<String, VerifiedPackageFile>,
    audit: &NativeToolAudit,
    observed_at_ms: i64,
    acl_policy: PackageAclPolicy<'_>,
    version_probe: &dyn ToolVersionProbe,
) -> Result<Vec<VerifiedNativeTool>, PackageCertificationBootstrapError> {
    if audit.schema_version != 1
        || !valid_text(&audit.audit_id, 1, 128)
        || !valid_text(&audit.approved_at, 20, 64)
        || audit.bundles.len() != NATIVE_BUNDLE_CONTRACTS.len()
        || audit.files.is_empty()
        || audit.files.len() > 4_096
        || manifest.native_tools.file_count != audit.files.len()
    {
        return Err(PackageCertificationBootstrapError::NativeAuditInvalid);
    }

    let mut audit_files = BTreeMap::new();
    for entry in &audit.files {
        validate_native_audit_file(entry)?;
        if audit_files
            .insert(path_identity(&entry.path), entry)
            .is_some()
        {
            return Err(PackageCertificationBootstrapError::NativeAuditInvalid);
        }
        let package_path = format!("tools/{}", entry.path);
        let package_file = package_files
            .get(&path_identity(&package_path))
            .ok_or(PackageCertificationBootstrapError::NativeToolContractInvalid)?;
        let expected_signer_role = if entry.authenticode_required {
            "vendor"
        } else {
            "hash_only"
        };
        if package_file.sha256 != entry.sha256
            || package_file.manifest.authenticode_required != entry.authenticode_required
            || package_file.manifest.signer_role != expected_signer_role
            || !package_file
                .manifest
                .signer_thumbprint
                .eq_ignore_ascii_case(&entry.signer_thumbprint)
            || package_file.manifest.publisher != entry.publisher
        {
            return Err(PackageCertificationBootstrapError::NativeToolContractInvalid);
        }
    }

    let expected_tool_paths = audit_files
        .keys()
        .map(|path| format!("tools/{path}"))
        .chain(std::iter::once(NATIVE_AUDIT_PATH.to_owned()))
        .map(|path| path_identity(&path))
        .collect::<BTreeSet<_>>();
    let manifest_tool_paths = package_files
        .keys()
        .filter(|path| path.starts_with("tools/"))
        .cloned()
        .collect::<BTreeSet<_>>();
    if expected_tool_paths != manifest_tool_paths {
        return Err(PackageCertificationBootstrapError::NativeToolContractInvalid);
    }
    validate_actual_tools_tree(install_root, &expected_tool_paths, Some(acl_policy))?;

    let mut bundle_ids = BTreeSet::new();
    let mut verified_tools = Vec::with_capacity(8);
    for contract in &NATIVE_BUNDLE_CONTRACTS {
        let bundle = audit
            .bundles
            .iter()
            .find(|bundle| bundle.id == contract.id)
            .ok_or(PackageCertificationBootstrapError::NativeToolContractInvalid)?;
        if !bundle_ids.insert(bundle.id.as_str())
            || bundle.root != contract.root
            || bundle.major != contract.major
            || !same_exact_set(
                bundle.required_executables.iter().map(String::as_str),
                contract.executables.iter().copied(),
            )
            || bundle.dependencies.is_empty()
            || bundle.dependencies.len() > 512
            || bundle.licenses.is_empty()
            || bundle.licenses.len() > 32
        {
            return Err(PackageCertificationBootstrapError::NativeToolContractInvalid);
        }
        validate_bundle_member_paths(bundle, &audit_files, contract)?;
        let trusted_dependencies = bundle
            .dependencies
            .iter()
            .map(|relative| {
                let audit_file = audit_files
                    .get(&path_identity(relative))
                    .ok_or(PackageCertificationBootstrapError::NativeToolContractInvalid)?;
                let package_path = format!("tools/{relative}");
                let dependency = package_files
                    .get(&path_identity(&package_path))
                    .ok_or(PackageCertificationBootstrapError::NativeToolContractInvalid)?;
                Ok((
                    dependency.path.clone(),
                    decode_sha256(&audit_file.sha256)
                        .ok_or(PackageCertificationBootstrapError::NativeToolContractInvalid)?,
                ))
            })
            .collect::<Result<Vec<_>, PackageCertificationBootstrapError>>()?;
        for relative in contract.executables {
            let audit_file = audit_files
                .get(&path_identity(relative))
                .ok_or(PackageCertificationBootstrapError::NativeToolContractInvalid)?;
            if audit_file.role != "executable" || !audit_file.authenticode_required {
                return Err(PackageCertificationBootstrapError::NativeToolContractInvalid);
            }
            let package_path = format!("tools/{relative}");
            let executable = &package_files[&path_identity(&package_path)].path;
            let executable_sha256 = decode_sha256(&audit_file.sha256)
                .ok_or(PackageCertificationBootstrapError::NativeToolContractInvalid)?;
            let version_text = sanitize_version_output(version_probe.probe(
                executable,
                executable_sha256,
                &trusted_dependencies,
            )?)?;
            validate_version_output(contract.id, &version_text)?;
            let tool_name = executable
                .file_stem()
                .and_then(|value| value.to_str())
                .map(str::to_ascii_lowercase)
                .ok_or(PackageCertificationBootstrapError::NativeToolContractInvalid)?;
            let executable_path = executable
                .to_str()
                .filter(|value| value.len() <= 2_048)
                .ok_or(PackageCertificationBootstrapError::NativeToolContractInvalid)?;
            let tool_id = format!(
                "package-tool-{}-{}",
                contract.id,
                tool_name.replace('_', "-")
            );
            verified_tools.push(VerifiedNativeTool {
                spec: NativeToolSpec::observed(
                    tool_id,
                    tool_name,
                    executable_path,
                    version_text,
                    "x86_64",
                    audit_file.sha256.clone(),
                    "bundled",
                    observed_at_ms,
                ),
                publisher: audit_file.publisher.clone(),
            });
        }
    }
    if bundle_ids.len() != NATIVE_BUNDLE_CONTRACTS.len() || verified_tools.len() != 8 {
        return Err(PackageCertificationBootstrapError::NativeToolContractInvalid);
    }
    Ok(verified_tools)
}

fn validate_native_audit_file(
    entry: &NativeToolAuditFile,
) -> Result<(), PackageCertificationBootstrapError> {
    validate_native_relative_path(&entry.path)?;
    if !is_lower_hex(&entry.sha256, 64, 64)
        || !matches!(
            entry.role.as_str(),
            "executable" | "dependency" | "license" | "resource"
        )
        || !is_optional_thumbprint(&entry.signer_thumbprint)
        || !valid_text(&entry.publisher, 1, 256)
        || (entry.authenticode_required && entry.signer_thumbprint.is_empty())
    {
        return Err(PackageCertificationBootstrapError::NativeAuditInvalid);
    }
    Ok(())
}

fn validate_bundle_member_paths(
    bundle: &NativeToolBundle,
    audit_files: &BTreeMap<String, &NativeToolAuditFile>,
    contract: &NativeBundleContract,
) -> Result<(), PackageCertificationBootstrapError> {
    let mut members = BTreeSet::new();
    for (paths, expected_role) in [
        (bundle.required_executables.as_slice(), "executable"),
        (bundle.dependencies.as_slice(), "dependency"),
        (bundle.licenses.as_slice(), "license"),
    ] {
        for relative in paths {
            validate_native_relative_path(relative)?;
            if !relative.starts_with(&format!("{}/", contract.root))
                || !members.insert(path_identity(relative))
                || audit_files
                    .get(&path_identity(relative))
                    .is_none_or(|entry| entry.role != expected_role)
            {
                return Err(PackageCertificationBootstrapError::NativeToolContractInvalid);
            }
        }
    }
    Ok(())
}

fn validate_actual_tools_tree(
    install_root: &Path,
    expected_paths: &BTreeSet<String>,
    acl_policy: Option<PackageAclPolicy<'_>>,
) -> Result<(), PackageCertificationBootstrapError> {
    let tools_root = verified_child_directory(install_root, "tools")?;
    if let Some(acl_policy) = acl_policy {
        verify_package_path_acl(&tools_root, acl_policy)?;
    }
    let mut discovered = BTreeSet::new();
    let mut pending = vec![tools_root];
    let mut total_bytes = 0_u64;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)
            .map_err(|_| PackageCertificationBootstrapError::PackageFileMissing)?
        {
            let entry =
                entry.map_err(|_| PackageCertificationBootstrapError::PackageFileMissing)?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|_| PackageCertificationBootstrapError::PackageFileMissing)?;
            if metadata.file_type().is_symlink() || metadata_is_reparse_point(&metadata) {
                return Err(PackageCertificationBootstrapError::ReparsePointBlocked);
            }
            if let Some(acl_policy) = acl_policy {
                verify_package_path_acl(&path, acl_policy)?;
            }
            if metadata.is_dir() {
                pending.push(path);
                continue;
            }
            if !metadata.is_file() || metadata.len() > MAX_NATIVE_FILE_BYTES {
                return Err(PackageCertificationBootstrapError::NativeToolContractInvalid);
            }
            total_bytes = total_bytes
                .checked_add(metadata.len())
                .filter(|value| *value <= MAX_NATIVE_TREE_BYTES)
                .ok_or(PackageCertificationBootstrapError::JsonLimitExceeded)?;
            let relative = path
                .strip_prefix(install_root)
                .map_err(|_| PackageCertificationBootstrapError::PackageFilePathInvalid)?
                .to_string_lossy()
                .replace('\\', "/");
            if !discovered.insert(path_identity(&relative)) || discovered.len() > 4_097 {
                return Err(PackageCertificationBootstrapError::NativeToolContractInvalid);
            }
        }
    }
    if discovered != *expected_paths {
        return Err(PackageCertificationBootstrapError::NativeToolContractInvalid);
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProviderCertificationDocument {
    schema_version: u32,
    source_revision: String,
    source_tree_digest: String,
    build_id: String,
    generated_at_ms: i64,
    certifications: Vec<ProviderEngineCertification>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProviderEngineCertification {
    id: String,
    engine: String,
    server_version_range: String,
    tool_name: String,
    tool_version_range: String,
    operating_system: String,
    architecture: String,
    dump_format: String,
    consistency_grade: String,
    evidence_locator: String,
    evidence_checksum_blake3: String,
    certified_at_ms: i64,
    expires_at_ms: i64,
    scope_modes: Vec<String>,
    capabilities: Vec<ProviderCertificationCapability>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProviderCertificationCapability {
    capability: String,
    support_status: String,
    #[serde(default)]
    limitation_text: Option<String>,
    evidence_locator: String,
}

struct ProviderEngineContract {
    engine: &'static str,
    tool_name: &'static str,
    version_range: &'static str,
    dump_format: &'static str,
}

const PROVIDER_ENGINE_CONTRACTS: [ProviderEngineContract; 3] = [
    ProviderEngineContract {
        engine: "postgresql",
        tool_name: "pg_dump",
        version_range: ">=18,<19",
        dump_format: "custom",
    },
    ProviderEngineContract {
        engine: "mysql",
        tool_name: "mysqldump",
        version_range: ">=8,<9",
        dump_format: "sql",
    },
    ProviderEngineContract {
        engine: "mariadb",
        tool_name: "mariadb-dump",
        version_range: ">=11,<12",
        dump_format: "sql",
    },
];

struct VerifiedEngineCertification {
    spec: EngineCertificationSpec,
    certified_at_ms: i64,
    expires_at_ms: i64,
}

fn validate_certification_document(
    manifest: &PackageManifest,
    document: &ProviderCertificationDocument,
    observed_at_ms: i64,
) -> Result<Vec<VerifiedEngineCertification>, PackageCertificationBootstrapError> {
    if document.schema_version != 1
        || document.source_revision != manifest.revision
        || document.source_tree_digest != manifest.source_tree_digest
        || document.build_id != manifest.build_id
        || document.generated_at_ms <= 0
        || document.generated_at_ms > observed_at_ms.saturating_add(MAX_CLOCK_SKEW_MS)
        || document.certifications.len() != PROVIDER_ENGINE_CONTRACTS.len()
    {
        return Err(PackageCertificationBootstrapError::CertificationContractInvalid);
    }
    let mut engines = BTreeSet::new();
    let mut ids = BTreeSet::new();
    let mut verified = Vec::with_capacity(document.certifications.len());
    for (index, certification) in document.certifications.iter().enumerate() {
        let contract = PROVIDER_ENGINE_CONTRACTS
            .iter()
            .find(|contract| contract.engine == certification.engine)
            .ok_or(PackageCertificationBootstrapError::CertificationContractInvalid)?;
        let expected_base =
            format!("package://provider-certifications.json#/certifications/{index}");
        if !engines.insert(certification.engine.as_str())
            || !ids.insert(certification.id.as_str())
            || !valid_certification_id(&certification.id)
            || certification.server_version_range != contract.version_range
            || certification.tool_name != contract.tool_name
            || certification.tool_version_range != contract.version_range
            || certification.operating_system != "windows"
            || certification.architecture != "x86_64"
            || certification.dump_format != contract.dump_format
            || !valid_identifier(&certification.consistency_grade, 1, 128)
            || certification.evidence_locator != expected_base
            || !is_lower_hex(&certification.evidence_checksum_blake3, 64, 64)
            || certification.certified_at_ms < document.generated_at_ms
            || certification.certified_at_ms > observed_at_ms.saturating_add(MAX_CLOCK_SKEW_MS)
            || certification.expires_at_ms <= observed_at_ms
            || certification.expires_at_ms <= certification.certified_at_ms
            || certification.expires_at_ms - certification.certified_at_ms
                > MAX_CERTIFICATION_TTL_MS
            || !same_exact_set(
                certification.scope_modes.iter().map(String::as_str),
                REQUIRED_SCOPE_MODES,
            )
            || certification.capabilities.len() != REQUIRED_CAPABILITIES.len()
        {
            return Err(PackageCertificationBootstrapError::CertificationContractInvalid);
        }
        let mut capability_names = BTreeSet::new();
        let mut spec = EngineCertificationSpec::candidate(
            certification.id.clone(),
            certification.engine.clone(),
            certification.server_version_range.clone(),
            certification.tool_name.clone(),
            certification.tool_version_range.clone(),
            certification.operating_system.clone(),
            certification.architecture.clone(),
            certification.dump_format.clone(),
            certification.consistency_grade.clone(),
            certification.evidence_locator.clone(),
            certification.evidence_checksum_blake3.clone(),
            document.generated_at_ms,
        );
        for capability in &certification.capabilities {
            let expected_locator =
                format!("{expected_base}/capabilities/{}", capability.capability);
            if !REQUIRED_CAPABILITIES.contains(&capability.capability.as_str())
                || !capability_names.insert(capability.capability.as_str())
                || capability.evidence_locator != expected_locator
                || !valid_package_locator(&capability.evidence_locator)
            {
                return Err(PackageCertificationBootstrapError::CertificationContractInvalid);
            }
            let evidence = match capability.support_status.as_str() {
                "passed" if capability.limitation_text.is_none() => {
                    CertificationCapability::passed(
                        capability.capability.clone(),
                        capability.evidence_locator.clone(),
                    )
                }
                "limited"
                    if capability
                        .limitation_text
                        .as_deref()
                        .is_some_and(|text| valid_text(text, 1, 512)) =>
                {
                    CertificationCapability::limited(
                        capability.capability.clone(),
                        capability.limitation_text.clone().unwrap_or_default(),
                        capability.evidence_locator.clone(),
                    )
                }
                _ => return Err(PackageCertificationBootstrapError::CertificationContractInvalid),
            };
            spec = spec.with_capability(evidence);
        }
        if capability_names.len() != ProviderCapability::REQUIRED.len()
            || !valid_package_locator(&certification.evidence_locator)
        {
            return Err(PackageCertificationBootstrapError::CertificationContractInvalid);
        }
        verified.push(VerifiedEngineCertification {
            spec,
            certified_at_ms: certification.certified_at_ms,
            expires_at_ms: certification.expires_at_ms,
        });
    }
    if engines
        != PROVIDER_ENGINE_CONTRACTS
            .iter()
            .map(|contract| contract.engine)
            .collect()
    {
        return Err(PackageCertificationBootstrapError::CertificationContractInvalid);
    }
    Ok(verified)
}

fn verified_install_root(
    install_root: &Path,
) -> Result<PathBuf, PackageCertificationBootstrapError> {
    if !install_root.is_absolute() {
        return Err(PackageCertificationBootstrapError::InstallRootInvalid);
    }
    let metadata = fs::symlink_metadata(install_root)
        .map_err(|_| PackageCertificationBootstrapError::InstallRootInvalid)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata_is_reparse_point(&metadata)
    {
        return Err(PackageCertificationBootstrapError::ReparsePointBlocked);
    }
    fs::canonicalize(install_root)
        .map_err(|_| PackageCertificationBootstrapError::InstallRootInvalid)
}

fn verified_child_file(
    root: &Path,
    relative: &str,
) -> Result<PathBuf, PackageCertificationBootstrapError> {
    verified_child(root, relative, false)
}

fn verified_child_directory(
    root: &Path,
    relative: &str,
) -> Result<PathBuf, PackageCertificationBootstrapError> {
    verified_child(root, relative, true)
}

fn verified_child(
    root: &Path,
    relative: &str,
    directory: bool,
) -> Result<PathBuf, PackageCertificationBootstrapError> {
    validate_relative_path(relative)?;
    let mut candidate = root.to_path_buf();
    for segment in relative.replace('\\', "/").split('/') {
        candidate.push(segment);
        let metadata = fs::symlink_metadata(&candidate)
            .map_err(|_| PackageCertificationBootstrapError::PackageFileMissing)?;
        if metadata.file_type().is_symlink() || metadata_is_reparse_point(&metadata) {
            return Err(PackageCertificationBootstrapError::ReparsePointBlocked);
        }
    }
    let metadata = fs::metadata(&candidate)
        .map_err(|_| PackageCertificationBootstrapError::PackageFileMissing)?;
    if (directory && !metadata.is_dir()) || (!directory && !metadata.is_file()) {
        return Err(PackageCertificationBootstrapError::PackageFileMissing);
    }
    let canonical = fs::canonicalize(candidate)
        .map_err(|_| PackageCertificationBootstrapError::PackageFileMissing)?;
    canonical
        .strip_prefix(root)
        .map_err(|_| PackageCertificationBootstrapError::PackageFilePathInvalid)?;
    Ok(canonical)
}

fn verify_certified_path_acl(
    root: &Path,
    path: &Path,
    acl_policy: PackageAclPolicy<'_>,
) -> Result<(), PackageCertificationBootstrapError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| PackageCertificationBootstrapError::PackageFilePathInvalid)?;
    let mut candidate = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(segment) = component else {
            return Err(PackageCertificationBootstrapError::PackageFilePathInvalid);
        };
        candidate.push(segment);
        verify_package_path_acl(&candidate, acl_policy)?;
    }
    Ok(())
}

#[cfg(windows)]
struct OwnedLocalSid(windows::Win32::Security::PSID);

#[cfg(windows)]
impl OwnedLocalSid {
    fn parse(value: &str) -> Result<Self, PackageCertificationBootstrapError> {
        use windows::core::PCWSTR;
        use windows::Win32::Security::Authorization::ConvertStringSidToSidW;
        use windows::Win32::Security::PSID;

        if value.is_empty()
            || value.len() > 184
            || value.contains(['\0', '\r', '\n'])
            || !value.starts_with("S-")
        {
            return Err(PackageCertificationBootstrapError::InstallRootAclInvalid);
        }
        let value: Vec<u16> = value.encode_utf16().chain(Some(0)).collect();
        let mut sid = PSID::default();
        unsafe { ConvertStringSidToSidW(PCWSTR(value.as_ptr()), &mut sid) }
            .map_err(|_| PackageCertificationBootstrapError::InstallRootAclInvalid)?;
        Ok(Self(sid))
    }

    const fn as_psid(&self) -> windows::Win32::Security::PSID {
        self.0
    }
}

#[cfg(windows)]
impl Drop for OwnedLocalSid {
    fn drop(&mut self) {
        use windows::Win32::Foundation::{LocalFree, HLOCAL};

        let _ = unsafe { LocalFree(HLOCAL(self.0 .0)) };
    }
}

#[cfg(windows)]
struct OwnedLocalSecurityDescriptor(windows::Win32::Security::PSECURITY_DESCRIPTOR);

#[cfg(windows)]
impl Drop for OwnedLocalSecurityDescriptor {
    fn drop(&mut self) {
        use windows::Win32::Foundation::{LocalFree, HLOCAL};

        let _ = unsafe { LocalFree(HLOCAL(self.0 .0)) };
    }
}

#[cfg(windows)]
fn verify_package_path_acl(
    path: &Path,
    acl_policy: PackageAclPolicy<'_>,
) -> Result<(), PackageCertificationBootstrapError> {
    use std::ffi::c_void;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows::Win32::Security::{
        EqualSid, GetAce, GetSecurityDescriptorControl, IsValidSid, ACCESS_ALLOWED_ACE,
        CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, OBJECT_INHERIT_ACE,
        OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED,
    };
    use windows::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;

    const FULL_CONTROL: u32 = 0x001f_01ff;
    const READ_AND_EXECUTE: u32 = 0x0012_00a9;

    fn sid_equal(left: PSID, right: PSID) -> bool {
        unsafe { EqualSid(left, right) }.is_ok()
    }

    if !acl_policy.service_sid.starts_with("S-1-5-80-") || acl_policy.service_sid == "S-1-5-80-0" {
        return Err(PackageCertificationBootstrapError::InstallRootAclInvalid);
    }
    let system_sid = OwnedLocalSid::parse("S-1-5-18")?;
    let owner_sid = OwnedLocalSid::parse(acl_policy.owner_sid)?;
    let service_sid = OwnedLocalSid::parse(acl_policy.service_sid)?;
    if sid_equal(system_sid.as_psid(), owner_sid.as_psid())
        || sid_equal(system_sid.as_psid(), service_sid.as_psid())
        || sid_equal(owner_sid.as_psid(), service_sid.as_psid())
    {
        return Err(PackageCertificationBootstrapError::InstallRootAclInvalid);
    }

    let is_directory = fs::metadata(path)
        .map_err(|_| PackageCertificationBootstrapError::InstallRootAclInvalid)?
        .is_dir();
    let path_wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut descriptor_owner = PSID::default();
    let mut dacl = std::ptr::null_mut();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    let security_query = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(path_wide.as_ptr()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            Some(&mut descriptor_owner),
            None,
            Some(&mut dacl),
            None,
            &mut descriptor,
        )
    };
    if security_query != ERROR_SUCCESS || descriptor.0.is_null() {
        if !descriptor.0.is_null() {
            drop(OwnedLocalSecurityDescriptor(descriptor));
        }
        return Err(PackageCertificationBootstrapError::InstallRootAclInvalid);
    }
    let descriptor = OwnedLocalSecurityDescriptor(descriptor);
    if descriptor_owner.0.is_null() || dacl.is_null() {
        return Err(PackageCertificationBootstrapError::InstallRootAclInvalid);
    }

    let mut control = 0_u16;
    let mut revision = 0_u32;
    unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) }
        .map_err(|_| PackageCertificationBootstrapError::InstallRootAclInvalid)?;
    if control & SE_DACL_PROTECTED.0 == 0
        || !sid_equal(descriptor_owner, owner_sid.as_psid())
        || unsafe { (*dacl).AceCount } != 3
    {
        return Err(PackageCertificationBootstrapError::InstallRootAclInvalid);
    }

    let expected_flags = if is_directory {
        (OBJECT_INHERIT_ACE.0 | CONTAINER_INHERIT_ACE.0) as u8
    } else {
        0
    };
    let mut saw_system = false;
    let mut saw_owner = false;
    let mut saw_service = false;
    for index in 0..3 {
        let mut raw_ace: *mut c_void = std::ptr::null_mut();
        unsafe { GetAce(dacl, index, &mut raw_ace) }
            .map_err(|_| PackageCertificationBootstrapError::InstallRootAclInvalid)?;
        if raw_ace.is_null() {
            return Err(PackageCertificationBootstrapError::InstallRootAclInvalid);
        }
        let header = unsafe { &*raw_ace.cast::<windows::Win32::Security::ACE_HEADER>() };
        if u32::from(header.AceType) != ACCESS_ALLOWED_ACE_TYPE
            || usize::from(header.AceSize) < size_of::<ACCESS_ALLOWED_ACE>()
            || header.AceFlags != expected_flags
        {
            return Err(PackageCertificationBootstrapError::InstallRootAclInvalid);
        }
        let ace = unsafe { &*raw_ace.cast::<ACCESS_ALLOWED_ACE>() };
        let trustee = PSID(std::ptr::addr_of!(ace.SidStart).cast_mut().cast::<c_void>());
        if !unsafe { IsValidSid(trustee) }.as_bool() {
            return Err(PackageCertificationBootstrapError::InstallRootAclInvalid);
        }
        let (seen, expected_mask) = if sid_equal(trustee, system_sid.as_psid()) {
            (&mut saw_system, FULL_CONTROL)
        } else if sid_equal(trustee, owner_sid.as_psid()) {
            (&mut saw_owner, FULL_CONTROL)
        } else if sid_equal(trustee, service_sid.as_psid()) {
            (&mut saw_service, READ_AND_EXECUTE)
        } else {
            return Err(PackageCertificationBootstrapError::InstallRootAclInvalid);
        };
        if *seen || ace.Mask != expected_mask {
            return Err(PackageCertificationBootstrapError::InstallRootAclInvalid);
        }
        *seen = true;
    }
    if !(saw_system && saw_owner && saw_service) {
        return Err(PackageCertificationBootstrapError::InstallRootAclInvalid);
    }
    Ok(())
}

#[cfg(not(windows))]
const fn verify_package_path_acl(
    _path: &Path,
    _acl_policy: PackageAclPolicy<'_>,
) -> Result<(), PackageCertificationBootstrapError> {
    Ok(())
}

fn validate_relative_path(relative: &str) -> Result<(), PackageCertificationBootstrapError> {
    if relative.is_empty()
        || relative.len() > 512
        || relative.contains(['\0', '\r', '\n', ':', '\\'])
        || Path::new(relative).is_absolute()
        || relative.starts_with('/')
        || relative.ends_with('/')
        || Path::new(relative)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        || relative
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(PackageCertificationBootstrapError::PackageFilePathInvalid);
    }
    Ok(())
}

fn validate_native_relative_path(relative: &str) -> Result<(), PackageCertificationBootstrapError> {
    validate_relative_path(relative)?;
    if !(relative.starts_with("pg/") || relative.starts_with("mysql/"))
        || relative.split('/').count() < 3
    {
        return Err(PackageCertificationBootstrapError::NativeToolContractInvalid);
    }
    Ok(())
}

fn read_bounded(
    path: &Path,
    maximum_bytes: u64,
) -> Result<Vec<u8>, PackageCertificationBootstrapError> {
    let metadata =
        fs::metadata(path).map_err(|_| PackageCertificationBootstrapError::PackageFileMissing)?;
    if metadata.len() == 0 || metadata.len() > maximum_bytes {
        return Err(PackageCertificationBootstrapError::JsonLimitExceeded);
    }
    let file =
        File::open(path).map_err(|_| PackageCertificationBootstrapError::PackageFileMissing)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(maximum_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| PackageCertificationBootstrapError::PackageFileMissing)?;
    if bytes.len() as u64 > maximum_bytes {
        return Err(PackageCertificationBootstrapError::JsonLimitExceeded);
    }
    Ok(bytes)
}

fn sha256_file(path: &Path) -> Result<String, PackageCertificationBootstrapError> {
    let mut file =
        File::open(path).map_err(|_| PackageCertificationBootstrapError::PackageFileMissing)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|_| PackageCertificationBootstrapError::PackageFileMissing)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn encode_sha256(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode_sha256(value: &str) -> Option<[u8; 32]> {
    if !is_lower_hex(value, 64, 64) {
        return None;
    }
    let mut decoded = [0_u8; 32];
    for (index, slot) in decoded.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(decoded)
}

fn probe_version_process(
    executable: &Path,
    expected_sha256: [u8; 32],
    trusted_dependencies: &[(PathBuf, [u8; 32])],
    timeout: Duration,
) -> Result<String, PackageCertificationBootstrapError> {
    if !executable.is_absolute() || timeout.is_zero() || timeout > Duration::from_secs(30) {
        return Err(PackageCertificationBootstrapError::ToolVersionProbeFailed);
    }
    #[cfg(windows)]
    {
        use std::collections::BTreeMap;
        use std::ffi::OsString;

        use dbbackup_runtime::{LaunchSpec, ProcessSupervisor};

        let working_directory = executable
            .parent()
            .ok_or(PackageCertificationBootstrapError::ToolVersionProbeFailed)?;
        let mut environment = BTreeMap::<OsString, OsString>::new();
        for key in ["SystemRoot", "WINDIR", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(key) {
                environment.insert(OsString::from(key), value);
            }
        }
        let spec = LaunchSpec::new(
            executable.to_path_buf(),
            working_directory.to_path_buf(),
            expected_sha256,
        )
        .and_then(|spec| spec.with_args(["--version"]))
        .and_then(|spec| spec.with_environment(environment))
        .and_then(|spec| spec.with_trusted_dependencies(trusted_dependencies.iter().cloned()))
        .and_then(|spec| spec.with_output_tail_bytes(MAX_VERSION_OUTPUT_BYTES))
        .map_err(|_| PackageCertificationBootstrapError::ToolVersionProbeFailed)?;
        let mut process = ProcessSupervisor::launch(&spec)
            .map_err(|_| PackageCertificationBootstrapError::ToolVersionProbeFailed)?;
        let Some(exit) = process
            .wait_timeout(timeout)
            .map_err(|_| PackageCertificationBootstrapError::ToolVersionProbeFailed)?
        else {
            let _ = process.terminate();
            return Err(PackageCertificationBootstrapError::ToolVersionProbeFailed);
        };
        if exit.exit_code() != 0 || exit.stdout().truncated() || exit.stderr().truncated() {
            return Err(PackageCertificationBootstrapError::ToolVersionProbeFailed);
        }
        let mut output = exit.stdout().tail().to_vec();
        if !exit.stderr().tail().is_empty() {
            output.push(b'\n');
            output.extend_from_slice(exit.stderr().tail());
        }
        String::from_utf8(output)
            .map_err(|_| PackageCertificationBootstrapError::ToolVersionProbeFailed)
    }

    #[cfg(not(windows))]
    {
        let _ = (expected_sha256, trusted_dependencies);
        Err(PackageCertificationBootstrapError::ToolVersionProbeFailed)
    }
}

fn sanitize_version_output(value: String) -> Result<String, PackageCertificationBootstrapError> {
    let value = value
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .filter(|line| valid_text(line, 1, 512))
        .ok_or(PackageCertificationBootstrapError::ToolVersionProbeFailed)?;
    Ok(value.to_owned())
}

fn validate_version_output(
    bundle_id: &str,
    version_text: &str,
) -> Result<(), PackageCertificationBootstrapError> {
    let lower = version_text.to_ascii_lowercase();
    let valid = match bundle_id {
        "postgresql-18" => lower.contains("postgresql") && lower.contains("18."),
        "mysql-8" => {
            (lower.contains("ver 8.") || lower.contains("distrib 8.")) && !lower.contains("mariadb")
        }
        "mariadb-11" => lower.contains("mariadb") && lower.contains("distrib 11."),
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(PackageCertificationBootstrapError::ToolVersionProbeFailed)
    }
}

fn same_exact_set<'a>(
    observed: impl IntoIterator<Item = &'a str>,
    expected: impl IntoIterator<Item = &'a str>,
) -> bool {
    let observed = observed.into_iter().collect::<Vec<_>>();
    let expected = expected.into_iter().collect::<Vec<_>>();
    let observed_set = observed.iter().copied().collect::<BTreeSet<_>>();
    let expected_set = expected.iter().copied().collect::<BTreeSet<_>>();
    observed.len() == observed_set.len()
        && expected.len() == expected_set.len()
        && observed_set == expected_set
}

fn valid_package_locator(value: &str) -> bool {
    let Some(pointer) = value.strip_prefix("package://provider-certifications.json#/") else {
        return false;
    };
    !pointer.is_empty()
        && value.len() <= 512
        && !pointer.contains("..")
        && !pointer.contains("//")
        && pointer
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'_' | b'-'))
}

fn parse_semver(value: &str) -> Option<(u64, u64, u64)> {
    let parts = value.split('.').collect::<Vec<_>>();
    if parts.len() != 3
        || !parts.iter().all(|part| {
            !part.is_empty()
                && part.bytes().all(|byte| byte.is_ascii_digit())
                && (part == &"0" || !part.starts_with('0'))
        })
    {
        return None;
    }
    Some((
        parts[0].parse().ok()?,
        parts[1].parse().ok()?,
        parts[2].parse().ok()?,
    ))
}

fn valid_certification_id(value: &str) -> bool {
    (3..=128).contains(&value.len())
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-' | b'.')
        })
}

fn valid_identifier(value: &str, minimum: usize, maximum: usize) -> bool {
    valid_text(value, minimum, maximum)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn valid_text(value: &str, minimum: usize, maximum: usize) -> bool {
    (minimum..=maximum).contains(&value.len())
        && !value.chars().any(char::is_control)
        && value.trim() == value
}

fn is_optional_thumbprint(value: &str) -> bool {
    value.is_empty() || is_hex(value, 40, 40) || is_hex(value, 64, 64)
}

fn is_lower_hex(value: &str, minimum: usize, maximum: usize) -> bool {
    (minimum..=maximum).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_hex(value: &str, minimum: usize, maximum: usize) -> bool {
    (minimum..=maximum).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn path_identity(value: &str) -> String {
    value.replace('\\', "/").to_ascii_lowercase()
}

#[cfg(windows)]
fn metadata_is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
const fn metadata_is_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}
