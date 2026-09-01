use std::collections::HashSet;

use restoreplane_core::ProviderCapability;
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};

use crate::OperationalStore;

use super::RepositoryError;

const REQUIRED_PROVIDER_CAPABILITY_COUNT: i64 = ProviderCapability::REQUIRED.len() as i64;
const VERIFIED_PACKAGE_MANIFEST_KEY: &str = "certification.package_manifest_sha256.v1";

#[derive(Clone, Debug)]
pub struct NativeToolSpec {
    id: String,
    tool_name: String,
    executable_path: String,
    version_text: String,
    architecture: String,
    file_sha256: String,
    source_kind: String,
    observed_at_ms: i64,
}

impl NativeToolSpec {
    #[allow(clippy::too_many_arguments)]
    pub fn observed(
        id: impl Into<String>,
        tool_name: impl Into<String>,
        executable_path: impl Into<String>,
        version_text: impl Into<String>,
        architecture: impl Into<String>,
        file_sha256: impl Into<String>,
        source_kind: impl Into<String>,
        observed_at_ms: i64,
    ) -> Self {
        Self {
            id: id.into(),
            tool_name: tool_name.into(),
            executable_path: executable_path.into(),
            version_text: version_text.into(),
            architecture: architecture.into(),
            file_sha256: file_sha256.into(),
            source_kind: source_kind.into(),
            observed_at_ms,
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if self.id.is_empty()
            || self.id.len() > 128
            || self.tool_name.is_empty()
            || self.tool_name.len() > 128
            || self.executable_path.is_empty()
            || self.executable_path.len() > 2048
            || self.version_text.is_empty()
            || self.version_text.len() > 512
            || self.architecture.is_empty()
            || self.architecture.len() > 128
            || !is_digest(&self.file_sha256)
            || !matches!(self.source_kind.as_str(), "bundled" | "system" | "operator")
            || self.observed_at_ms < 0
        {
            return Err(RepositoryError::CertificationInputInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolObservation {
    id: String,
    binary_changed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustedToolIdentity {
    tool_name: String,
    executable_path: String,
    version_text: String,
    file_sha256: String,
}

impl TrustedToolIdentity {
    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    pub fn executable_path(&self) -> &str {
        &self.executable_path
    }

    pub fn version_text(&self) -> &str {
        &self.version_text
    }

    pub fn file_sha256(&self) -> &str {
        &self.file_sha256
    }
}

impl ToolObservation {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub const fn binary_changed(&self) -> bool {
        self.binary_changed
    }
}

#[derive(Clone, Debug)]
pub struct CertificationCapability {
    capability: String,
    support_status: &'static str,
    limitation_text: Option<String>,
    evidence_locator: String,
}

impl CertificationCapability {
    pub fn passed(capability: impl Into<String>, evidence_locator: impl Into<String>) -> Self {
        Self {
            capability: capability.into(),
            support_status: "passed",
            limitation_text: None,
            evidence_locator: evidence_locator.into(),
        }
    }

    pub fn limited(
        capability: impl Into<String>,
        limitation_text: impl Into<String>,
        evidence_locator: impl Into<String>,
    ) -> Self {
        Self {
            capability: capability.into(),
            support_status: "limited",
            limitation_text: Some(limitation_text.into()),
            evidence_locator: evidence_locator.into(),
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if !ProviderCapability::REQUIRED
            .iter()
            .any(|capability| capability.as_str() == self.capability)
            || self.evidence_locator.is_empty()
            || self.evidence_locator.len() > 2048
            || self
                .limitation_text
                .as_ref()
                .is_some_and(|text| text.is_empty() || text.len() > 4 * 1024)
        {
            return Err(RepositoryError::CertificationInputInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct EngineCertificationSpec {
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
    created_at_ms: i64,
    capabilities: Vec<CertificationCapability>,
}

#[derive(Clone, Debug)]
pub struct VerifiedPackageCertificationSpec {
    package_manifest_sha256: String,
    imported_at_ms: i64,
    tools: Vec<(NativeToolSpec, String)>,
    certifications: Vec<(EngineCertificationSpec, i64, i64)>,
}

impl VerifiedPackageCertificationSpec {
    pub fn new(package_manifest_sha256: impl Into<String>, imported_at_ms: i64) -> Self {
        Self {
            package_manifest_sha256: package_manifest_sha256.into(),
            imported_at_ms,
            tools: Vec::new(),
            certifications: Vec::new(),
        }
    }

    pub fn with_tool(mut self, tool: NativeToolSpec, publisher: impl Into<String>) -> Self {
        self.tools.push((tool, publisher.into()));
        self
    }

    pub fn with_certification(
        mut self,
        certification: EngineCertificationSpec,
        certified_at_ms: i64,
        expires_at_ms: i64,
    ) -> Self {
        self.certifications
            .push((certification, certified_at_ms, expires_at_ms));
        self
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if !is_digest(&self.package_manifest_sha256)
            || self.imported_at_ms < 0
            || self.tools.is_empty()
            || self.tools.len() > 128
            || self.certifications.is_empty()
            || self.certifications.len() > 32
        {
            return Err(RepositoryError::CertificationInputInvalid);
        }
        let mut tool_ids = HashSet::new();
        let mut tool_paths = HashSet::new();
        for (tool, publisher) in &self.tools {
            tool.validate()?;
            if tool.source_kind != "bundled"
                || publisher.is_empty()
                || publisher.len() > 512
                || publisher.contains(['\0', '\r', '\n'])
                || !tool_ids.insert(tool.id.as_str())
                || !tool_paths.insert(tool.executable_path.as_str())
            {
                return Err(RepositoryError::CertificationInputInvalid);
            }
        }
        let mut certification_ids = HashSet::new();
        for (certification, certified_at_ms, expires_at_ms) in &self.certifications {
            certification.validate()?;
            if certification.capabilities.len() != REQUIRED_PROVIDER_CAPABILITY_COUNT as usize
                || !certification
                    .evidence_locator
                    .starts_with("package://provider-certifications.json#/")
                || *certified_at_ms < certification.created_at_ms
                || *expires_at_ms <= *certified_at_ms
                || !certification_ids.insert(certification.id.as_str())
            {
                return Err(RepositoryError::CertificationInputInvalid);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageCertificationImportOutcome {
    deduplicated: bool,
    tool_count: usize,
    certification_count: usize,
}

impl PackageCertificationImportOutcome {
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

impl EngineCertificationSpec {
    #[allow(clippy::too_many_arguments)]
    pub fn candidate(
        id: impl Into<String>,
        engine: impl Into<String>,
        server_version_range: impl Into<String>,
        tool_name: impl Into<String>,
        tool_version_range: impl Into<String>,
        operating_system: impl Into<String>,
        architecture: impl Into<String>,
        dump_format: impl Into<String>,
        consistency_grade: impl Into<String>,
        evidence_locator: impl Into<String>,
        evidence_checksum_blake3: impl Into<String>,
        created_at_ms: i64,
    ) -> Self {
        Self {
            id: id.into(),
            engine: engine.into(),
            server_version_range: server_version_range.into(),
            tool_name: tool_name.into(),
            tool_version_range: tool_version_range.into(),
            operating_system: operating_system.into(),
            architecture: architecture.into(),
            dump_format: dump_format.into(),
            consistency_grade: consistency_grade.into(),
            evidence_locator: evidence_locator.into(),
            evidence_checksum_blake3: evidence_checksum_blake3.into(),
            created_at_ms,
            capabilities: Vec::new(),
        }
    }

    pub fn with_capability(mut self, capability: CertificationCapability) -> Self {
        self.capabilities.push(capability);
        self
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if self.id.is_empty()
            || self.id.len() > 128
            || !matches!(
                self.engine.as_str(),
                "postgresql"
                    | "mysql"
                    | "mariadb"
                    | "sqlserver"
                    | "sqlite"
                    | "mongodb"
                    | "firebird"
                    | "redis"
                    | "valkey"
            )
            || [
                &self.server_version_range,
                &self.tool_name,
                &self.tool_version_range,
                &self.operating_system,
                &self.architecture,
                &self.dump_format,
                &self.consistency_grade,
                &self.evidence_locator,
            ]
            .iter()
            .any(|value| value.is_empty())
            || !is_digest(&self.evidence_checksum_blake3)
            || self.created_at_ms < 0
            || self.capabilities.is_empty()
        {
            return Err(RepositoryError::CertificationInputInvalid);
        }
        let mut unique = HashSet::new();
        for capability in &self.capabilities {
            capability.validate()?;
            if !unique.insert(capability.capability.as_str()) {
                return Err(RepositoryError::CertificationInputInvalid);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectiveCapability {
    support_status: String,
    limitation_text: Option<String>,
}

impl EffectiveCapability {
    pub fn support_status(&self) -> &str {
        &self.support_status
    }

    pub fn limitation_text(&self) -> Option<&str> {
        self.limitation_text.as_deref()
    }
}

#[derive(Clone)]
pub struct CertificationRepository {
    store: OperationalStore,
}

impl OperationalStore {
    pub fn certification(&self) -> CertificationRepository {
        CertificationRepository {
            store: self.clone(),
        }
    }
}

impl CertificationRepository {
    pub fn import_verified_package(
        &self,
        spec: VerifiedPackageCertificationSpec,
    ) -> Result<PackageCertificationImportOutcome, RepositoryError> {
        spec.validate()?;
        let tool_count = spec.tools.len();
        let certification_count = spec.certifications.len();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let existing_manifest: Option<String> = transaction
                .query_row(
                    "SELECT value_text FROM app_meta WHERE key = ?1",
                    [VERIFIED_PACKAGE_MANIFEST_KEY],
                    |row| row.get(0),
                )
                .optional()?;
            for (tool, _) in &spec.tools {
                let id_owner: Option<String> = transaction
                    .query_row(
                        "SELECT source_kind FROM native_tool_inventory WHERE id = ?1",
                        [tool.id.as_str()],
                        |row| row.get(0),
                    )
                    .optional()?;
                let path_owner: Option<String> = transaction
                    .query_row(
                        "SELECT source_kind
                         FROM native_tool_inventory
                         WHERE executable_path = ?1 AND deleted_at_ms IS NULL",
                        [tool.executable_path.as_str()],
                        |row| row.get(0),
                    )
                    .optional()?;
                if id_owner.as_deref().is_some_and(|owner| owner != "bundled")
                    || path_owner
                        .as_deref()
                        .is_some_and(|owner| owner != "bundled")
                {
                    return Err(RepositoryError::CertificationInputInvalid);
                }
            }
            for (certification, _, _) in &spec.certifications {
                let evidence_locator: Option<String> = transaction
                    .query_row(
                        "SELECT evidence_locator FROM engine_certifications WHERE id = ?1",
                        [certification.id.as_str()],
                        |row| row.get(0),
                    )
                    .optional()?;
                if evidence_locator.as_deref().is_some_and(|locator| {
                    !locator.starts_with("package://provider-certifications.json#/")
                }) {
                    return Err(RepositoryError::CertificationInputInvalid);
                }
            }
            if existing_manifest.as_deref() == Some(spec.package_manifest_sha256.as_str())
                && verified_package_projection_matches(&transaction, &spec)?
            {
                transaction.commit()?;
                return Ok(PackageCertificationImportOutcome {
                    deduplicated: true,
                    tool_count,
                    certification_count,
                });
            }
            transaction.execute(
                "UPDATE native_tool_inventory
                 SET trust_status = 'blocked', deleted_at_ms = ?1,
                     updated_at_ms = MAX(updated_at_ms, ?1)
                 WHERE source_kind = 'bundled'
                   AND deleted_at_ms IS NULL",
                [spec.imported_at_ms],
            )?;
            transaction.execute(
                "UPDATE engine_certifications
                 SET status = 'suspended', suspended_reason = 'package_replaced',
                     updated_at_ms = MAX(updated_at_ms, ?1)
                 WHERE evidence_locator LIKE 'package://provider-certifications.json#/%'
                   AND status = 'certified'",
                [spec.imported_at_ms],
            )?;
            for (tool, publisher) in spec.tools {
                transaction.execute(
                    "INSERT INTO native_tool_inventory(
                         id, tool_name, executable_path, version_text, architecture,
                         file_sha256, publisher, signature_status, trust_status, source_kind,
                         first_seen_at_ms, last_seen_at_ms, last_verified_at_ms,
                         created_at_ms, updated_at_ms
                     ) VALUES (
                         ?1, ?2, ?3, ?4, ?5,
                         ?6, ?7, 'valid', 'trusted', 'bundled',
                         ?8, ?8, ?8, ?8, ?8
                     )
                     ON CONFLICT(id) DO UPDATE SET
                         tool_name = excluded.tool_name,
                         executable_path = excluded.executable_path,
                         version_text = excluded.version_text,
                         architecture = excluded.architecture,
                         file_sha256 = excluded.file_sha256,
                         publisher = excluded.publisher,
                         signature_status = 'valid',
                         trust_status = 'trusted',
                         source_kind = 'bundled',
                         last_seen_at_ms = excluded.last_seen_at_ms,
                         last_verified_at_ms = excluded.last_verified_at_ms,
                         deleted_at_ms = NULL,
                         updated_at_ms = excluded.updated_at_ms",
                    params![
                        tool.id,
                        tool.tool_name,
                        tool.executable_path,
                        tool.version_text,
                        tool.architecture,
                        tool.file_sha256,
                        publisher,
                        spec.imported_at_ms,
                    ],
                )?;
            }
            for (certification, certified_at_ms, expires_at_ms) in spec.certifications {
                transaction.execute(
                    "INSERT INTO engine_certifications(
                         id, engine, server_version_range, tool_name, tool_version_range,
                         operating_system, architecture, dump_format, consistency_grade,
                         status, evidence_locator, evidence_checksum_blake3,
                         certified_at_ms, expires_at_ms, created_at_ms, updated_at_ms
                     ) VALUES (
                         ?1, ?2, ?3, ?4, ?5,
                         ?6, ?7, ?8, ?9,
                         'certified', ?10, ?11,
                         ?12, ?13, ?14, ?12
                     )
                     ON CONFLICT(id) DO UPDATE SET
                         engine = excluded.engine,
                         server_version_range = excluded.server_version_range,
                         tool_name = excluded.tool_name,
                         tool_version_range = excluded.tool_version_range,
                         operating_system = excluded.operating_system,
                         architecture = excluded.architecture,
                         dump_format = excluded.dump_format,
                         consistency_grade = excluded.consistency_grade,
                         status = 'certified',
                         evidence_locator = excluded.evidence_locator,
                         evidence_checksum_blake3 = excluded.evidence_checksum_blake3,
                         certified_at_ms = excluded.certified_at_ms,
                         expires_at_ms = excluded.expires_at_ms,
                         suspended_reason = NULL,
                         updated_at_ms = excluded.updated_at_ms",
                    params![
                        certification.id,
                        certification.engine,
                        certification.server_version_range,
                        certification.tool_name,
                        certification.tool_version_range,
                        certification.operating_system,
                        certification.architecture,
                        certification.dump_format,
                        certification.consistency_grade,
                        certification.evidence_locator,
                        certification.evidence_checksum_blake3,
                        certified_at_ms,
                        expires_at_ms,
                        certification.created_at_ms,
                    ],
                )?;
                transaction.execute(
                    "DELETE FROM engine_certification_capabilities
                     WHERE certification_id = ?1",
                    [certification.id.as_str()],
                )?;
                for capability in certification.capabilities {
                    transaction.execute(
                        "INSERT INTO engine_certification_capabilities(
                             certification_id, capability, support_status,
                             limitation_text, evidence_locator, created_at_ms
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        params![
                            certification.id,
                            capability.capability,
                            capability.support_status,
                            capability.limitation_text,
                            capability.evidence_locator,
                            certification.created_at_ms,
                        ],
                    )?;
                }
            }
            transaction.execute(
                "INSERT INTO app_meta(key, value_text, updated_at_ms)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(key) DO UPDATE SET
                     value_text = excluded.value_text,
                     updated_at_ms = excluded.updated_at_ms",
                params![
                    VERIFIED_PACKAGE_MANIFEST_KEY,
                    spec.package_manifest_sha256,
                    spec.imported_at_ms,
                ],
            )?;
            transaction.commit()?;
            Ok(PackageCertificationImportOutcome {
                deduplicated: false,
                tool_count,
                certification_count,
            })
        })
    }

    pub fn record_tool(&self, spec: NativeToolSpec) -> Result<ToolObservation, RepositoryError> {
        spec.validate()?;
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let existing: Option<(String, String, i64)> = transaction
                .query_row(
                    "SELECT id, file_sha256, last_seen_at_ms
                     FROM native_tool_inventory
                     WHERE executable_path = ?1
                       AND deleted_at_ms IS NULL",
                    [spec.executable_path.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            let result = if let Some((id, old_sha256, last_seen_at_ms)) = existing {
                if spec.observed_at_ms < last_seen_at_ms {
                    return Err(RepositoryError::RuntimeStateInvalid);
                }
                let binary_changed = old_sha256 != spec.file_sha256;
                let affected = transaction.execute(
                    "UPDATE native_tool_inventory
                     SET tool_name = ?2, version_text = ?3, architecture = ?4,
                         file_sha256 = ?5, source_kind = ?6, last_seen_at_ms = ?7,
                         signature_status = CASE WHEN ?8 THEN 'not_checked' ELSE signature_status END,
                         trust_status = CASE WHEN ?8 THEN 'unreviewed' ELSE trust_status END,
                         publisher = CASE WHEN ?8 THEN NULL ELSE publisher END,
                         last_verified_at_ms = CASE WHEN ?8 THEN NULL ELSE last_verified_at_ms END,
                         updated_at_ms = ?7
                     WHERE id = ?1 AND deleted_at_ms IS NULL",
                    params![
                        id,
                        spec.tool_name,
                        spec.version_text,
                        spec.architecture,
                        spec.file_sha256,
                        spec.source_kind,
                        spec.observed_at_ms,
                        binary_changed
                    ],
                )?;
                if affected != 1 {
                    return Err(RepositoryError::RuntimeStateInvalid);
                }
                ToolObservation { id, binary_changed }
            } else {
                transaction.execute(
                    "INSERT INTO native_tool_inventory(
                         id, tool_name, executable_path, version_text, architecture,
                         file_sha256, signature_status, trust_status, source_kind,
                         first_seen_at_ms, last_seen_at_ms, created_at_ms, updated_at_ms
                     ) VALUES (
                         ?1, ?2, ?3, ?4, ?5,
                         ?6, 'not_checked', 'unreviewed', ?7,
                         ?8, ?8, ?8, ?8
                     )",
                    params![
                        spec.id,
                        spec.tool_name,
                        spec.executable_path,
                        spec.version_text,
                        spec.architecture,
                        spec.file_sha256,
                        spec.source_kind,
                        spec.observed_at_ms
                    ],
                )?;
                ToolObservation {
                    id: spec.id,
                    binary_changed: false,
                }
            };
            transaction.commit()?;
            Ok(result)
        })
    }

    pub fn trust_verified_tool(
        &self,
        tool_id: &str,
        publisher: &str,
        verified_at_ms: i64,
    ) -> Result<(), RepositoryError> {
        if tool_id.is_empty()
            || tool_id.len() > 128
            || publisher.is_empty()
            || publisher.len() > 512
            || verified_at_ms < 0
        {
            return Err(RepositoryError::CertificationInputInvalid);
        }
        let tool_id = tool_id.to_owned();
        let publisher = publisher.to_owned();
        self.write(move |connection| {
            let affected = connection.execute(
                "UPDATE native_tool_inventory
                 SET publisher = ?2, signature_status = 'valid', trust_status = 'trusted',
                     last_verified_at_ms = ?3, last_seen_at_ms = MAX(last_seen_at_ms, ?3),
                     updated_at_ms = ?3
                 WHERE id = ?1
                   AND deleted_at_ms IS NULL
                   AND first_seen_at_ms <= ?3
                   AND updated_at_ms <= ?3",
                params![tool_id, publisher, verified_at_ms],
            )?;
            if affected != 1 {
                return Err(RepositoryError::NotFound);
            }
            Ok(())
        })
    }

    pub fn tool_trust(&self, tool_id: &str) -> Result<(String, String), RepositoryError> {
        let reader = self.store.open_reader()?;
        reader
            .connection()
            .query_row(
                "SELECT trust_status, signature_status
                 FROM native_tool_inventory
                 WHERE id = ?1 AND deleted_at_ms IS NULL",
                [tool_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or(RepositoryError::NotFound)
    }

    pub fn has_trusted_tool(&self, tool_name: &str) -> Result<bool, RepositoryError> {
        if tool_name.is_empty() || tool_name.len() > 128 || tool_name.chars().any(char::is_control)
        {
            return Err(RepositoryError::CertificationInputInvalid);
        }
        let reader = self.store.open_reader()?;
        reader
            .connection()
            .query_row(
                "SELECT EXISTS(
                 SELECT 1
                 FROM native_tool_inventory
                 WHERE tool_name = ?1
                   AND trust_status = 'trusted'
                   AND signature_status = 'valid'
                   AND deleted_at_ms IS NULL
             )",
                [tool_name],
                |row| row.get(0),
            )
            .map_err(RepositoryError::from)
    }

    pub fn trusted_tool_identity_at_path(
        &self,
        tool_name: &str,
        executable_path: &str,
    ) -> Result<TrustedToolIdentity, RepositoryError> {
        if tool_name.is_empty()
            || tool_name.len() > 128
            || tool_name.chars().any(char::is_control)
            || executable_path.is_empty()
            || executable_path.len() > 2048
            || executable_path.chars().any(char::is_control)
        {
            return Err(RepositoryError::CertificationInputInvalid);
        }
        let reader = self.store.open_reader()?;
        reader
            .connection()
            .query_row(
                "SELECT tool_name, executable_path, version_text, file_sha256
                 FROM native_tool_inventory
                 WHERE tool_name = ?1
                   AND executable_path = ?2
                   AND source_kind = 'bundled'
                   AND trust_status = 'trusted'
                   AND signature_status = 'valid'
                   AND deleted_at_ms IS NULL",
                params![tool_name, executable_path],
                |row| {
                    Ok(TrustedToolIdentity {
                        tool_name: row.get(0)?,
                        executable_path: row.get(1)?,
                        version_text: row.get(2)?,
                        file_sha256: row.get(3)?,
                    })
                },
            )
            .optional()?
            .ok_or(RepositoryError::NotFound)
    }

    pub fn verified_package_manifest_sha256(&self) -> Result<String, RepositoryError> {
        let reader = self.store.open_reader()?;
        reader
            .connection()
            .query_row(
                "SELECT value_text FROM app_meta WHERE key = ?1",
                [VERIFIED_PACKAGE_MANIFEST_KEY],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .filter(|value| is_digest(value))
            .ok_or(RepositoryError::NotFound)
    }

    pub fn insert_candidate(&self, spec: EngineCertificationSpec) -> Result<(), RepositoryError> {
        spec.validate()?;
        self.write(move |connection| {
            let transaction = connection.transaction()?;
            transaction.execute(
                "INSERT INTO engine_certifications(
                     id, engine, server_version_range, tool_name, tool_version_range,
                     operating_system, architecture, dump_format, consistency_grade,
                     status, evidence_locator, evidence_checksum_blake3,
                     created_at_ms, updated_at_ms
                 ) VALUES (
                     ?1, ?2, ?3, ?4, ?5,
                     ?6, ?7, ?8, ?9,
                     'candidate', ?10, ?11,
                     ?12, ?12
                 )",
                params![
                    spec.id,
                    spec.engine,
                    spec.server_version_range,
                    spec.tool_name,
                    spec.tool_version_range,
                    spec.operating_system,
                    spec.architecture,
                    spec.dump_format,
                    spec.consistency_grade,
                    spec.evidence_locator,
                    spec.evidence_checksum_blake3,
                    spec.created_at_ms
                ],
            )?;
            for capability in spec.capabilities {
                transaction.execute(
                    "INSERT INTO engine_certification_capabilities(
                         certification_id, capability, support_status,
                         limitation_text, evidence_locator, created_at_ms
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        spec.id,
                        capability.capability,
                        capability.support_status,
                        capability.limitation_text,
                        capability.evidence_locator,
                        spec.created_at_ms
                    ],
                )?;
            }
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn certify(
        &self,
        certification_id: &str,
        certified_at_ms: i64,
        expires_at_ms: i64,
    ) -> Result<(), RepositoryError> {
        if certification_id.is_empty() || certified_at_ms < 0 || expires_at_ms <= certified_at_ms {
            return Err(RepositoryError::CertificationInputInvalid);
        }
        let certification_id = certification_id.to_owned();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let capability_count: i64 = transaction.query_row(
                "SELECT COUNT(*)
                 FROM engine_certification_capabilities
                 WHERE certification_id = ?1",
                [certification_id.as_str()],
                |row| row.get(0),
            )?;
            if capability_count != REQUIRED_PROVIDER_CAPABILITY_COUNT {
                return Err(RepositoryError::CertificationInputInvalid);
            }
            let affected = transaction.execute(
                "UPDATE engine_certifications
                 SET status = 'certified', certified_at_ms = ?2, expires_at_ms = ?3,
                     updated_at_ms = ?2
                 WHERE id = ?1
                   AND status = 'candidate'
                   AND created_at_ms <= ?2",
                params![certification_id, certified_at_ms, expires_at_ms],
            )?;
            if affected != 1 {
                return Err(RepositoryError::RuntimeStateInvalid);
            }
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn effective_capability(
        &self,
        engine: &str,
        capability: &str,
        now_ms: i64,
    ) -> Result<Option<EffectiveCapability>, RepositoryError> {
        if now_ms < 0
            || !ProviderCapability::REQUIRED
                .iter()
                .any(|known| known.as_str() == capability)
        {
            return Err(RepositoryError::CertificationInputInvalid);
        }
        let reader = self.store.open_reader()?;
        reader
            .connection()
            .query_row(
                "SELECT capabilities.support_status, capabilities.limitation_text
                 FROM engine_certifications AS certifications
                 JOIN engine_certification_capabilities AS capabilities
                   ON capabilities.certification_id = certifications.id
                 WHERE certifications.engine = ?1
                   AND capabilities.capability = ?2
                   AND capabilities.support_status IN ('passed', 'limited')
                   AND certifications.status = 'certified'
                   AND certifications.certified_at_ms <= ?3
                   AND (certifications.expires_at_ms IS NULL OR certifications.expires_at_ms > ?3)
                 ORDER BY certifications.certified_at_ms DESC, certifications.id
                 LIMIT 1",
                params![engine, capability, now_ms],
                |row| {
                    Ok(EffectiveCapability {
                        support_status: row.get(0)?,
                        limitation_text: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(RepositoryError::from)
    }

    fn write<F, T>(&self, job: F) -> Result<T, RepositoryError>
    where
        F: FnOnce(&mut rusqlite::Connection) -> Result<T, RepositoryError> + Send + 'static,
        T: Send + 'static,
    {
        self.store.run_write(job).map_err(RepositoryError::from)?
    }
}

fn verified_package_projection_matches(
    transaction: &Transaction<'_>,
    spec: &VerifiedPackageCertificationSpec,
) -> Result<bool, rusqlite::Error> {
    let active_tool_count: i64 = transaction.query_row(
        "SELECT COUNT(*)
         FROM native_tool_inventory
         WHERE source_kind = 'bundled'
           AND signature_status = 'valid'
           AND trust_status = 'trusted'
           AND deleted_at_ms IS NULL",
        [],
        |row| row.get(0),
    )?;
    if active_tool_count != spec.tools.len() as i64 {
        return Ok(false);
    }
    for (tool, publisher) in &spec.tools {
        let matches: bool = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1
                 FROM native_tool_inventory
                 WHERE id = ?1
                   AND tool_name = ?2
                   AND executable_path = ?3
                   AND version_text = ?4
                   AND architecture = ?5
                   AND file_sha256 = ?6
                   AND publisher = ?7
                   AND signature_status = 'valid'
                   AND trust_status = 'trusted'
                   AND source_kind = 'bundled'
                   AND deleted_at_ms IS NULL
             )",
            params![
                tool.id,
                tool.tool_name,
                tool.executable_path,
                tool.version_text,
                tool.architecture,
                tool.file_sha256,
                publisher,
            ],
            |row| row.get(0),
        )?;
        if !matches {
            return Ok(false);
        }
    }
    let certified_count: i64 = transaction.query_row(
        "SELECT COUNT(*)
         FROM engine_certifications
         WHERE evidence_locator LIKE 'package://provider-certifications.json#/%'
           AND status = 'certified'",
        [],
        |row| row.get(0),
    )?;
    if certified_count != spec.certifications.len() as i64 {
        return Ok(false);
    }
    for (certification, certified_at_ms, expires_at_ms) in &spec.certifications {
        let matches: bool = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1
                 FROM engine_certifications
                 WHERE id = ?1
                   AND engine = ?2
                   AND server_version_range = ?3
                   AND tool_name = ?4
                   AND tool_version_range = ?5
                   AND operating_system = ?6
                   AND architecture = ?7
                   AND dump_format = ?8
                   AND consistency_grade = ?9
                   AND status = 'certified'
                   AND evidence_locator = ?10
                   AND evidence_checksum_blake3 = ?11
                   AND certified_at_ms = ?12
                   AND expires_at_ms = ?13
             )",
            params![
                certification.id,
                certification.engine,
                certification.server_version_range,
                certification.tool_name,
                certification.tool_version_range,
                certification.operating_system,
                certification.architecture,
                certification.dump_format,
                certification.consistency_grade,
                certification.evidence_locator,
                certification.evidence_checksum_blake3,
                certified_at_ms,
                expires_at_ms,
            ],
            |row| row.get(0),
        )?;
        if !matches {
            return Ok(false);
        }
        let capability_count: i64 = transaction.query_row(
            "SELECT COUNT(*)
             FROM engine_certification_capabilities
             WHERE certification_id = ?1",
            [certification.id.as_str()],
            |row| row.get(0),
        )?;
        if capability_count != certification.capabilities.len() as i64 {
            return Ok(false);
        }
        for capability in &certification.capabilities {
            let matches: bool = transaction.query_row(
                "SELECT EXISTS(
                     SELECT 1
                     FROM engine_certification_capabilities
                     WHERE certification_id = ?1
                       AND capability = ?2
                       AND support_status = ?3
                       AND limitation_text IS ?4
                       AND evidence_locator = ?5
                 )",
                params![
                    certification.id,
                    capability.capability,
                    capability.support_status,
                    capability.limitation_text,
                    capability.evidence_locator,
                ],
                |row| row.get(0),
            )?;
            if !matches {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
