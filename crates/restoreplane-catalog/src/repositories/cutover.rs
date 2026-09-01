use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::OperationalStore;

use super::shadow_import::verify_shadow_import_for_cutover;
use super::RepositoryError;

const AUTHORITY_META_KEY: &str = "execution_authority.v1";
const PRODUCTION_EPOCH_META_KEY: &str = "production_epoch.v1";
const MINIMUM_READER_META_KEY: &str = "minimum_reader_version.v1";
const ROLLBACK_ELIGIBLE_VALUE: &str = "rollback_eligible";
const FORWARD_FIX_REQUIRED_PREFIX: &str = "forward_fix_required:";
const CONFIRMATION_RESOURCE_TYPE: &str = "config_import_run";
const CUTOVER_MARKER_PREFIX: &str = "config-cutover-v1:";
const STANDALONE_MARKER_PREFIX: &str = "standalone-bootstrap-v1:";
const STANDALONE_AUTHORITY_KIND: &str = "standalone_install";
const STANDALONE_EPOCH_VALUE: &str = "forward_fix_required:standalone";
const EXPECTED_STANDALONE_TABLES: usize = 67;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PersistedExecutionAuthority {
    LegacyDirect,
    Service,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PersistedCutoverMode {
    RehearsalOnly,
    Production,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PersistedCutoverRecoveryDisposition {
    RollbackEligible,
    ForwardFixRequired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PersistedAuthorityOrigin {
    Rehearsal,
    LegacyCutover,
    StandaloneInstall,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigCutoverRollbackReason {
    ServiceStartFailed,
    PostCutoverHealthFailed,
    OperatorRequested,
}

impl ConfigCutoverRollbackReason {
    const fn code(self) -> &'static str {
        match self {
            Self::ServiceStartFailed => "service_start_failed",
            Self::PostCutoverHealthFailed => "post_cutover_health_failed",
            Self::OperatorRequested => "operator_requested",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigCutoverRollbackSpec {
    rollback_id: Uuid,
    cutover_marker: String,
    recovery_bundle_digest_blake3: String,
    legacy_scheduler_restore_digest_blake3: String,
    requested_by_sid_hash: String,
    reason: ConfigCutoverRollbackReason,
    rolled_back_at_ms: i64,
}

impl ConfigCutoverRollbackSpec {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        rollback_id: Uuid,
        cutover_marker: impl Into<String>,
        recovery_bundle_digest_blake3: impl Into<String>,
        legacy_scheduler_restore_digest_blake3: impl Into<String>,
        requested_by_sid_hash: impl Into<String>,
        reason: ConfigCutoverRollbackReason,
        rolled_back_at_ms: i64,
    ) -> Self {
        Self {
            rollback_id,
            cutover_marker: cutover_marker.into(),
            recovery_bundle_digest_blake3: recovery_bundle_digest_blake3.into(),
            legacy_scheduler_restore_digest_blake3: legacy_scheduler_restore_digest_blake3.into(),
            requested_by_sid_hash: requested_by_sid_hash.into(),
            reason,
            rolled_back_at_ms,
        }
    }

    pub const fn rollback_id(&self) -> Uuid {
        self.rollback_id
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if !is_cutover_marker(&self.cutover_marker)
            || !is_digest(&self.recovery_bundle_digest_blake3)
            || !is_digest(&self.legacy_scheduler_restore_digest_blake3)
            || !is_digest(&self.requested_by_sid_hash)
            || self.rolled_back_at_ms < 0
        {
            return Err(RepositoryError::CutoverRollbackInputInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigCutoverRollbackResult {
    rollback_id: Uuid,
    cutover_marker: String,
    created: bool,
}

impl ConfigCutoverRollbackResult {
    fn new(rollback_id: Uuid, cutover_marker: String, created: bool) -> Self {
        Self {
            rollback_id,
            cutover_marker,
            created,
        }
    }

    pub const fn rollback_id(&self) -> Uuid {
        self.rollback_id
    }

    pub fn cutover_marker(&self) -> &str {
        &self.cutover_marker
    }

    pub const fn created(&self) -> bool {
        self.created
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PersistedAuthoritySnapshot {
    origin: PersistedAuthorityOrigin,
    execution_authority: PersistedExecutionAuthority,
    cutover_mode: PersistedCutoverMode,
    g1_source_revision: Option<String>,
    g1_source_tree_digest: Option<String>,
    g1_build_id: Option<String>,
    cutover_marker: Option<String>,
    recovery_disposition: Option<PersistedCutoverRecoveryDisposition>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MinimumReaderVersionSnapshot {
    minimum_reader_version: String,
    cutover_marker: String,
    updated_at_ms: i64,
}

impl MinimumReaderVersionSnapshot {
    pub fn minimum_reader_version(&self) -> &str {
        &self.minimum_reader_version
    }

    pub fn cutover_marker(&self) -> &str {
        &self.cutover_marker
    }

    pub const fn updated_at_ms(&self) -> i64 {
        self.updated_at_ms
    }
}

impl PersistedAuthoritySnapshot {
    fn rehearsal() -> Self {
        Self {
            origin: PersistedAuthorityOrigin::Rehearsal,
            execution_authority: PersistedExecutionAuthority::LegacyDirect,
            cutover_mode: PersistedCutoverMode::RehearsalOnly,
            g1_source_revision: None,
            g1_source_tree_digest: None,
            g1_build_id: None,
            cutover_marker: None,
            recovery_disposition: None,
        }
    }

    pub const fn origin(&self) -> PersistedAuthorityOrigin {
        self.origin
    }

    pub const fn execution_authority(&self) -> PersistedExecutionAuthority {
        self.execution_authority
    }

    pub const fn cutover_mode(&self) -> PersistedCutoverMode {
        self.cutover_mode
    }

    pub const fn g1_proven(&self) -> bool {
        self.g1_source_revision.is_some()
            && self.g1_source_tree_digest.is_some()
            && self.g1_build_id.is_some()
    }

    pub fn g1_source_revision(&self) -> Option<&str> {
        self.g1_source_revision.as_deref()
    }

    pub fn g1_source_tree_digest(&self) -> Option<&str> {
        self.g1_source_tree_digest.as_deref()
    }

    pub fn g1_build_id(&self) -> Option<&str> {
        self.g1_build_id.as_deref()
    }

    pub fn cutover_marker(&self) -> Option<&str> {
        self.cutover_marker.as_deref()
    }

    pub const fn recovery_disposition(&self) -> Option<PersistedCutoverRecoveryDisposition> {
        self.recovery_disposition
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StandaloneAuthorityBootstrapSpec {
    installation_id: Uuid,
    source_revision: String,
    package_manifest_digest_sha256: String,
    build_id: String,
    service_binary_sha256: String,
    committed_at_ms: i64,
}

impl StandaloneAuthorityBootstrapSpec {
    pub fn new(
        installation_id: Uuid,
        source_revision: impl Into<String>,
        package_manifest_digest_sha256: impl Into<String>,
        build_id: impl Into<String>,
        service_binary_sha256: impl Into<String>,
        committed_at_ms: i64,
    ) -> Self {
        Self {
            installation_id,
            source_revision: source_revision.into(),
            package_manifest_digest_sha256: package_manifest_digest_sha256.into(),
            build_id: build_id.into(),
            service_binary_sha256: service_binary_sha256.into(),
            committed_at_ms,
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if self.installation_id.is_nil()
            || !is_source_revision(&self.source_revision)
            || !is_digest(&self.package_manifest_digest_sha256)
            || !is_digest(&self.build_id)
            || !is_digest(&self.service_binary_sha256)
            || self.committed_at_ms < 0
        {
            return Err(RepositoryError::StandaloneBootstrapInputInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StandaloneAuthorityBootstrapResult {
    authority_marker: String,
    created: bool,
}

impl StandaloneAuthorityBootstrapResult {
    pub fn authority_marker(&self) -> &str {
        &self.authority_marker
    }

    pub const fn created(&self) -> bool {
        self.created
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct G1EvidenceBindingV1 {
    source_revision: String,
    source_tree_digest: String,
    build_id: String,
    evidence_digest_blake3: String,
}

impl G1EvidenceBindingV1 {
    pub fn new(
        source_revision: impl Into<String>,
        source_tree_digest: impl Into<String>,
        build_id: impl Into<String>,
        evidence_digest_blake3: impl Into<String>,
    ) -> Self {
        Self {
            source_revision: source_revision.into(),
            source_tree_digest: source_tree_digest.into(),
            build_id: build_id.into(),
            evidence_digest_blake3: evidence_digest_blake3.into(),
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if !is_source_revision(&self.source_revision)
            || !is_digest(&self.source_tree_digest)
            || !is_digest(&self.build_id)
            || !is_digest(&self.evidence_digest_blake3)
        {
            return Err(RepositoryError::CutoverInputInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ConfigCutoverSpec {
    cutover_run_id: Uuid,
    shadow_import_id: Uuid,
    source_checksum_blake3: String,
    relation_digest_blake3: String,
    recovery_bundle_digest_blake3: String,
    legacy_scheduler_freeze_digest_blake3: String,
    service_health_digest_blake3: String,
    g1_evidence: G1EvidenceBindingV1,
    cutover_marker: String,
    committed_at_ms: i64,
}

impl ConfigCutoverSpec {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cutover_run_id: Uuid,
        shadow_import_id: Uuid,
        source_checksum_blake3: impl Into<String>,
        relation_digest_blake3: impl Into<String>,
        recovery_bundle_digest_blake3: impl Into<String>,
        legacy_scheduler_freeze_digest_blake3: impl Into<String>,
        service_health_digest_blake3: impl Into<String>,
        g1_evidence: G1EvidenceBindingV1,
        committed_at_ms: i64,
    ) -> Self {
        let mut spec = Self {
            cutover_run_id,
            shadow_import_id,
            source_checksum_blake3: source_checksum_blake3.into(),
            relation_digest_blake3: relation_digest_blake3.into(),
            recovery_bundle_digest_blake3: recovery_bundle_digest_blake3.into(),
            legacy_scheduler_freeze_digest_blake3: legacy_scheduler_freeze_digest_blake3.into(),
            service_health_digest_blake3: service_health_digest_blake3.into(),
            g1_evidence,
            cutover_marker: String::new(),
            committed_at_ms,
        };
        spec.cutover_marker = marker_for_spec(&spec);
        spec
    }

    pub const fn confirmation_resource_type(&self) -> &'static str {
        CONFIRMATION_RESOURCE_TYPE
    }

    pub fn cutover_marker(&self) -> &str {
        &self.cutover_marker
    }

    pub const fn committed_at_ms(&self) -> i64 {
        self.committed_at_ms
    }

    pub(super) fn validate(
        &self,
        accepted_at_ms: i64,
        request_expires_at_ms: i64,
    ) -> Result<(), RepositoryError> {
        self.g1_evidence.validate()?;
        if !is_digest(&self.source_checksum_blake3)
            || !is_digest(&self.relation_digest_blake3)
            || !is_digest(&self.recovery_bundle_digest_blake3)
            || !is_digest(&self.legacy_scheduler_freeze_digest_blake3)
            || !is_digest(&self.service_health_digest_blake3)
            || self.committed_at_ms < accepted_at_ms
            || self.committed_at_ms >= request_expires_at_ms
            || self.cutover_marker != marker_for_spec(self)
        {
            return Err(RepositoryError::CutoverInputInvalid);
        }
        Ok(())
    }

    pub(super) fn shadow_import_id(&self) -> String {
        self.shadow_import_id.to_string()
    }

    pub(super) fn source_checksum_blake3(&self) -> &str {
        &self.source_checksum_blake3
    }

    pub(super) fn relation_digest_blake3(&self) -> &str {
        &self.relation_digest_blake3
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigCutoverResult {
    cutover_marker: String,
    created: bool,
}

impl ConfigCutoverResult {
    pub(super) fn new(cutover_marker: String, created: bool) -> Self {
        Self {
            cutover_marker,
            created,
        }
    }

    pub fn cutover_marker(&self) -> &str {
        &self.cutover_marker
    }

    pub const fn created(&self) -> bool {
        self.created
    }
}

#[derive(Clone)]
pub struct ConfigCutoverRepository {
    store: OperationalStore,
}

impl OperationalStore {
    pub fn cutover(&self) -> ConfigCutoverRepository {
        ConfigCutoverRepository {
            store: self.clone(),
        }
    }
}

impl ConfigCutoverRepository {
    pub fn authority(&self) -> Result<PersistedAuthoritySnapshot, RepositoryError> {
        let reader = self.store.open_reader()?;
        load_authority(reader.connection())
    }

    pub fn bootstrap_standalone_authority(
        &self,
        spec: StandaloneAuthorityBootstrapSpec,
    ) -> Result<StandaloneAuthorityBootstrapResult, RepositoryError> {
        spec.validate()?;
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let current = load_authority(&transaction)?;
            if current.execution_authority() != PersistedExecutionAuthority::LegacyDirect {
                return Err(RepositoryError::StandaloneBootstrapPreconditionFailed);
            }
            ensure_pristine_standalone_store(&transaction)?;

            let document = standalone_document_from_spec(&spec);
            validate_standalone_document(&document)?;
            let document_json = serde_json::to_string(&document)
                .map_err(|_| RepositoryError::StandaloneBootstrapInputInvalid)?;
            let minimum_reader = MinimumReaderDocumentV1 {
                schema_version: 1,
                minimum_reader_version: env!("CARGO_PKG_VERSION").to_owned(),
                cutover_marker: document.authority_marker.clone(),
                updated_at_ms: document.committed_at_ms,
            };
            validate_minimum_reader_document(&minimum_reader)?;
            let minimum_reader_json = serde_json::to_string(&minimum_reader)
                .map_err(|_| RepositoryError::StandaloneBootstrapInputInvalid)?;

            transaction.execute(
                "INSERT INTO app_meta(key, value_text, updated_at_ms)
                 VALUES (?1, ?2, ?3)",
                params![AUTHORITY_META_KEY, document_json, document.committed_at_ms],
            )?;
            transaction.execute(
                "INSERT INTO app_meta(key, value_text, updated_at_ms)
                 VALUES (?1, ?2, ?3)",
                params![
                    PRODUCTION_EPOCH_META_KEY,
                    STANDALONE_EPOCH_VALUE,
                    document.committed_at_ms
                ],
            )?;
            transaction.execute(
                "INSERT INTO app_meta(key, value_text, updated_at_ms)
                 VALUES (?1, ?2, ?3)",
                params![
                    MINIMUM_READER_META_KEY,
                    minimum_reader_json,
                    document.committed_at_ms
                ],
            )?;

            let persisted = load_authority(&transaction)?;
            if persisted.origin() != PersistedAuthorityOrigin::StandaloneInstall
                || persisted.cutover_marker() != Some(document.authority_marker.as_str())
            {
                return Err(RepositoryError::CutoverStateInvalid);
            }
            transaction.commit()?;
            Ok(StandaloneAuthorityBootstrapResult {
                authority_marker: document.authority_marker,
                created: true,
            })
        })
    }

    pub fn minimum_reader_version(
        &self,
    ) -> Result<Option<MinimumReaderVersionSnapshot>, RepositoryError> {
        let reader = self.store.open_reader()?;
        load_minimum_reader(reader.connection())
    }

    pub fn advance_minimum_reader_version(
        &self,
        minimum_reader_version: impl Into<String>,
        cutover_marker: impl Into<String>,
        updated_at_ms: i64,
    ) -> Result<bool, RepositoryError> {
        let minimum_reader_version = minimum_reader_version.into();
        let cutover_marker = cutover_marker.into();
        if parse_reader_version(&minimum_reader_version).is_none()
            || !is_authority_marker(&cutover_marker)
            || updated_at_ms < 0
        {
            return Err(RepositoryError::MinimumReaderInputInvalid);
        }
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let authority = load_authority(&transaction)?;
            if authority.execution_authority() != PersistedExecutionAuthority::Service
                || authority.cutover_marker() != Some(cutover_marker.as_str())
            {
                return Err(RepositoryError::MinimumReaderInputInvalid);
            }
            let current =
                load_minimum_reader(&transaction)?.ok_or(RepositoryError::CutoverStateInvalid)?;
            let current_version = parse_reader_version(current.minimum_reader_version())
                .ok_or(RepositoryError::CutoverStateInvalid)?;
            let next_version = parse_reader_version(&minimum_reader_version)
                .ok_or(RepositoryError::MinimumReaderInputInvalid)?;
            if next_version < current_version || updated_at_ms < current.updated_at_ms {
                return Err(RepositoryError::MinimumReaderRegression);
            }
            if next_version == current_version {
                transaction.commit()?;
                return Ok(false);
            }
            let previous_document = MinimumReaderDocumentV1::from_snapshot(&current);
            let previous_json = serde_json::to_string(&previous_document)
                .map_err(|_| RepositoryError::CutoverStateInvalid)?;
            let document = MinimumReaderDocumentV1 {
                schema_version: 1,
                minimum_reader_version,
                cutover_marker,
                updated_at_ms,
            };
            validate_minimum_reader_document(&document)?;
            let value_text = serde_json::to_string(&document)
                .map_err(|_| RepositoryError::CutoverStateInvalid)?;
            let updated = transaction.execute(
                "UPDATE app_meta
                 SET value_text = ?2, updated_at_ms = ?3
                 WHERE key = ?1 AND value_text = ?4 AND updated_at_ms = ?5",
                params![
                    MINIMUM_READER_META_KEY,
                    value_text,
                    updated_at_ms,
                    previous_json,
                    current.updated_at_ms
                ],
            )?;
            if updated != 1 {
                return Err(RepositoryError::CutoverStateInvalid);
            }
            transaction.commit()?;
            Ok(true)
        })
    }

    pub fn require_legacy_direct_execution(&self) -> Result<(), RepositoryError> {
        let authority = self.authority()?;
        if authority.execution_authority() == PersistedExecutionAuthority::Service {
            return Err(RepositoryError::LegacyDirectExecutionBlocked);
        }
        Ok(())
    }

    pub fn rollback_if_clean(
        &self,
        spec: ConfigCutoverRollbackSpec,
    ) -> Result<ConfigCutoverRollbackResult, RepositoryError> {
        spec.validate()?;
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let authority = load_authority(&transaction)?;
            if authority.execution_authority() == PersistedExecutionAuthority::LegacyDirect {
                let existing = load_rollback_audit(&transaction, spec.rollback_id)?;
                let Some(existing) = existing else {
                    return Err(RepositoryError::CutoverRollbackPreconditionFailed);
                };
                validate_rollback_replay(&existing, &spec)?;
                transaction.commit()?;
                return Ok(ConfigCutoverRollbackResult::new(
                    spec.rollback_id,
                    spec.cutover_marker,
                    false,
                ));
            }
            if authority.cutover_marker() != Some(spec.cutover_marker.as_str()) {
                return Err(RepositoryError::CutoverRollbackPreconditionFailed);
            }
            if authority.recovery_disposition()
                != Some(PersistedCutoverRecoveryDisposition::RollbackEligible)
            {
                return Err(RepositoryError::CutoverForwardFixRequired);
            }

            let (authority_json, authority_updated_at_ms): (String, i64) = transaction.query_row(
                "SELECT value_text, updated_at_ms
                     FROM app_meta
                     WHERE key = ?1",
                [AUTHORITY_META_KEY],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let original: CutoverAuditDocumentV1 = serde_json::from_str(&authority_json)
                .map_err(|_| RepositoryError::CutoverStateInvalid)?;
            validate_document(&original)?;
            if original.cutover_marker != spec.cutover_marker
                || original.recovery_bundle_digest_blake3 != spec.recovery_bundle_digest_blake3
                || spec.rolled_back_at_ms < original.committed_at_ms
                || authority_updated_at_ms != original.committed_at_ms
            {
                return Err(RepositoryError::CutoverRollbackPreconditionFailed);
            }
            if load_rollback_audit(&transaction, spec.rollback_id)?.is_some() {
                return Err(RepositoryError::CutoverRollbackConflict);
            }

            let audit = CutoverRollbackAuditDocumentV1 {
                schema_version: 1,
                rollback_id: spec.rollback_id.to_string(),
                cutover_marker: spec.cutover_marker.clone(),
                cutover_run_id: original.cutover_run_id.clone(),
                shadow_import_id: original.shadow_import_id.clone(),
                recovery_bundle_digest_blake3: spec.recovery_bundle_digest_blake3.clone(),
                legacy_scheduler_restore_digest_blake3: spec
                    .legacy_scheduler_restore_digest_blake3
                    .clone(),
                requested_by_sid_hash: spec.requested_by_sid_hash.clone(),
                reason_code: spec.reason.code().to_owned(),
                rolled_back_at_ms: spec.rolled_back_at_ms,
                original_cutover: original.clone(),
            };
            validate_rollback_document(&audit)?;
            let audit_json = serde_json::to_string(&audit)
                .map_err(|_| RepositoryError::CutoverRollbackInputInvalid)?;

            let authority_deleted = transaction.execute(
                "DELETE FROM app_meta
                 WHERE key = ?1 AND value_text = ?2 AND updated_at_ms = ?3",
                params![AUTHORITY_META_KEY, authority_json, original.committed_at_ms],
            )?;
            let epoch_deleted = transaction.execute(
                "DELETE FROM app_meta
                 WHERE key = ?1
                   AND value_text = ?2
                   AND updated_at_ms = ?3",
                params![
                    PRODUCTION_EPOCH_META_KEY,
                    ROLLBACK_ELIGIBLE_VALUE,
                    original.committed_at_ms
                ],
            )?;
            let minimum_reader_deleted = transaction.execute(
                "DELETE FROM app_meta
                 WHERE key = ?1
                   AND json_extract(value_text, '$.cutoverMarker') = ?2",
                params![MINIMUM_READER_META_KEY, spec.cutover_marker],
            )?;
            let audit_updated = transaction.execute(
                "UPDATE config_import_runs
                 SET mode = 'shadow',
                     status = 'cancelled',
                     diff_json = ?2,
                     completed_at_ms = ?3,
                     cutover_marker = NULL,
                     confirmation_token_id = NULL,
                     error_code = 'CONFIG_CUTOVER_ROLLED_BACK'
                 WHERE id = ?1
                   AND mode = 'cutover'
                   AND status = 'succeeded'
                   AND cutover_marker = ?4",
                params![
                    original.cutover_run_id,
                    audit_json,
                    spec.rolled_back_at_ms,
                    spec.cutover_marker
                ],
            )?;
            if authority_deleted != 1
                || epoch_deleted != 1
                || minimum_reader_deleted != 1
                || audit_updated != 1
            {
                return Err(RepositoryError::CutoverStateInvalid);
            }
            let rolled_back_authority = load_authority(&transaction)?;
            if rolled_back_authority.execution_authority()
                != PersistedExecutionAuthority::LegacyDirect
                || rolled_back_authority.cutover_mode() != PersistedCutoverMode::RehearsalOnly
            {
                return Err(RepositoryError::CutoverStateInvalid);
            }
            transaction.commit()?;
            Ok(ConfigCutoverRollbackResult::new(
                spec.rollback_id,
                spec.cutover_marker,
                true,
            ))
        })
    }

    fn write<F, T>(&self, job: F) -> Result<T, RepositoryError>
    where
        F: FnOnce(&mut Connection) -> Result<T, RepositoryError> + Send + 'static,
        T: Send + 'static,
    {
        self.store.run_write(job).map_err(RepositoryError::from)?
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct CutoverAuditDocumentV1 {
    schema_version: u32,
    execution_authority: String,
    cutover_mode: String,
    cutover_run_id: String,
    shadow_import_id: String,
    source_checksum_blake3: String,
    relation_digest_blake3: String,
    recovery_bundle_digest_blake3: String,
    legacy_scheduler_freeze_digest_blake3: String,
    service_health_digest_blake3: String,
    g1_source_revision: String,
    g1_source_tree_digest: String,
    g1_build_id: String,
    g1_evidence_digest_blake3: String,
    request_id: String,
    request_payload_digest_blake3: String,
    confirmation_token_id: String,
    cutover_marker: String,
    committed_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct StandaloneAuthorityDocumentV1 {
    schema_version: u32,
    authority_kind: String,
    execution_authority: String,
    cutover_mode: String,
    installation_id: String,
    source_revision: String,
    package_manifest_digest_sha256: String,
    build_id: String,
    service_binary_sha256: String,
    bootstrap_binary_version: String,
    authority_marker: String,
    committed_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct MinimumReaderDocumentV1 {
    schema_version: u32,
    minimum_reader_version: String,
    cutover_marker: String,
    updated_at_ms: i64,
}

impl MinimumReaderDocumentV1 {
    fn from_snapshot(snapshot: &MinimumReaderVersionSnapshot) -> Self {
        Self {
            schema_version: 1,
            minimum_reader_version: snapshot.minimum_reader_version.clone(),
            cutover_marker: snapshot.cutover_marker.clone(),
            updated_at_ms: snapshot.updated_at_ms,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct CutoverRollbackAuditDocumentV1 {
    schema_version: u32,
    rollback_id: String,
    cutover_marker: String,
    cutover_run_id: String,
    shadow_import_id: String,
    recovery_bundle_digest_blake3: String,
    legacy_scheduler_restore_digest_blake3: String,
    requested_by_sid_hash: String,
    reason_code: String,
    rolled_back_at_ms: i64,
    original_cutover: CutoverAuditDocumentV1,
}

struct CutoverRow {
    id: String,
    source_kind: String,
    source_checksum_blake3: String,
    status: String,
    diff_json: String,
    started_at_ms: i64,
    completed_at_ms: Option<i64>,
    cutover_marker: Option<String>,
    confirmation_token_id: Option<String>,
    created_at_ms: i64,
}

struct ConfirmationLedgerRow {
    action: String,
    client_sid_hash: String,
    resource_type: String,
    resource_id: String,
    resource_fingerprint: String,
    confirmation_payload_digest: String,
    confirmation_state: String,
    confirmation_issued_at_ms: i64,
    confirmation_expires_at_ms: i64,
    consumed_at_ms: Option<i64>,
    consumed_by_request_id: Option<String>,
    ledger_request_id: String,
    command_name: String,
    ledger_client_sid_hash: String,
    ledger_payload_digest: String,
    ledger_state: String,
    ledger_accepted_at_ms: i64,
    ledger_expires_at_ms: i64,
    result_ref_type: Option<String>,
    result_ref_id: Option<String>,
    response_json: Option<String>,
    completed_at_ms: Option<i64>,
}

pub(super) fn ensure_cutover_not_committed(connection: &Connection) -> Result<(), RepositoryError> {
    let cutover_rows: i64 = connection.query_row(
        "SELECT COUNT(*)
         FROM config_import_runs
         WHERE mode = 'cutover' OR cutover_marker IS NOT NULL",
        [],
        |row| row.get(0),
    )?;
    let meta_exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM app_meta WHERE key = ?1)",
        [AUTHORITY_META_KEY],
        |row| row.get(0),
    )?;
    let epoch_exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM app_meta WHERE key = ?1)",
        [PRODUCTION_EPOCH_META_KEY],
        |row| row.get(0),
    )?;
    let minimum_reader_exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM app_meta WHERE key = ?1)",
        [MINIMUM_READER_META_KEY],
        |row| row.get(0),
    )?;
    match (
        cutover_rows,
        meta_exists,
        epoch_exists,
        minimum_reader_exists,
    ) {
        (0, false, false, false) => Ok(()),
        (0, true, true, true) => {
            let authority = load_authority(connection)?;
            if authority.origin() == PersistedAuthorityOrigin::StandaloneInstall {
                Err(RepositoryError::CutoverAlreadyCommitted)
            } else {
                Err(RepositoryError::CutoverStateInvalid)
            }
        }
        (1, true, true, true) => {
            load_authority(connection)?;
            Err(RepositoryError::CutoverAlreadyCommitted)
        }
        _ => Err(RepositoryError::CutoverStateInvalid),
    }
}

pub(super) fn insert_cutover_state(
    transaction: &Transaction<'_>,
    request_id: &str,
    request_payload_digest_blake3: &str,
    request_accepted_at_ms: i64,
    confirmation_token_id: &str,
    spec: &ConfigCutoverSpec,
) -> Result<(), RepositoryError> {
    let document = document_from_spec(
        request_id,
        request_payload_digest_blake3,
        confirmation_token_id,
        spec,
    );
    validate_document(&document)?;
    let document_json =
        serde_json::to_string(&document).map_err(|_| RepositoryError::CutoverStateInvalid)?;
    transaction.execute(
        "INSERT INTO config_import_runs(
             id, source_kind, source_checksum_blake3, mode, status,
             diff_json, started_at_ms, completed_at_ms, cutover_marker,
             confirmation_token_id, error_code, created_at_ms
         ) VALUES (
             ?1, 'legacy_toml', ?2, 'cutover', 'succeeded',
             ?3, ?4, ?5, ?6,
             ?7, NULL, ?4
         )",
        params![
            spec.cutover_run_id.to_string(),
            spec.source_checksum_blake3,
            document_json,
            request_accepted_at_ms,
            spec.committed_at_ms,
            spec.cutover_marker,
            confirmation_token_id,
        ],
    )?;
    transaction.execute(
        "INSERT INTO app_meta(key, value_text, updated_at_ms)
         VALUES (?1, ?2, ?3)",
        params![AUTHORITY_META_KEY, document_json, spec.committed_at_ms],
    )?;
    transaction.execute(
        "INSERT INTO app_meta(key, value_text, updated_at_ms)
         VALUES (?1, ?2, ?3)",
        params![
            PRODUCTION_EPOCH_META_KEY,
            ROLLBACK_ELIGIBLE_VALUE,
            spec.committed_at_ms
        ],
    )?;
    let minimum_reader = MinimumReaderDocumentV1 {
        schema_version: 1,
        minimum_reader_version: env!("CARGO_PKG_VERSION").to_owned(),
        cutover_marker: spec.cutover_marker.clone(),
        updated_at_ms: spec.committed_at_ms,
    };
    validate_minimum_reader_document(&minimum_reader)?;
    let minimum_reader_json =
        serde_json::to_string(&minimum_reader).map_err(|_| RepositoryError::CutoverStateInvalid)?;
    transaction.execute(
        "INSERT INTO app_meta(key, value_text, updated_at_ms)
         VALUES (?1, ?2, ?3)",
        params![
            MINIMUM_READER_META_KEY,
            minimum_reader_json,
            spec.committed_at_ms
        ],
    )?;
    Ok(())
}

pub(super) fn expected_response_json(cutover_marker: &str) -> String {
    serde_json::json!({
        "cutoverMarker": cutover_marker,
        "cutoverMode": "production",
        "executionAuthority": "service"
    })
    .to_string()
}

pub(super) fn load_authority(
    connection: &Connection,
) -> Result<PersistedAuthoritySnapshot, RepositoryError> {
    let meta: Option<(String, i64)> = connection
        .query_row(
            "SELECT value_text, updated_at_ms
             FROM app_meta
             WHERE key = ?1",
            [AUTHORITY_META_KEY],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let epoch: Option<(String, i64)> = connection
        .query_row(
            "SELECT value_text, updated_at_ms
             FROM app_meta
             WHERE key = ?1",
            [PRODUCTION_EPOCH_META_KEY],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let minimum_reader = load_minimum_reader(connection)?;
    let cutover_rows: i64 = connection.query_row(
        "SELECT COUNT(*)
         FROM config_import_runs
         WHERE mode = 'cutover' OR cutover_marker IS NOT NULL",
        [],
        |row| row.get(0),
    )?;
    if cutover_rows == 0 && meta.is_none() && epoch.is_none() && minimum_reader.is_none() {
        return Ok(PersistedAuthoritySnapshot::rehearsal());
    }
    if cutover_rows == 0 {
        let (
            Some((meta_json, meta_updated_at_ms)),
            Some((epoch_value, epoch_updated_at_ms)),
            Some(minimum_reader),
        ) = (meta.as_ref(), epoch.as_ref(), minimum_reader.as_ref())
        else {
            return Err(RepositoryError::CutoverStateInvalid);
        };
        let value: serde_json::Value =
            serde_json::from_str(meta_json).map_err(|_| RepositoryError::CutoverStateInvalid)?;
        if value
            .get("authorityKind")
            .and_then(serde_json::Value::as_str)
            == Some(STANDALONE_AUTHORITY_KIND)
        {
            return load_standalone_authority(
                meta_json,
                *meta_updated_at_ms,
                epoch_value,
                *epoch_updated_at_ms,
                minimum_reader,
            );
        }
        return Err(RepositoryError::CutoverStateInvalid);
    }
    if cutover_rows != 1 || meta.is_none() || epoch.is_none() || minimum_reader.is_none() {
        return Err(RepositoryError::CutoverStateInvalid);
    }
    let (meta_json, meta_updated_at_ms) = meta.expect("checked above");
    let document: CutoverAuditDocumentV1 =
        serde_json::from_str(&meta_json).map_err(|_| RepositoryError::CutoverStateInvalid)?;
    validate_document(&document)?;
    let canonical_json =
        serde_json::to_string(&document).map_err(|_| RepositoryError::CutoverStateInvalid)?;
    if canonical_json != meta_json || meta_updated_at_ms != document.committed_at_ms {
        return Err(RepositoryError::CutoverStateInvalid);
    }
    let minimum_reader = minimum_reader.expect("checked above");
    if minimum_reader.cutover_marker != document.cutover_marker
        || minimum_reader.updated_at_ms < document.committed_at_ms
    {
        return Err(RepositoryError::CutoverStateInvalid);
    }
    let (epoch_value, epoch_updated_at_ms) = epoch.expect("checked above");
    let recovery_disposition = if epoch_value == ROLLBACK_ELIGIBLE_VALUE {
        if epoch_updated_at_ms != document.committed_at_ms {
            return Err(RepositoryError::CutoverStateInvalid);
        }
        PersistedCutoverRecoveryDisposition::RollbackEligible
    } else if epoch_value.starts_with(FORWARD_FIX_REQUIRED_PREFIX) {
        if epoch_value.len() <= FORWARD_FIX_REQUIRED_PREFIX.len()
            || epoch_updated_at_ms < document.committed_at_ms
        {
            return Err(RepositoryError::CutoverStateInvalid);
        }
        PersistedCutoverRecoveryDisposition::ForwardFixRequired
    } else {
        return Err(RepositoryError::CutoverStateInvalid);
    };

    let cutover = connection
        .query_row(
            "SELECT id, source_kind, source_checksum_blake3, status, diff_json,
                    started_at_ms, completed_at_ms, cutover_marker,
                    confirmation_token_id, created_at_ms
             FROM config_import_runs
             WHERE mode = 'cutover'",
            [],
            |row| {
                Ok(CutoverRow {
                    id: row.get(0)?,
                    source_kind: row.get(1)?,
                    source_checksum_blake3: row.get(2)?,
                    status: row.get(3)?,
                    diff_json: row.get(4)?,
                    started_at_ms: row.get(5)?,
                    completed_at_ms: row.get(6)?,
                    cutover_marker: row.get(7)?,
                    confirmation_token_id: row.get(8)?,
                    created_at_ms: row.get(9)?,
                })
            },
        )
        .optional()?
        .ok_or(RepositoryError::CutoverStateInvalid)?;
    if cutover.id != document.cutover_run_id
        || cutover.source_kind != "legacy_toml"
        || cutover.source_checksum_blake3 != document.source_checksum_blake3
        || cutover.status != "succeeded"
        || cutover.diff_json != canonical_json
        || cutover.started_at_ms > document.committed_at_ms
        || cutover.completed_at_ms != Some(document.committed_at_ms)
        || cutover.cutover_marker.as_deref() != Some(document.cutover_marker.as_str())
        || cutover.confirmation_token_id.as_deref() != Some(document.confirmation_token_id.as_str())
        || cutover.created_at_ms != cutover.started_at_ms
    {
        return Err(RepositoryError::CutoverStateInvalid);
    }
    verify_shadow_import_for_cutover(
        connection,
        &document.shadow_import_id,
        &document.source_checksum_blake3,
        &document.relation_digest_blake3,
    )
    .map_err(|_| RepositoryError::CutoverStateInvalid)?;

    let binding = connection
        .query_row(
            "SELECT
                 confirmation.action,
                 confirmation.client_sid_hash,
                 confirmation.resource_type,
                 confirmation.resource_id,
                 confirmation.resource_fingerprint,
                 confirmation.payload_digest_blake3,
                 confirmation.state,
                 confirmation.issued_at_ms,
                 confirmation.expires_at_ms,
                 confirmation.consumed_at_ms,
                 confirmation.consumed_by_request_id,
                 ledger.request_id,
                 ledger.command_name,
                 ledger.client_sid_hash,
                 ledger.payload_digest_blake3,
                 ledger.state,
                 ledger.accepted_at_ms,
                 ledger.expires_at_ms,
                 ledger.result_ref_type,
                 ledger.result_ref_id,
                 ledger.response_json,
                 ledger.completed_at_ms
             FROM confirmation_tokens AS confirmation
             JOIN ipc_request_ledger AS ledger
               ON ledger.request_id = confirmation.consumed_by_request_id
             WHERE confirmation.id = ?1",
            [document.confirmation_token_id.as_str()],
            |row| {
                Ok(ConfirmationLedgerRow {
                    action: row.get(0)?,
                    client_sid_hash: row.get(1)?,
                    resource_type: row.get(2)?,
                    resource_id: row.get(3)?,
                    resource_fingerprint: row.get(4)?,
                    confirmation_payload_digest: row.get(5)?,
                    confirmation_state: row.get(6)?,
                    confirmation_issued_at_ms: row.get(7)?,
                    confirmation_expires_at_ms: row.get(8)?,
                    consumed_at_ms: row.get(9)?,
                    consumed_by_request_id: row.get(10)?,
                    ledger_request_id: row.get(11)?,
                    command_name: row.get(12)?,
                    ledger_client_sid_hash: row.get(13)?,
                    ledger_payload_digest: row.get(14)?,
                    ledger_state: row.get(15)?,
                    ledger_accepted_at_ms: row.get(16)?,
                    ledger_expires_at_ms: row.get(17)?,
                    result_ref_type: row.get(18)?,
                    result_ref_id: row.get(19)?,
                    response_json: row.get(20)?,
                    completed_at_ms: row.get(21)?,
                })
            },
        )
        .optional()?
        .ok_or(RepositoryError::CutoverStateInvalid)?;
    if binding.action != "config_cutover"
        || !is_digest(&binding.client_sid_hash)
        || binding.resource_type != CONFIRMATION_RESOURCE_TYPE
        || binding.resource_id != document.shadow_import_id
        || binding.resource_fingerprint != document.cutover_marker
        || binding.confirmation_payload_digest != document.request_payload_digest_blake3
        || binding.confirmation_state != "consumed"
        || binding.confirmation_issued_at_ms > binding.ledger_accepted_at_ms
        || binding.confirmation_expires_at_ms <= binding.ledger_accepted_at_ms
        || binding.consumed_at_ms != Some(document.committed_at_ms)
        || binding.consumed_by_request_id.as_deref() != Some(document.request_id.as_str())
        || binding.ledger_request_id != document.request_id
        || binding.command_name != "config.cutover"
        || binding.ledger_client_sid_hash != binding.client_sid_hash
        || binding.ledger_payload_digest != document.request_payload_digest_blake3
        || binding.ledger_state != "succeeded"
        || binding.ledger_accepted_at_ms != cutover.started_at_ms
        || binding.ledger_expires_at_ms <= document.committed_at_ms
        || binding.result_ref_type.as_deref() != Some("config_cutover")
        || binding.result_ref_id.as_deref() != Some(document.cutover_marker.as_str())
        || binding.response_json.as_deref()
            != Some(expected_response_json(&document.cutover_marker).as_str())
        || binding.completed_at_ms != Some(document.committed_at_ms)
    {
        return Err(RepositoryError::CutoverStateInvalid);
    }

    Ok(PersistedAuthoritySnapshot {
        origin: PersistedAuthorityOrigin::LegacyCutover,
        execution_authority: PersistedExecutionAuthority::Service,
        cutover_mode: PersistedCutoverMode::Production,
        g1_source_revision: Some(document.g1_source_revision),
        g1_source_tree_digest: Some(document.g1_source_tree_digest),
        g1_build_id: Some(document.g1_build_id),
        cutover_marker: Some(document.cutover_marker),
        recovery_disposition: Some(recovery_disposition),
    })
}

fn load_standalone_authority(
    meta_json: &str,
    meta_updated_at_ms: i64,
    epoch_value: &str,
    epoch_updated_at_ms: i64,
    minimum_reader: &MinimumReaderVersionSnapshot,
) -> Result<PersistedAuthoritySnapshot, RepositoryError> {
    let document: StandaloneAuthorityDocumentV1 =
        serde_json::from_str(meta_json).map_err(|_| RepositoryError::CutoverStateInvalid)?;
    validate_standalone_document(&document)?;
    let canonical =
        serde_json::to_string(&document).map_err(|_| RepositoryError::CutoverStateInvalid)?;
    if canonical != meta_json
        || meta_updated_at_ms != document.committed_at_ms
        || epoch_value != STANDALONE_EPOCH_VALUE
        || epoch_updated_at_ms != document.committed_at_ms
        || minimum_reader.cutover_marker != document.authority_marker
        || minimum_reader.updated_at_ms < document.committed_at_ms
    {
        return Err(RepositoryError::CutoverStateInvalid);
    }
    Ok(PersistedAuthoritySnapshot {
        origin: PersistedAuthorityOrigin::StandaloneInstall,
        execution_authority: PersistedExecutionAuthority::Service,
        cutover_mode: PersistedCutoverMode::Production,
        g1_source_revision: Some(document.source_revision),
        g1_source_tree_digest: Some(document.package_manifest_digest_sha256),
        g1_build_id: Some(document.build_id),
        cutover_marker: Some(document.authority_marker),
        recovery_disposition: Some(PersistedCutoverRecoveryDisposition::ForwardFixRequired),
    })
}

fn ensure_pristine_standalone_store(connection: &Connection) -> Result<(), RepositoryError> {
    let mut statement = connection.prepare(
        "SELECT name
         FROM sqlite_schema
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
         ORDER BY name",
    )?;
    let table_names = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    if table_names.len() != EXPECTED_STANDALONE_TABLES {
        return Err(RepositoryError::StandaloneBootstrapPreconditionFailed);
    }
    for table_name in table_names {
        match table_name.as_str() {
            "schema_migrations" => {
                let pristine: bool = connection.query_row(
                    "SELECT COUNT(*) = 3
                        AND SUM(CASE WHEN version IN (2, 3, 4) THEN 1 ELSE 0 END) = 3
                     FROM schema_migrations",
                    [],
                    |row| row.get(0),
                )?;
                if pristine {
                    continue;
                }
            }
            "workspaces" => {
                let pristine: bool = connection.query_row(
                    "SELECT COUNT(*) = 1 AND EXISTS(
                         SELECT 1 FROM workspaces
                         WHERE id = '00000000-0000-4000-8000-000000000001'
                           AND display_name = 'Default Workspace'
                           AND normalized_name = 'default workspace'
                           AND state = 'active'
                           AND is_default = 1
                           AND created_at_ms = 0
                           AND updated_at_ms = 0
                     ) FROM workspaces",
                    [],
                    |row| row.get(0),
                )?;
                if pristine {
                    continue;
                }
            }
            "workspace_dispatch_state" => {
                let pristine: bool = connection.query_row(
                    "SELECT COUNT(*) = 1 AND EXISTS(
                         SELECT 1 FROM workspace_dispatch_state
                         WHERE workspace_id = '00000000-0000-4000-8000-000000000001'
                           AND last_claimed_at_ms = 0
                           AND claim_count = 0
                     ) FROM workspace_dispatch_state",
                    [],
                    |row| row.get(0),
                )?;
                if pristine {
                    continue;
                }
            }
            _ if !table_name.contains(['"', '\0']) => {
                let query = format!("SELECT EXISTS(SELECT 1 FROM \"{table_name}\")");
                let has_rows: bool = connection.query_row(&query, [], |row| row.get(0))?;
                if !has_rows {
                    continue;
                }
            }
            _ => {}
        }
        return Err(RepositoryError::StandaloneBootstrapPreconditionFailed);
    }
    Ok(())
}

fn standalone_document_from_spec(
    spec: &StandaloneAuthorityBootstrapSpec,
) -> StandaloneAuthorityDocumentV1 {
    let mut document = StandaloneAuthorityDocumentV1 {
        schema_version: 1,
        authority_kind: STANDALONE_AUTHORITY_KIND.to_owned(),
        execution_authority: "service".to_owned(),
        cutover_mode: "production".to_owned(),
        installation_id: spec.installation_id.to_string(),
        source_revision: spec.source_revision.clone(),
        package_manifest_digest_sha256: spec.package_manifest_digest_sha256.clone(),
        build_id: spec.build_id.clone(),
        service_binary_sha256: spec.service_binary_sha256.clone(),
        bootstrap_binary_version: env!("CARGO_PKG_VERSION").to_owned(),
        authority_marker: String::new(),
        committed_at_ms: spec.committed_at_ms,
    };
    document.authority_marker = standalone_marker_for_document(&document);
    document
}

fn validate_standalone_document(
    document: &StandaloneAuthorityDocumentV1,
) -> Result<(), RepositoryError> {
    if document.schema_version != 1
        || document.authority_kind != STANDALONE_AUTHORITY_KIND
        || document.execution_authority != "service"
        || document.cutover_mode != "production"
        || !is_canonical_uuid(&document.installation_id)
        || !is_source_revision(&document.source_revision)
        || !is_digest(&document.package_manifest_digest_sha256)
        || !is_digest(&document.build_id)
        || !is_digest(&document.service_binary_sha256)
        || parse_reader_version(&document.bootstrap_binary_version).is_none()
        || document.authority_marker != standalone_marker_for_document(document)
        || document.committed_at_ms < 0
    {
        return Err(RepositoryError::CutoverStateInvalid);
    }
    Ok(())
}

fn document_from_spec(
    request_id: &str,
    request_payload_digest_blake3: &str,
    confirmation_token_id: &str,
    spec: &ConfigCutoverSpec,
) -> CutoverAuditDocumentV1 {
    CutoverAuditDocumentV1 {
        schema_version: 1,
        execution_authority: "service".to_owned(),
        cutover_mode: "production".to_owned(),
        cutover_run_id: spec.cutover_run_id.to_string(),
        shadow_import_id: spec.shadow_import_id.to_string(),
        source_checksum_blake3: spec.source_checksum_blake3.clone(),
        relation_digest_blake3: spec.relation_digest_blake3.clone(),
        recovery_bundle_digest_blake3: spec.recovery_bundle_digest_blake3.clone(),
        legacy_scheduler_freeze_digest_blake3: spec.legacy_scheduler_freeze_digest_blake3.clone(),
        service_health_digest_blake3: spec.service_health_digest_blake3.clone(),
        g1_source_revision: spec.g1_evidence.source_revision.clone(),
        g1_source_tree_digest: spec.g1_evidence.source_tree_digest.clone(),
        g1_build_id: spec.g1_evidence.build_id.clone(),
        g1_evidence_digest_blake3: spec.g1_evidence.evidence_digest_blake3.clone(),
        request_id: request_id.to_owned(),
        request_payload_digest_blake3: request_payload_digest_blake3.to_owned(),
        confirmation_token_id: confirmation_token_id.to_owned(),
        cutover_marker: spec.cutover_marker.clone(),
        committed_at_ms: spec.committed_at_ms,
    }
}

fn validate_document(document: &CutoverAuditDocumentV1) -> Result<(), RepositoryError> {
    if document.schema_version != 1
        || document.execution_authority != "service"
        || document.cutover_mode != "production"
        || !is_canonical_uuid(&document.cutover_run_id)
        || !is_canonical_uuid(&document.shadow_import_id)
        || !is_digest(&document.source_checksum_blake3)
        || !is_digest(&document.relation_digest_blake3)
        || !is_digest(&document.recovery_bundle_digest_blake3)
        || !is_digest(&document.legacy_scheduler_freeze_digest_blake3)
        || !is_digest(&document.service_health_digest_blake3)
        || !is_source_revision(&document.g1_source_revision)
        || !is_digest(&document.g1_source_tree_digest)
        || !is_digest(&document.g1_build_id)
        || !is_digest(&document.g1_evidence_digest_blake3)
        || !is_canonical_uuid(&document.request_id)
        || !is_digest(&document.request_payload_digest_blake3)
        || !is_canonical_uuid(&document.confirmation_token_id)
        || document.cutover_marker != marker_for_document(document)
        || document.committed_at_ms < 0
    {
        return Err(RepositoryError::CutoverStateInvalid);
    }
    Ok(())
}

fn load_minimum_reader(
    connection: &Connection,
) -> Result<Option<MinimumReaderVersionSnapshot>, RepositoryError> {
    let row: Option<(String, i64)> = connection
        .query_row(
            "SELECT value_text, updated_at_ms FROM app_meta WHERE key = ?1",
            [MINIMUM_READER_META_KEY],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((value_text, updated_at_ms)) = row else {
        return Ok(None);
    };
    let document: MinimumReaderDocumentV1 =
        serde_json::from_str(&value_text).map_err(|_| RepositoryError::CutoverStateInvalid)?;
    validate_minimum_reader_document(&document)?;
    let canonical =
        serde_json::to_string(&document).map_err(|_| RepositoryError::CutoverStateInvalid)?;
    if canonical != value_text || updated_at_ms != document.updated_at_ms {
        return Err(RepositoryError::CutoverStateInvalid);
    }
    Ok(Some(MinimumReaderVersionSnapshot {
        minimum_reader_version: document.minimum_reader_version,
        cutover_marker: document.cutover_marker,
        updated_at_ms: document.updated_at_ms,
    }))
}

fn validate_minimum_reader_document(
    document: &MinimumReaderDocumentV1,
) -> Result<(), RepositoryError> {
    if document.schema_version != 1
        || parse_reader_version(&document.minimum_reader_version).is_none()
        || !is_authority_marker(&document.cutover_marker)
        || document.updated_at_ms < 0
    {
        return Err(RepositoryError::CutoverStateInvalid);
    }
    Ok(())
}

fn parse_reader_version(value: &str) -> Option<semver::Version> {
    semver::Version::parse(value).ok()
}

fn load_rollback_audit(
    connection: &Connection,
    rollback_id: Uuid,
) -> Result<Option<CutoverRollbackAuditDocumentV1>, RepositoryError> {
    let rollback_id = rollback_id.to_string();
    let count: i64 = connection.query_row(
        "SELECT COUNT(*)
         FROM config_import_runs
         WHERE error_code = 'CONFIG_CUTOVER_ROLLED_BACK'
           AND json_extract(diff_json, '$.rollbackId') = ?1",
        [rollback_id.as_str()],
        |row| row.get(0),
    )?;
    if count > 1 {
        return Err(RepositoryError::CutoverStateInvalid);
    }
    let audit_json: Option<String> = connection
        .query_row(
            "SELECT diff_json
             FROM config_import_runs
             WHERE error_code = 'CONFIG_CUTOVER_ROLLED_BACK'
               AND json_extract(diff_json, '$.rollbackId') = ?1",
            [rollback_id.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    let Some(audit_json) = audit_json else {
        return Ok(None);
    };
    let audit =
        serde_json::from_str(&audit_json).map_err(|_| RepositoryError::CutoverStateInvalid)?;
    validate_rollback_document(&audit)?;
    let canonical_json =
        serde_json::to_string(&audit).map_err(|_| RepositoryError::CutoverStateInvalid)?;
    if canonical_json != audit_json {
        return Err(RepositoryError::CutoverStateInvalid);
    }
    Ok(Some(audit))
}

fn validate_rollback_document(
    document: &CutoverRollbackAuditDocumentV1,
) -> Result<(), RepositoryError> {
    validate_document(&document.original_cutover)?;
    if document.schema_version != 1
        || !is_canonical_uuid(&document.rollback_id)
        || !is_cutover_marker(&document.cutover_marker)
        || !is_canonical_uuid(&document.cutover_run_id)
        || !is_canonical_uuid(&document.shadow_import_id)
        || !is_digest(&document.recovery_bundle_digest_blake3)
        || !is_digest(&document.legacy_scheduler_restore_digest_blake3)
        || !is_digest(&document.requested_by_sid_hash)
        || !matches!(
            document.reason_code.as_str(),
            "service_start_failed" | "post_cutover_health_failed" | "operator_requested"
        )
        || document.rolled_back_at_ms < document.original_cutover.committed_at_ms
        || document.cutover_marker != document.original_cutover.cutover_marker
        || document.cutover_run_id != document.original_cutover.cutover_run_id
        || document.shadow_import_id != document.original_cutover.shadow_import_id
        || document.recovery_bundle_digest_blake3
            != document.original_cutover.recovery_bundle_digest_blake3
    {
        return Err(RepositoryError::CutoverStateInvalid);
    }
    Ok(())
}

fn validate_rollback_replay(
    document: &CutoverRollbackAuditDocumentV1,
    spec: &ConfigCutoverRollbackSpec,
) -> Result<(), RepositoryError> {
    if document.rollback_id != spec.rollback_id.to_string()
        || document.cutover_marker != spec.cutover_marker
        || document.recovery_bundle_digest_blake3 != spec.recovery_bundle_digest_blake3
        || document.legacy_scheduler_restore_digest_blake3
            != spec.legacy_scheduler_restore_digest_blake3
        || document.requested_by_sid_hash != spec.requested_by_sid_hash
        || document.reason_code != spec.reason.code()
        || document.rolled_back_at_ms != spec.rolled_back_at_ms
    {
        return Err(RepositoryError::CutoverRollbackConflict);
    }
    Ok(())
}

fn marker_for_spec(spec: &ConfigCutoverSpec) -> String {
    marker_from_fields(&[
        &spec.cutover_run_id.to_string(),
        &spec.shadow_import_id.to_string(),
        &spec.source_checksum_blake3,
        &spec.relation_digest_blake3,
        &spec.recovery_bundle_digest_blake3,
        &spec.legacy_scheduler_freeze_digest_blake3,
        &spec.service_health_digest_blake3,
        &spec.g1_evidence.source_revision,
        &spec.g1_evidence.source_tree_digest,
        &spec.g1_evidence.build_id,
        &spec.g1_evidence.evidence_digest_blake3,
    ])
}

fn marker_for_document(document: &CutoverAuditDocumentV1) -> String {
    marker_from_fields(&[
        &document.cutover_run_id,
        &document.shadow_import_id,
        &document.source_checksum_blake3,
        &document.relation_digest_blake3,
        &document.recovery_bundle_digest_blake3,
        &document.legacy_scheduler_freeze_digest_blake3,
        &document.service_health_digest_blake3,
        &document.g1_source_revision,
        &document.g1_source_tree_digest,
        &document.g1_build_id,
        &document.g1_evidence_digest_blake3,
    ])
}

fn standalone_marker_for_document(document: &StandaloneAuthorityDocumentV1) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"restoreplane.standalone-authority.bootstrap.v1\0");
    for field in [
        document.installation_id.as_str(),
        document.source_revision.as_str(),
        document.package_manifest_digest_sha256.as_str(),
        document.build_id.as_str(),
        document.service_binary_sha256.as_str(),
        document.bootstrap_binary_version.as_str(),
    ] {
        hasher.update(&(field.len() as u64).to_le_bytes());
        hasher.update(field.as_bytes());
    }
    hasher.update(&document.committed_at_ms.to_le_bytes());
    format!("{STANDALONE_MARKER_PREFIX}{}", hasher.finalize().to_hex())
}

fn marker_from_fields(fields: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"restoreplane.config-cutover.readiness.v1\0");
    for field in fields {
        hasher.update(&(field.len() as u64).to_le_bytes());
        hasher.update(field.as_bytes());
    }
    format!("{CUTOVER_MARKER_PREFIX}{}", hasher.finalize().to_hex())
}

fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_cutover_marker(value: &str) -> bool {
    value
        .strip_prefix(CUTOVER_MARKER_PREFIX)
        .is_some_and(is_digest)
}

fn is_standalone_marker(value: &str) -> bool {
    value
        .strip_prefix(STANDALONE_MARKER_PREFIX)
        .is_some_and(is_digest)
}

fn is_authority_marker(value: &str) -> bool {
    is_cutover_marker(value) || is_standalone_marker(value)
}

fn is_source_revision(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_canonical_uuid(value: &str) -> bool {
    Uuid::parse_str(value)
        .map(|parsed| parsed.to_string() == value)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::parse_reader_version;

    #[test]
    fn minimum_reader_versions_accept_semver_prereleases_and_order_release_after_rc() {
        let release_candidate =
            parse_reader_version("0.1.0-rc.1").expect("valid release candidate");
        let release = parse_reader_version("0.1.0").expect("valid release");

        assert!(release_candidate < release);
        assert!(parse_reader_version("v0.1.0").is_none());
        assert!(parse_reader_version(" 0.1.0").is_none());
    }
}
