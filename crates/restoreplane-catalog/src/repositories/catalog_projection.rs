use std::str::FromStr;

use ed25519_dalek::VerifyingKey;
use restoreplane_core::{ArtifactPartRole, ConsistencyGrade, Engine};
use restoreplane_domain::{
    ArtifactPartId, ArtifactReplicaId, BackupRunId, BackupScopeId, DestinationRequirement,
    PlanOccurrenceId, ProtectionCompliance, RecoveryPointId, RestoreEligibility,
    SnapshotArtifactCompleteness, SnapshotReplicaDurability, StoredObjectId, WorkspaceId,
};
use restoreplane_pipeline::{
    ManifestPartV2, ReaderStatusV2, ReplicaPublishError, ReplicaPublishOutcomeV2,
    ReplicaRequirementV2, SignedSetManifestV2,
};
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};
use uuid::Uuid;

use crate::{OperationalStore, DEFAULT_WORKSPACE_ID};

use super::{
    rebuild_v2::{CatalogRebuildApplyModeV2, CatalogRebuildPlanV2},
    verification::schedule_post_backup_in_transaction,
    RepositoryError,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotAxes {
    artifact_completeness: SnapshotArtifactCompleteness,
    replica_durability: SnapshotReplicaDurability,
    protection_compliance: ProtectionCompliance,
    restore_eligibility: RestoreEligibility,
}

impl SnapshotAxes {
    pub fn calculate(
        parts: &[ArtifactPartSpec],
        replicas: &[ArtifactReplicaSpec],
        objects: &[StoredObjectSpec],
    ) -> Result<Self, RepositoryError> {
        if parts.is_empty()
            || !parts.iter().any(|part| part.required)
            || replicas.is_empty()
            || !replicas
                .iter()
                .any(|replica| replica.requirement == DestinationRequirement::Required)
        {
            return Err(RepositoryError::CatalogProjectionInputInvalid);
        }

        let mut part_ids = std::collections::BTreeSet::new();
        for part in parts {
            part.validate()?;
            if !part_ids.insert(part.id.as_str()) {
                return Err(RepositoryError::CatalogProjectionInputInvalid);
            }
        }
        let mut replica_ids = std::collections::BTreeSet::new();
        for replica in replicas {
            replica.validate()?;
            if !replica_ids.insert(replica.id.as_str()) {
                return Err(RepositoryError::CatalogProjectionInputInvalid);
            }
        }
        let mut object_ids = std::collections::BTreeSet::new();
        for object in objects {
            object.validate()?;
            if !object_ids.insert(object.id.as_str())
                || !replica_ids.contains(object.artifact_replica_id.as_str())
                || object
                    .artifact_part_id
                    .as_ref()
                    .is_some_and(|part_id| !part_ids.contains(part_id.as_str()))
            {
                return Err(RepositoryError::CatalogProjectionInputInvalid);
            }
        }

        let replica_is_complete = |replica: &ArtifactReplicaSpec| {
            replica.state == ArtifactReplicaState::Committed
                && replica.reader_status == ReplicaReaderStatus::Eligible
                && parts.iter().filter(|part| part.required).all(|part| {
                    objects.iter().any(|object| {
                        object.artifact_replica_id == replica.id
                            && object.artifact_part_id.as_ref() == Some(&part.id)
                            && object.role == StoredObjectRole::Payload
                    })
                })
        };
        let eligible_replica_count = replicas
            .iter()
            .filter(|replica| replica_is_complete(replica))
            .count();
        let required_replicas: Vec<_> = replicas
            .iter()
            .filter(|replica| replica.requirement == DestinationRequirement::Required)
            .collect();
        let all_required_healthy = required_replicas
            .iter()
            .all(|replica| replica_is_complete(replica));

        let artifact_completeness = if eligible_replica_count > 0 {
            SnapshotArtifactCompleteness::Complete
        } else {
            SnapshotArtifactCompleteness::Incomplete
        };
        let replica_durability = if all_required_healthy {
            SnapshotReplicaDurability::Healthy
        } else if eligible_replica_count > 0 {
            SnapshotReplicaDurability::Degraded
        } else {
            SnapshotReplicaDurability::Unavailable
        };
        let protection_compliance = if all_required_healthy {
            ProtectionCompliance::Compliant
        } else {
            ProtectionCompliance::NonCompliant
        };
        let restore_eligibility = match replica_durability {
            SnapshotReplicaDurability::Healthy => RestoreEligibility::Eligible,
            SnapshotReplicaDurability::Degraded => RestoreEligibility::Conditional,
            SnapshotReplicaDurability::Unavailable => RestoreEligibility::Blocked,
        };
        Ok(Self {
            artifact_completeness,
            replica_durability,
            protection_compliance,
            restore_eligibility,
        })
    }

    pub const fn artifact_completeness(self) -> SnapshotArtifactCompleteness {
        self.artifact_completeness
    }

    pub const fn replica_durability(self) -> SnapshotReplicaDurability {
        self.replica_durability
    }

    pub const fn protection_compliance(self) -> ProtectionCompliance {
        self.protection_compliance
    }

    pub const fn restore_eligibility(self) -> RestoreEligibility {
        self.restore_eligibility
    }
}

#[derive(Clone, Debug)]
pub struct ArtifactPartSpec {
    id: ArtifactPartId,
    role: ArtifactPartRole,
    logical_name: String,
    required: bool,
    dump_format: String,
    compression_format: String,
    encryption_format: String,
    plaintext_checksum_blake3: String,
    plaintext_bytes: i64,
    created_at_ms: i64,
}

impl ArtifactPartSpec {
    pub fn database_dump(
        id: ArtifactPartId,
        logical_name: impl Into<String>,
        plaintext_checksum_blake3: impl Into<String>,
        plaintext_bytes: i64,
        created_at_ms: i64,
    ) -> Self {
        Self {
            id,
            role: ArtifactPartRole::DatabaseDump,
            logical_name: logical_name.into(),
            required: true,
            dump_format: "custom".to_owned(),
            compression_format: "zstd".to_owned(),
            encryption_format: "chacha20poly1305-stream".to_owned(),
            plaintext_checksum_blake3: plaintext_checksum_blake3.into(),
            plaintext_bytes,
            created_at_ms,
        }
    }

    pub fn id(&self) -> &ArtifactPartId {
        &self.id
    }

    fn from_manifest(part: &ManifestPartV2, created_at_ms: i64) -> Result<Self, RepositoryError> {
        let value = Self {
            id: ArtifactPartId::from_str(&part.part_id.to_string())
                .map_err(|_| RepositoryError::CatalogProjectionInputInvalid)?,
            role: part.role,
            logical_name: part.logical_name.clone(),
            required: part.required,
            dump_format: part.dump_format.clone(),
            compression_format: part.compression_format.clone(),
            encryption_format: part.encryption_format.clone(),
            plaintext_checksum_blake3: part.plaintext_blake3.clone(),
            plaintext_bytes: i64::try_from(part.plaintext_bytes)
                .map_err(|_| RepositoryError::CatalogProjectionInputInvalid)?,
            created_at_ms,
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if self.logical_name.is_empty()
            || self.logical_name.len() > 512
            || !is_format(&self.dump_format)
            || !is_format(&self.compression_format)
            || !is_format(&self.encryption_format)
            || !is_digest(&self.plaintext_checksum_blake3)
            || self.plaintext_bytes < 0
            || self.created_at_ms < 0
        {
            return Err(RepositoryError::CatalogProjectionInputInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtifactReplicaState {
    Committed,
    Degraded,
    Missing,
    Corrupt,
    Quarantined,
}

impl ArtifactReplicaState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Committed => "committed",
            Self::Degraded => "degraded",
            Self::Missing => "missing",
            Self::Corrupt => "corrupt",
            Self::Quarantined => "quarantined",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplicaReaderStatus {
    Eligible,
    Unavailable,
    Unverified,
}

impl ReplicaReaderStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Eligible => "eligible",
            Self::Unavailable => "unavailable",
            Self::Unverified => "unverified",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ArtifactReplicaSpec {
    id: ArtifactReplicaId,
    destination_fingerprint: String,
    destination_kind: String,
    requirement: DestinationRequirement,
    state: ArtifactReplicaState,
    immutability_capability: String,
    reader_status: ReplicaReaderStatus,
    committed_at_ms: Option<i64>,
    observed_at_ms: i64,
}

impl ArtifactReplicaSpec {
    #[allow(clippy::too_many_arguments)]
    pub fn observed(
        id: ArtifactReplicaId,
        destination_fingerprint: impl Into<String>,
        destination_kind: impl Into<String>,
        requirement: DestinationRequirement,
        state: ArtifactReplicaState,
        immutability_capability: impl Into<String>,
        reader_status: ReplicaReaderStatus,
        committed_at_ms: Option<i64>,
        observed_at_ms: i64,
    ) -> Self {
        Self {
            id,
            destination_fingerprint: destination_fingerprint.into(),
            destination_kind: destination_kind.into(),
            requirement,
            state,
            immutability_capability: immutability_capability.into(),
            reader_status,
            committed_at_ms,
            observed_at_ms,
        }
    }

    pub fn required_local(
        id: ArtifactReplicaId,
        destination_fingerprint: impl Into<String>,
        immutability_capability: impl Into<String>,
        committed_at_ms: i64,
        observed_at_ms: i64,
    ) -> Self {
        Self::observed(
            id,
            destination_fingerprint,
            "local",
            DestinationRequirement::Required,
            ArtifactReplicaState::Committed,
            immutability_capability,
            ReplicaReaderStatus::Eligible,
            Some(committed_at_ms),
            observed_at_ms,
        )
    }

    pub fn id(&self) -> &ArtifactReplicaId {
        &self.id
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if self.destination_fingerprint.is_empty()
            || self.destination_fingerprint.len() > 512
            || !matches!(
                self.destination_kind.as_str(),
                "local" | "s3" | "ssh_sftp" | "azure_blob" | "smb" | "rclone"
            )
            || !matches!(
                self.immutability_capability.as_str(),
                "none" | "governance" | "compliance" | "server_append_only" | "unknown"
            )
            || self.committed_at_ms.is_some_and(|value| value < 0)
            || (self.state == ArtifactReplicaState::Committed && self.committed_at_ms.is_none())
            || self
                .committed_at_ms
                .is_some_and(|value| self.observed_at_ms < value)
            || self.observed_at_ms < 0
        {
            return Err(RepositoryError::CatalogProjectionInputInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct StoredObjectSpec {
    id: StoredObjectId,
    artifact_replica_id: ArtifactReplicaId,
    artifact_part_id: Option<ArtifactPartId>,
    role: StoredObjectRole,
    object_key: String,
    object_version: Option<String>,
    checksum_blake3: String,
    size_bytes: i64,
    observed_at_ms: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StoredObjectRole {
    Payload,
    SetManifest,
}

impl StoredObjectRole {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Payload => "payload",
            Self::SetManifest => "set_manifest",
        }
    }
}

impl StoredObjectSpec {
    #[allow(clippy::too_many_arguments)]
    pub fn payload(
        id: StoredObjectId,
        artifact_replica_id: ArtifactReplicaId,
        artifact_part_id: ArtifactPartId,
        object_key: impl Into<String>,
        checksum_blake3: impl Into<String>,
        size_bytes: i64,
        observed_at_ms: i64,
    ) -> Self {
        Self {
            id,
            artifact_replica_id,
            artifact_part_id: Some(artifact_part_id),
            role: StoredObjectRole::Payload,
            object_key: object_key.into(),
            object_version: None,
            checksum_blake3: checksum_blake3.into(),
            size_bytes,
            observed_at_ms,
        }
    }

    fn set_manifest(
        id: StoredObjectId,
        artifact_replica_id: ArtifactReplicaId,
        object_key: impl Into<String>,
        checksum_blake3: impl Into<String>,
        size_bytes: i64,
        observed_at_ms: i64,
    ) -> Self {
        Self {
            id,
            artifact_replica_id,
            artifact_part_id: None,
            role: StoredObjectRole::SetManifest,
            object_key: object_key.into(),
            object_version: None,
            checksum_blake3: checksum_blake3.into(),
            size_bytes,
            observed_at_ms,
        }
    }

    pub fn id(&self) -> &StoredObjectId {
        &self.id
    }

    pub fn with_object_version(mut self, object_version: impl Into<String>) -> Self {
        self.object_version = Some(object_version.into());
        self
    }

    fn with_optional_object_version(mut self, object_version: Option<String>) -> Self {
        self.object_version = object_version;
        self
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if self.object_key.is_empty()
            || self.object_key.len() > 2048
            || self.object_version.as_deref().is_some_and(|version| {
                version.is_empty()
                    || version.len() > 1_024
                    || version
                        .chars()
                        .any(|value| matches!(value, '\0' | '\r' | '\n'))
            })
            || !is_digest(&self.checksum_blake3)
            || self.size_bytes < 0
            || self.observed_at_ms < 0
            || (self.role == StoredObjectRole::Payload && self.artifact_part_id.is_none())
            || (self.role == StoredObjectRole::SetManifest && self.artifact_part_id.is_some())
        {
            return Err(RepositoryError::CatalogProjectionInputInvalid);
        }
        Ok(())
    }
}

/// Verified input for a complete Recovery Point projection.
///
/// External callers must derive this value from a signed set Manifest and observed publish
/// outcomes. They cannot self-assert that arbitrary rows were signature-verified.
///
/// ```compile_fail
/// use restoreplane_catalog::CompleteRecoveryPointSpec;
/// use restoreplane_domain::RecoveryPointId;
///
/// let _forged = CompleteRecoveryPointSpec::new_verified(
///     RecoveryPointId::new(),
///     "instance-a",
///     "target:orders",
///     "orders",
///     "postgresql",
///     "application-consistent",
///     1_000,
///     "a".repeat(64),
///     "signing-key-01",
///     1_010,
/// );
/// ```
#[derive(Clone, Debug)]
pub struct CompleteRecoveryPointSpec {
    id: RecoveryPointId,
    origin_instance_id: String,
    origin_occurrence_id: Option<PlanOccurrenceId>,
    origin_backup_run_id: Option<BackupRunId>,
    manifest_target_id: BackupScopeId,
    source_target_identity: String,
    manifest_scope_evidence: Option<ManifestScopeEvidence>,
    source_display_name: String,
    engine: String,
    consistency_grade: String,
    committed_at_ms: i64,
    set_manifest_checksum_blake3: String,
    signing_key_id: String,
    workspace_id: WorkspaceId,
    workspace_display_name: String,
    ingested_at_ms: i64,
    parts: Vec<ArtifactPartSpec>,
    replicas: Vec<ArtifactReplicaSpec>,
    objects: Vec<StoredObjectSpec>,
}

impl CompleteRecoveryPointSpec {
    pub fn from_verified_publish(
        signed: &SignedSetManifestV2,
        outcomes: &[ReplicaPublishOutcomeV2],
        expected_verifying_key: &VerifyingKey,
        ingested_at_ms: i64,
    ) -> Result<Self, RepositoryError> {
        signed
            .verify(expected_verifying_key)
            .map_err(|_| RepositoryError::CatalogProjectionInputInvalid)?;
        let manifest_json = signed
            .to_json()
            .map_err(|_| RepositoryError::CatalogProjectionInputInvalid)?;
        let manifest_checksum = blake3::hash(manifest_json.as_bytes()).to_hex().to_string();
        let manifest_size = i64::try_from(manifest_json.len())
            .map_err(|_| RepositoryError::CatalogProjectionInputInvalid)?;
        let recovery_point_id =
            RecoveryPointId::from_str(&signed.manifest.recovery_point_id.to_string())
                .map_err(|_| RepositoryError::CatalogProjectionInputInvalid)?;
        let occurrence_id = PlanOccurrenceId::from_str(&signed.manifest.occurrence_id.to_string())
            .map_err(|_| RepositoryError::CatalogProjectionInputInvalid)?;
        let backup_run_id = BackupRunId::from_str(&signed.manifest.backup_run_id.to_string())
            .map_err(|_| RepositoryError::CatalogProjectionInputInvalid)?;
        let manifest_target_id = BackupScopeId::from_str(&signed.manifest.target_id.to_string())
            .map_err(|_| RepositoryError::CatalogProjectionInputInvalid)?;
        let manifest_scope_evidence = parse_manifest_scope_evidence(signed, &manifest_target_id)?;
        let workspace_id = WorkspaceId::from_str(
            signed
                .manifest
                .options
                .get("workspaceId")
                .map(String::as_str)
                .unwrap_or(DEFAULT_WORKSPACE_ID),
        )
        .map_err(|_| RepositoryError::CatalogProjectionInputInvalid)?;
        let workspace_display_name = signed
            .manifest
            .options
            .get("workspaceName")
            .cloned()
            .unwrap_or_else(|| "Default Workspace".to_owned());

        let mut outcomes_by_fingerprint = std::collections::BTreeMap::new();
        for outcome in outcomes {
            if outcomes_by_fingerprint
                .insert(outcome.destination_fingerprint.as_str(), outcome)
                .is_some()
            {
                return Err(RepositoryError::CatalogProjectionInputInvalid);
            }
        }
        if outcomes_by_fingerprint.len() != signed.manifest.replicas.len() {
            return Err(RepositoryError::CatalogProjectionInputInvalid);
        }

        let parts = signed
            .manifest
            .parts
            .iter()
            .map(|part| ArtifactPartSpec::from_manifest(part, signed.manifest.committed_at_ms))
            .collect::<Result<Vec<_>, _>>()?;
        let mut replicas = Vec::with_capacity(signed.manifest.replicas.len());
        let mut objects = Vec::new();
        for replica in &signed.manifest.replicas {
            let outcome = outcomes_by_fingerprint
                .get(replica.destination_fingerprint.as_str())
                .copied()
                .ok_or(RepositoryError::CatalogProjectionInputInvalid)?;
            let requirement = map_replica_requirement(replica.requirement);
            if requirement != map_replica_requirement(outcome.requirement)
                || outcome.payload_receipts.len() > signed.manifest.parts.len()
                || outcome
                    .payload_receipts
                    .iter()
                    .zip(&signed.manifest.parts)
                    .any(|(receipt, part)| receipt.bytes != part.ciphertext_bytes)
                || outcome
                    .manifest_receipt
                    .as_ref()
                    .is_some_and(|receipt| i64::try_from(receipt.bytes).ok() != Some(manifest_size))
                || (outcome.result.is_ok()
                    && (outcome.payload_receipts.len() != signed.manifest.parts.len()
                        || outcome.manifest_receipt.is_none()
                        || outcome.reader_status != ReaderStatusV2::Eligible))
            {
                return Err(RepositoryError::CatalogProjectionInputInvalid);
            }

            let replica_id = ArtifactReplicaId::from_stable_name(
                recovery_point_id.as_uuid(),
                format!("replica\0{}", replica.destination_fingerprint).as_bytes(),
            );
            let state = replica_state_from_outcome(outcome);
            let reader_status = map_reader_status(outcome.reader_status);
            let committed_at_ms = outcome
                .manifest_receipt
                .as_ref()
                .map(|_| signed.manifest.committed_at_ms);
            replicas.push(ArtifactReplicaSpec::observed(
                replica_id.clone(),
                replica.destination_fingerprint.clone(),
                replica.destination_kind.clone(),
                requirement,
                state,
                replica.immutability_capability.clone(),
                reader_status,
                committed_at_ms,
                ingested_at_ms,
            ));

            for (part, receipt) in signed.manifest.parts.iter().zip(&outcome.payload_receipts) {
                let part_id = ArtifactPartId::from_str(&part.part_id.to_string())
                    .map_err(|_| RepositoryError::CatalogProjectionInputInvalid)?;
                let object_id = StoredObjectId::from_stable_name(
                    recovery_point_id.as_uuid(),
                    format!(
                        "payload\0{}\0{}",
                        replica.destination_fingerprint, part.part_id
                    )
                    .as_bytes(),
                );
                objects.push(
                    StoredObjectSpec::payload(
                        object_id,
                        replica_id.clone(),
                        part_id,
                        part.object_key.clone(),
                        part.ciphertext_blake3.clone(),
                        i64::try_from(part.ciphertext_bytes)
                            .map_err(|_| RepositoryError::CatalogProjectionInputInvalid)?,
                        ingested_at_ms,
                    )
                    .with_optional_object_version(receipt.version.clone()),
                );
            }
            if let Some(receipt) = &outcome.manifest_receipt {
                let object_id = StoredObjectId::from_stable_name(
                    recovery_point_id.as_uuid(),
                    format!("set-manifest\0{}", replica.destination_fingerprint).as_bytes(),
                );
                objects.push(
                    StoredObjectSpec::set_manifest(
                        object_id,
                        replica_id,
                        replica.manifest_object_key.clone(),
                        manifest_checksum.clone(),
                        manifest_size,
                        ingested_at_ms,
                    )
                    .with_optional_object_version(receipt.version.clone()),
                );
            }
        }
        if outcomes_by_fingerprint.keys().any(|fingerprint| {
            !signed
                .manifest
                .replicas
                .iter()
                .any(|replica| replica.destination_fingerprint == **fingerprint)
        }) {
            return Err(RepositoryError::CatalogProjectionInputInvalid);
        }

        let value = Self {
            id: recovery_point_id,
            origin_instance_id: signed.manifest.origin_instance_id.clone(),
            origin_occurrence_id: Some(occurrence_id),
            origin_backup_run_id: Some(backup_run_id),
            manifest_target_id,
            source_target_identity: signed.manifest.source_identity.clone(),
            manifest_scope_evidence,
            source_display_name: signed.manifest.source_display_name.clone(),
            engine: engine_str(signed.manifest.engine).to_owned(),
            consistency_grade: consistency_grade_str(signed.manifest.consistency_grade).to_owned(),
            committed_at_ms: signed.manifest.committed_at_ms,
            set_manifest_checksum_blake3: manifest_checksum,
            signing_key_id: signed.manifest.signing_key_id.clone(),
            workspace_id,
            workspace_display_name,
            ingested_at_ms,
            parts,
            replicas,
            objects,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn id(&self) -> &RecoveryPointId {
        &self.id
    }

    pub fn snapshot_axes(&self) -> Result<SnapshotAxes, RepositoryError> {
        SnapshotAxes::calculate(&self.parts, &self.replicas, &self.objects)
    }

    pub(super) fn plaintext_bytes(&self) -> Result<i64, RepositoryError> {
        self.parts.iter().try_fold(0_i64, |total, part| {
            total
                .checked_add(part.plaintext_bytes)
                .ok_or(RepositoryError::CatalogProjectionInputInvalid)
        })
    }

    pub(super) fn matches_backup_origin(
        &self,
        occurrence_id: &str,
        backup_run_id: &str,
        engine: &str,
    ) -> bool {
        self.origin_occurrence_id
            .as_ref()
            .is_some_and(|value| value.as_str() == occurrence_id)
            && self
                .origin_backup_run_id
                .as_ref()
                .is_some_and(|value| value.as_str() == backup_run_id)
            && self.engine == engine
    }

    pub(super) fn matches_backup_scope(
        &self,
        backup_scope_id: &str,
        source_fingerprint: &str,
        scope_mode: &str,
        scope_snapshot_hash_blake3: &str,
        resolved_object_count: i64,
    ) -> bool {
        if self.manifest_target_id.as_str() != backup_scope_id
            || self.source_target_identity != source_fingerprint
        {
            return false;
        }
        match &self.manifest_scope_evidence {
            Some(evidence) => {
                evidence.mode == scope_mode
                    && evidence.snapshot_hash_blake3 == scope_snapshot_hash_blake3
                    && evidence.object_count == resolved_object_count
            }
            None => {
                scope_mode == "whole_database"
                    && resolved_object_count == 0
                    && scope_snapshot_hash_blake3 == crate::WHOLE_DATABASE_SELECTOR_HASH_BLAKE3
            }
        }
    }

    pub(super) fn validate(&self) -> Result<(), RepositoryError> {
        let origin_pair_valid =
            self.origin_occurrence_id.is_some() == self.origin_backup_run_id.is_some();
        if self.origin_instance_id.is_empty()
            || !origin_pair_valid
            || self.source_target_identity.is_empty()
            || self.source_display_name.is_empty()
            || self.engine.is_empty()
            || self.consistency_grade.is_empty()
            || self.signing_key_id.is_empty()
            || self.workspace_display_name.trim().is_empty()
            || self.workspace_display_name.chars().count() > 96
            || self.workspace_display_name.chars().any(char::is_control)
            || !is_digest(&self.set_manifest_checksum_blake3)
            || self.committed_at_ms < 0
            || self.ingested_at_ms < self.committed_at_ms
        {
            return Err(RepositoryError::CatalogProjectionInputInvalid);
        }
        for part in &self.parts {
            part.validate()?;
        }
        for replica in &self.replicas {
            replica.validate()?;
        }
        for object in &self.objects {
            object.validate()?;
        }
        if self.snapshot_axes()?.artifact_completeness() != SnapshotArtifactCompleteness::Complete {
            return Err(RepositoryError::RecoveryPointIncomplete);
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct ManifestScopeEvidence {
    mode: String,
    snapshot_hash_blake3: String,
    object_count: i64,
}

fn parse_manifest_scope_evidence(
    signed: &SignedSetManifestV2,
    manifest_target_id: &BackupScopeId,
) -> Result<Option<ManifestScopeEvidence>, RepositoryError> {
    const KEYS: [&str; 5] = [
        "backupScopeId",
        "backupScopeMode",
        "backupScopeSnapshot",
        "backupScopeSnapshotHashBlake3",
        "backupScopeObjectCount",
    ];
    let present = KEYS
        .iter()
        .filter(|key| signed.manifest.options.contains_key(**key))
        .count();
    if present == 0 {
        return Ok(None);
    }
    if present != KEYS.len() {
        return Err(RepositoryError::CatalogProjectionInputInvalid);
    }

    let scope_id = signed
        .manifest
        .options
        .get("backupScopeId")
        .ok_or(RepositoryError::CatalogProjectionInputInvalid)?;
    if BackupScopeId::from_str(scope_id)
        .map_err(|_| RepositoryError::CatalogProjectionInputInvalid)?
        != *manifest_target_id
    {
        return Err(RepositoryError::CatalogProjectionInputInvalid);
    }
    let mode = signed
        .manifest
        .options
        .get("backupScopeMode")
        .ok_or(RepositoryError::CatalogProjectionInputInvalid)?;
    if !matches!(mode.as_str(), "whole_database" | "explicit_tables") {
        return Err(RepositoryError::CatalogProjectionInputInvalid);
    }
    let snapshot_hash = signed
        .manifest
        .options
        .get("backupScopeSnapshotHashBlake3")
        .ok_or(RepositoryError::CatalogProjectionInputInvalid)?;
    if !is_digest(snapshot_hash) {
        return Err(RepositoryError::CatalogProjectionInputInvalid);
    }
    let object_count_text = signed
        .manifest
        .options
        .get("backupScopeObjectCount")
        .ok_or(RepositoryError::CatalogProjectionInputInvalid)?;
    let object_count = object_count_text
        .parse::<i64>()
        .map_err(|_| RepositoryError::CatalogProjectionInputInvalid)?;
    if object_count < 0 || object_count.to_string() != *object_count_text {
        return Err(RepositoryError::CatalogProjectionInputInvalid);
    }
    let snapshot = signed
        .manifest
        .options
        .get("backupScopeSnapshot")
        .ok_or(RepositoryError::CatalogProjectionInputInvalid)?;
    validate_manifest_scope_snapshot(snapshot, mode, snapshot_hash, object_count)?;
    Ok(Some(ManifestScopeEvidence {
        mode: mode.clone(),
        snapshot_hash_blake3: snapshot_hash.clone(),
        object_count,
    }))
}

fn validate_manifest_scope_snapshot(
    snapshot: &str,
    expected_mode: &str,
    expected_hash_blake3: &str,
    expected_object_count: i64,
) -> Result<(), RepositoryError> {
    let parsed: serde_json::Value = serde_json::from_str(snapshot)
        .map_err(|_| RepositoryError::CatalogProjectionInputInvalid)?;
    let canonical = serde_json::to_string(&parsed)
        .map_err(|_| RepositoryError::CatalogProjectionInputInvalid)?;
    if canonical != snapshot {
        return Err(RepositoryError::CatalogProjectionInputInvalid);
    }
    let object = parsed
        .as_object()
        .ok_or(RepositoryError::CatalogProjectionInputInvalid)?;
    if object.len() == 1 {
        let digest = object
            .get("blake3")
            .and_then(serde_json::Value::as_str)
            .ok_or(RepositoryError::CatalogProjectionInputInvalid)?;
        if digest != expected_hash_blake3 || !is_digest(digest) {
            return Err(RepositoryError::CatalogProjectionInputInvalid);
        }
        return Ok(());
    }
    if object.len() != 2
        || object.get("mode").and_then(serde_json::Value::as_str) != Some(expected_mode)
    {
        return Err(RepositoryError::CatalogProjectionInputInvalid);
    }
    let objects = object
        .get("objects")
        .and_then(serde_json::Value::as_array)
        .ok_or(RepositoryError::CatalogProjectionInputInvalid)?;
    if i64::try_from(objects.len()).ok() != Some(expected_object_count)
        || blake3::hash(canonical.as_bytes()).to_hex().as_str() != expected_hash_blake3
    {
        return Err(RepositoryError::CatalogProjectionInputInvalid);
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotRecord {
    artifact_completeness: SnapshotArtifactCompleteness,
    replica_durability: SnapshotReplicaDurability,
    protection_compliance: ProtectionCompliance,
    restore_eligibility: RestoreEligibility,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlStateProbe {
    digest_blake3: String,
    plan_count: i64,
    occurrence_count: i64,
    backup_run_count: i64,
    work_item_count: i64,
    attempt_count: i64,
}

impl ControlStateProbe {
    pub fn digest_blake3(&self) -> &str {
        &self.digest_blake3
    }

    pub const fn plan_count(&self) -> i64 {
        self.plan_count
    }

    pub const fn occurrence_count(&self) -> i64 {
        self.occurrence_count
    }

    pub const fn backup_run_count(&self) -> i64 {
        self.backup_run_count
    }

    pub const fn work_item_count(&self) -> i64 {
        self.work_item_count
    }

    pub const fn attempt_count(&self) -> i64 {
        self.attempt_count
    }
}

impl SnapshotRecord {
    pub const fn artifact_completeness(&self) -> SnapshotArtifactCompleteness {
        self.artifact_completeness
    }

    pub const fn replica_durability(&self) -> SnapshotReplicaDurability {
        self.replica_durability
    }

    pub const fn protection_compliance(&self) -> ProtectionCompliance {
        self.protection_compliance
    }

    pub const fn restore_eligibility(&self) -> RestoreEligibility {
        self.restore_eligibility
    }
}

#[derive(Clone)]
pub struct CatalogProjectionRepository {
    store: OperationalStore,
}

impl OperationalStore {
    pub fn catalog_projection(&self) -> CatalogProjectionRepository {
        CatalogProjectionRepository {
            store: self.clone(),
        }
    }
}

impl CatalogProjectionRepository {
    pub fn ingest_complete(&self, spec: CompleteRecoveryPointSpec) -> Result<(), RepositoryError> {
        spec.validate()?;
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            ingest_complete_projection(&transaction, &spec)?;
            schedule_post_backup_in_transaction(&transaction, spec.id(), spec.ingested_at_ms)?;
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn snapshot(
        &self,
        recovery_point_id: &RecoveryPointId,
    ) -> Result<SnapshotRecord, RepositoryError> {
        let reader = self.store.open_reader()?;
        let raw: Option<(String, String, String, String)> = reader
            .connection()
            .query_row(
                "SELECT artifact_completeness, replica_durability,
                        protection_compliance, restore_eligibility
                 FROM catalog_snapshot_projection
                 WHERE recovery_point_id = ?1",
                [recovery_point_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let (artifact, durability, compliance, eligibility) =
            raw.ok_or(RepositoryError::NotFound)?;
        Ok(SnapshotRecord {
            artifact_completeness: SnapshotArtifactCompleteness::from_str(&artifact)?,
            replica_durability: SnapshotReplicaDurability::from_str(&durability)?,
            protection_compliance: ProtectionCompliance::from_str(&compliance)?,
            restore_eligibility: RestoreEligibility::from_str(&eligibility)?,
        })
    }

    pub fn recovery_point_counts(
        &self,
        recovery_point_id: &RecoveryPointId,
    ) -> Result<(i64, i64), RepositoryError> {
        let reader = self.store.open_reader()?;
        reader
            .connection()
            .query_row(
                "SELECT required_part_count, present_part_count
                 FROM catalog_recovery_points
                 WHERE id = ?1",
                [recovery_point_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or(RepositoryError::NotFound)
    }

    pub fn recovery_point_count(&self) -> Result<i64, RepositoryError> {
        let reader = self.store.open_reader()?;
        reader
            .connection()
            .query_row("SELECT COUNT(*) FROM catalog_recovery_points", [], |row| {
                row.get(0)
            })
            .map_err(RepositoryError::from)
    }

    pub fn clear_all_projections(&self) -> Result<(), RepositoryError> {
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute_batch(
                "DELETE FROM catalog_manifest_ingest;
                 DELETE FROM catalog_verification_projection;
                 DELETE FROM catalog_target_protection_projection;
                 DELETE FROM catalog_snapshot_projection;
                 DELETE FROM catalog_stored_objects;
                 DELETE FROM catalog_artifact_parts;
                 DELETE FROM catalog_artifact_replicas;
                 DELETE FROM catalog_recovery_points;",
            )?;
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn apply_rebuild_v2(
        &self,
        plan: CatalogRebuildPlanV2,
        mode: CatalogRebuildApplyModeV2,
    ) -> Result<usize, RepositoryError> {
        plan.validate_for_apply(mode)?;
        let accepted_count = plan.accepted_specs.len();
        if mode == CatalogRebuildApplyModeV2::DryRun {
            return Ok(0);
        }
        let accepted_specs = plan.accepted_specs;
        let rebuild_at_ms = accepted_specs
            .iter()
            .map(|spec| spec.ingested_at_ms)
            .max()
            .unwrap_or(0);
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if mode == CatalogRebuildApplyModeV2::Replace {
                transaction.execute_batch(
                    "DELETE FROM catalog_manifest_ingest;
                     DELETE FROM catalog_verification_projection;
                     DELETE FROM catalog_target_protection_projection;
                     DELETE FROM catalog_snapshot_projection;
                     DELETE FROM catalog_stored_objects;
                     DELETE FROM catalog_artifact_parts;
                     DELETE FROM catalog_artifact_replicas;
                     DELETE FROM catalog_recovery_points;",
                )?;
            }
            for spec in &accepted_specs {
                ingest_complete_projection(&transaction, spec)?;
            }
            rebuild_verification_projections_from_runs(&transaction, rebuild_at_ms)?;
            transaction.commit()?;
            Ok(accepted_count)
        })
    }

    pub fn control_state_probe(&self) -> Result<ControlStateProbe, RepositoryError> {
        let reader = self.store.open_reader()?;
        let connection = reader.connection();
        let plan_count = count_rows(connection, "backup_plans")?;
        let occurrence_count = count_rows(connection, "plan_occurrences")?;
        let backup_run_count = count_rows(connection, "backup_runs")?;
        let work_item_count = count_rows(connection, "work_items")?;
        let attempt_count = count_rows(connection, "work_attempts")?;

        let mut hasher = blake3::Hasher::new();
        for query in CONTROL_DIGEST_QUERIES {
            let mut statement = connection.prepare(query)?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            for row in rows {
                let row = row?;
                hasher.update(&(row.len() as u64).to_le_bytes());
                hasher.update(row.as_bytes());
            }
        }
        Ok(ControlStateProbe {
            digest_blake3: hasher.finalize().to_hex().to_string(),
            plan_count,
            occurrence_count,
            backup_run_count,
            work_item_count,
            attempt_count,
        })
    }

    pub fn control_to_catalog_foreign_key_count(&self) -> Result<i64, RepositoryError> {
        let reader = self.store.open_reader()?;
        let connection = reader.connection();
        let mut tables = connection.prepare(
            "SELECT name
             FROM sqlite_schema
             WHERE type = 'table'
               AND name NOT LIKE 'sqlite_%'
             ORDER BY name",
        )?;
        let table_names = tables
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let mut count = 0_i64;
        for source_table in table_names {
            if REBUILDABLE_TABLES.contains(&source_table.as_str()) {
                continue;
            }
            let escaped = source_table.replace('"', "\"\"");
            let mut foreign_keys =
                connection.prepare(&format!("PRAGMA foreign_key_list(\"{escaped}\")"))?;
            let targets = foreign_keys
                .query_map([], |row| row.get::<_, String>(2))?
                .collect::<Result<Vec<_>, _>>()?;
            count += targets
                .iter()
                .filter(|target| REBUILDABLE_TABLES.contains(&target.as_str()))
                .count() as i64;
        }
        Ok(count)
    }

    fn write<F, T>(&self, job: F) -> Result<T, RepositoryError>
    where
        F: FnOnce(&mut rusqlite::Connection) -> Result<T, RepositoryError> + Send + 'static,
        T: Send + 'static,
    {
        self.store.run_write(job).map_err(RepositoryError::from)?
    }
}

struct TerminalVerificationProjectionSource {
    recovery_point_id: String,
    requested_level: i64,
    achieved_level: Option<i64>,
    state: String,
    evidence_json: Option<String>,
    completed_at_ms: Option<i64>,
}

pub(super) fn project_terminal_verification_run(
    transaction: &Transaction<'_>,
    verification_run_id: &str,
    ingested_at_ms: i64,
) -> Result<(), RepositoryError> {
    if verification_run_id.is_empty() || ingested_at_ms < 0 {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    let row: Option<TerminalVerificationProjectionSource> = transaction
        .query_row(
            "SELECT verification.recovery_point_id, verification.requested_level,
                    verification.achieved_level, verification.state,
                    verification.evidence_json, verification.completed_at_ms
             FROM verification_runs AS verification
             JOIN catalog_recovery_points AS recovery
               ON recovery.id = verification.recovery_point_id
             WHERE verification.id = ?1",
            [verification_run_id],
            |row| {
                Ok(TerminalVerificationProjectionSource {
                    recovery_point_id: row.get(0)?,
                    requested_level: row.get(1)?,
                    achieved_level: row.get(2)?,
                    state: row.get(3)?,
                    evidence_json: row.get(4)?,
                    completed_at_ms: row.get(5)?,
                })
            },
        )
        .optional()?;
    let Some(row) = row else {
        return Err(RepositoryError::NotFound);
    };
    let completed_at_ms = row
        .completed_at_ms
        .ok_or(RepositoryError::RuntimeStateInvalid)?;
    let result = match row.state.as_str() {
        "passed" if row.achieved_level == Some(row.requested_level) => "passed",
        "failed" if row.achieved_level.is_none() => "failed",
        "unsupported" if row.achieved_level.is_none() => "unsupported",
        "cancelled" if row.achieved_level.is_none() => "cancelled",
        _ => return Err(RepositoryError::RuntimeStateInvalid),
    };
    if !(1..=3).contains(&row.requested_level) || completed_at_ms < 0 {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    let effective_ingested_at_ms = ingested_at_ms.max(completed_at_ms);
    let evidence: serde_json::Value =
        serde_json::from_str(row.evidence_json.as_deref().unwrap_or("{}"))
            .map_err(|_| RepositoryError::RuntimeStateInvalid)?;
    if !evidence.is_object() {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    let canonical_evidence =
        serde_json::to_string(&evidence).map_err(|_| RepositoryError::RuntimeStateInvalid)?;
    let evidence_checksum_blake3 = blake3::hash(canonical_evidence.as_bytes())
        .to_hex()
        .to_string();
    let projection_id = Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("catalog-verification-projection\0{verification_run_id}").as_bytes(),
    )
    .to_string();
    let affected = transaction.execute(
        "INSERT INTO catalog_verification_projection(
             id, recovery_point_id, origin_verification_run_id,
             verify_level, result, verified_at_ms, tool_name, tool_version,
             evidence_checksum_blake3, evidence_locator, evidence_json, ingested_at_ms
         ) VALUES (
             ?1, ?2, ?3,
             ?4, ?5, ?6, NULL, NULL,
             ?7, NULL, ?8, ?9
         )
         ON CONFLICT(id) DO UPDATE SET
             ingested_at_ms = MAX(
                 catalog_verification_projection.ingested_at_ms,
                 excluded.ingested_at_ms
             )
         WHERE catalog_verification_projection.recovery_point_id = excluded.recovery_point_id
           AND catalog_verification_projection.origin_verification_run_id
                 IS excluded.origin_verification_run_id
           AND catalog_verification_projection.verify_level = excluded.verify_level
           AND catalog_verification_projection.result = excluded.result
           AND catalog_verification_projection.verified_at_ms = excluded.verified_at_ms
           AND catalog_verification_projection.tool_name IS excluded.tool_name
           AND catalog_verification_projection.tool_version IS excluded.tool_version
           AND catalog_verification_projection.evidence_checksum_blake3
                 IS excluded.evidence_checksum_blake3
           AND catalog_verification_projection.evidence_locator IS excluded.evidence_locator
           AND catalog_verification_projection.evidence_json = excluded.evidence_json",
        params![
            projection_id,
            row.recovery_point_id,
            verification_run_id,
            row.requested_level,
            result,
            completed_at_ms,
            evidence_checksum_blake3,
            canonical_evidence,
            effective_ingested_at_ms,
        ],
    )?;
    if affected != 1 {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    if result == "passed" {
        let snapshot_affected = transaction.execute(
            "UPDATE catalog_snapshot_projection
             SET highest_verified_level = CASE
                     WHEN highest_verified_level IS NULL OR ?2 > highest_verified_level THEN ?2
                     ELSE highest_verified_level
                 END,
                 last_verified_at_ms = CASE
                     WHEN last_verified_at_ms IS NULL OR ?3 > last_verified_at_ms THEN ?3
                     ELSE last_verified_at_ms
                 END,
                 projection_version = projection_version + 1,
                 projected_at_ms = MAX(projected_at_ms, ?4)
             WHERE recovery_point_id = ?1",
            params![
                row.recovery_point_id,
                row.requested_level,
                completed_at_ms,
                effective_ingested_at_ms
            ],
        )?;
        if snapshot_affected != 1 {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
    }
    Ok(())
}

pub(super) fn rebuild_verification_projections_from_runs(
    transaction: &Transaction<'_>,
    ingested_at_ms: i64,
) -> Result<(), RepositoryError> {
    if ingested_at_ms < 0 {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    const MAX_REBUILD_VERIFICATION_PROJECTIONS: usize = 10_000;
    let verification_run_ids = {
        let mut statement = transaction.prepare(
            "SELECT verification.id
             FROM verification_runs AS verification
             JOIN catalog_recovery_points AS recovery
               ON recovery.id = verification.recovery_point_id
             WHERE verification.state IN ('passed', 'failed', 'unsupported', 'cancelled')
               AND verification.completed_at_ms IS NOT NULL
             ORDER BY verification.completed_at_ms, verification.id
             LIMIT 10001",
        )?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    };
    if verification_run_ids.len() > MAX_REBUILD_VERIFICATION_PROJECTIONS {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    for verification_run_id in verification_run_ids {
        project_terminal_verification_run(transaction, &verification_run_id, ingested_at_ms)?;
    }
    Ok(())
}

pub(super) fn ingest_complete_projection(
    transaction: &rusqlite::Transaction<'_>,
    spec: &CompleteRecoveryPointSpec,
) -> Result<bool, RepositoryError> {
    spec.validate()?;
    let plaintext_bytes = spec.plaintext_bytes()?;
    let calculated_axes = spec.snapshot_axes()?;
    let existing: Option<ExistingRecoveryPoint> = transaction
        .query_row(
            "SELECT
                 origin_instance_id, origin_occurrence_id, origin_backup_run_id,
                 source_target_identity, source_display_name, engine, state,
                 consistency_grade, committed_at_ms, set_manifest_checksum_blake3,
                 signing_key_id, signature_status, required_part_count, present_part_count
             FROM catalog_recovery_points
             WHERE id = ?1",
            [spec.id.as_str()],
            |row| {
                Ok(ExistingRecoveryPoint {
                    origin_instance_id: row.get(0)?,
                    origin_occurrence_id: row.get(1)?,
                    origin_backup_run_id: row.get(2)?,
                    source_target_identity: row.get(3)?,
                    source_display_name: row.get(4)?,
                    engine: row.get(5)?,
                    state: row.get(6)?,
                    consistency_grade: row.get(7)?,
                    committed_at_ms: row.get(8)?,
                    set_manifest_checksum_blake3: row.get(9)?,
                    signing_key_id: row.get(10)?,
                    signature_status: row.get(11)?,
                    required_part_count: row.get(12)?,
                    present_part_count: row.get(13)?,
                })
            },
        )
        .optional()?;
    if let Some(existing) = existing {
        ensure_existing_projection_matches(transaction, spec, &existing)?;
        ensure_recovery_point_workspace(transaction, spec)?;
        return Ok(false);
    }

    ensure_manifest_workspace(transaction, spec)?;

    transaction.execute(
        "INSERT INTO catalog_recovery_points(
             id, origin_instance_id, origin_occurrence_id, origin_backup_run_id,
             source_target_identity, source_display_name,
             engine, state, consistency_grade, committed_at_ms,
             manifest_schema_version, set_manifest_checksum_blake3, signing_key_id,
             signature_status, required_part_count, present_part_count,
             engine_metadata_json, plaintext_bytes, first_ingested_at_ms, last_ingested_at_ms
         ) VALUES (
             ?1, ?2, ?3, ?4,
             ?5, ?6,
             ?7, 'incomplete', ?8, ?9,
             1, ?10, ?11,
             'valid', 0, 0,
             '{}', ?13, ?12, ?12
         )",
        params![
            spec.id.as_str(),
            spec.origin_instance_id.as_str(),
            spec.origin_occurrence_id
                .as_ref()
                .map(PlanOccurrenceId::as_str),
            spec.origin_backup_run_id.as_ref().map(BackupRunId::as_str),
            spec.source_target_identity.as_str(),
            spec.source_display_name.as_str(),
            spec.engine.as_str(),
            spec.consistency_grade.as_str(),
            spec.committed_at_ms,
            spec.set_manifest_checksum_blake3.as_str(),
            spec.signing_key_id.as_str(),
            spec.ingested_at_ms,
            plaintext_bytes
        ],
    )?;
    transaction.execute(
        "UPDATE workspace_recovery_points
         SET workspace_id = ?1
         WHERE recovery_point_id = ?2",
        params![spec.workspace_id.as_str(), spec.id.as_str()],
    )?;
    for part in &spec.parts {
        transaction.execute(
            "INSERT INTO catalog_artifact_parts(
                 id, recovery_point_id, part_role, logical_name, required,
                 dump_format, compression_format, encryption_format,
                 plaintext_checksum_blake3, plaintext_bytes,
                 metadata_json, created_at_ms
             ) VALUES (
                 ?1, ?2, ?3, ?4, ?5,
                 ?6, ?7, ?8,
                 ?9, ?10,
                 '{}', ?11
             )",
            params![
                part.id.as_str(),
                spec.id.as_str(),
                artifact_part_role_str(part.role),
                part.logical_name.as_str(),
                part.required,
                part.dump_format.as_str(),
                part.compression_format.as_str(),
                part.encryption_format.as_str(),
                part.plaintext_checksum_blake3.as_str(),
                part.plaintext_bytes,
                part.created_at_ms
            ],
        )?;
    }
    for replica in &spec.replicas {
        transaction.execute(
            "INSERT INTO catalog_artifact_replicas(
                 id, recovery_point_id, destination_fingerprint, destination_kind,
                 requirement, state, immutability_capability, reader_status,
                 committed_at_ms, last_observed_at_ms, metadata_json
             ) VALUES (
                 ?1, ?2, ?3, ?4,
                 ?5, ?6, ?7, ?8,
                 ?9, ?10, '{}'
             )",
            params![
                replica.id.as_str(),
                spec.id.as_str(),
                replica.destination_fingerprint.as_str(),
                replica.destination_kind.as_str(),
                replica.requirement.as_str(),
                replica.state.as_str(),
                replica.immutability_capability.as_str(),
                replica.reader_status.as_str(),
                replica.committed_at_ms,
                replica.observed_at_ms
            ],
        )?;
    }
    for object in &spec.objects {
        transaction.execute(
            "INSERT INTO catalog_stored_objects(
                 id, recovery_point_id, artifact_replica_id, artifact_part_id,
                 object_role, object_key, object_version, checksum_blake3,
                 size_bytes, state, observed_at_ms, metadata_json
             ) VALUES (
                 ?1, ?2, ?3, ?4,
                 ?5, ?6, ?7, ?8,
                 ?9, 'present', ?10, '{}'
             )",
            params![
                object.id.as_str(),
                spec.id.as_str(),
                object.artifact_replica_id.as_str(),
                object.artifact_part_id.as_ref().map(ArtifactPartId::as_str),
                object.role.as_str(),
                object.object_key.as_str(),
                object.object_version.as_deref(),
                object.checksum_blake3.as_str(),
                object.size_bytes,
                object.observed_at_ms
            ],
        )?;
    }

    let (required_count, present_count): (i64, i64) = transaction.query_row(
        "SELECT
             COUNT(DISTINCT parts.id),
             COUNT(DISTINCT CASE
                 WHEN objects.state = 'present'
                  AND replicas.state = 'committed'
                  AND replicas.reader_status = 'eligible'
                 THEN parts.id
             END)
         FROM catalog_artifact_parts AS parts
         LEFT JOIN catalog_stored_objects AS objects
           ON objects.recovery_point_id = parts.recovery_point_id
          AND objects.artifact_part_id = parts.id
         LEFT JOIN catalog_artifact_replicas AS replicas
           ON replicas.recovery_point_id = objects.recovery_point_id
          AND replicas.id = objects.artifact_replica_id
         WHERE parts.recovery_point_id = ?1
           AND parts.required = 1",
        [spec.id.as_str()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if required_count == 0 || present_count < required_count {
        return Err(RepositoryError::RecoveryPointIncomplete);
    }
    let completed = transaction.execute(
        "UPDATE catalog_recovery_points
         SET state = 'complete',
             required_part_count = ?2,
             present_part_count = ?3,
             last_ingested_at_ms = ?4,
             plaintext_bytes = ?5
         WHERE id = ?1
           AND state = 'incomplete'
           AND signature_status = 'valid'",
        params![
            spec.id.as_str(),
            required_count,
            present_count,
            spec.ingested_at_ms,
            plaintext_bytes
        ],
    )?;
    if completed != 1 {
        return Err(RepositoryError::RecoveryPointIncomplete);
    }
    let search_text = format!(
        "{} {}",
        spec.source_display_name, spec.source_target_identity
    );
    transaction.execute(
        "INSERT INTO catalog_snapshot_projection(
             recovery_point_id, source_target_identity, source_display_name,
             engine, committed_at_ms, artifact_completeness, replica_durability,
             protection_compliance, restore_eligibility, search_text,
             projection_version, projected_at_ms
         ) VALUES (
             ?1, ?2, ?3,
             ?4, ?5, ?6, ?7,
             ?8, ?9, ?10,
             1, ?11
         )",
        params![
            spec.id.as_str(),
            spec.source_target_identity.as_str(),
            spec.source_display_name.as_str(),
            spec.engine.as_str(),
            spec.committed_at_ms,
            calculated_axes.artifact_completeness.as_str(),
            calculated_axes.replica_durability.as_str(),
            calculated_axes.protection_compliance.as_str(),
            calculated_axes.restore_eligibility.as_str(),
            search_text,
            spec.ingested_at_ms
        ],
    )?;
    Ok(true)
}

fn ensure_manifest_workspace(
    transaction: &rusqlite::Transaction<'_>,
    spec: &CompleteRecoveryPointSpec,
) -> Result<(), RepositoryError> {
    let exists: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM workspaces WHERE id = ?1)",
        [spec.workspace_id.as_str()],
        |row| row.get(0),
    )?;
    if exists {
        return Ok(());
    }
    if spec.workspace_id.as_str() == DEFAULT_WORKSPACE_ID {
        return Err(RepositoryError::CatalogProjectionInputInvalid);
    }

    let requested_name = spec.workspace_display_name.trim();
    let requested_key = requested_name.to_lowercase();
    let name_taken: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM workspaces WHERE normalized_name = ?1)",
        [&requested_key],
        |row| row.get(0),
    )?;
    let display_name = if name_taken {
        format!("Recovered {}", &spec.workspace_id.as_str()[..8])
    } else {
        requested_name.to_owned()
    };
    let normalized_name = display_name.to_lowercase();
    transaction.execute(
        "INSERT INTO workspaces(
             id, display_name, normalized_name, state, is_default, created_at_ms, updated_at_ms
         ) VALUES (?1, ?2, ?3, 'active', 0, ?4, ?4)",
        params![
            spec.workspace_id.as_str(),
            display_name,
            normalized_name,
            spec.ingested_at_ms
        ],
    )?;
    Ok(())
}

fn ensure_recovery_point_workspace(
    transaction: &rusqlite::Transaction<'_>,
    spec: &CompleteRecoveryPointSpec,
) -> Result<(), RepositoryError> {
    ensure_manifest_workspace(transaction, spec)?;
    let current = transaction
        .query_row(
            "SELECT workspace_id FROM workspace_recovery_points WHERE recovery_point_id = ?1",
            [spec.id.as_str()],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    match current {
        Some(workspace_id) if workspace_id == spec.workspace_id.as_str() => Ok(()),
        Some(_) => Err(RepositoryError::CatalogProjectionInputInvalid),
        None => {
            transaction.execute(
                "INSERT INTO workspace_recovery_points(
                     workspace_id, recovery_point_id, created_at_ms
                 ) VALUES (?1, ?2, ?3)",
                params![
                    spec.workspace_id.as_str(),
                    spec.id.as_str(),
                    spec.ingested_at_ms
                ],
            )?;
            Ok(())
        }
    }
}

struct ExistingRecoveryPoint {
    origin_instance_id: String,
    origin_occurrence_id: Option<String>,
    origin_backup_run_id: Option<String>,
    source_target_identity: String,
    source_display_name: String,
    engine: String,
    state: String,
    consistency_grade: String,
    committed_at_ms: Option<i64>,
    set_manifest_checksum_blake3: String,
    signing_key_id: String,
    signature_status: String,
    required_part_count: i64,
    present_part_count: i64,
}

fn ensure_existing_projection_matches(
    transaction: &rusqlite::Transaction<'_>,
    spec: &CompleteRecoveryPointSpec,
    existing: &ExistingRecoveryPoint,
) -> Result<(), RepositoryError> {
    let axes = spec.snapshot_axes()?;
    let expected_occurrence = spec
        .origin_occurrence_id
        .as_ref()
        .map(PlanOccurrenceId::as_str);
    let expected_backup = spec.origin_backup_run_id.as_ref().map(BackupRunId::as_str);
    if existing.origin_instance_id != spec.origin_instance_id
        || existing.origin_occurrence_id.as_deref() != expected_occurrence
        || existing.origin_backup_run_id.as_deref() != expected_backup
        || existing.source_target_identity != spec.source_target_identity
        || existing.source_display_name != spec.source_display_name
        || existing.engine != spec.engine
        || existing.state != "complete"
        || existing.consistency_grade != spec.consistency_grade
        || existing.committed_at_ms != Some(spec.committed_at_ms)
        || existing.set_manifest_checksum_blake3 != spec.set_manifest_checksum_blake3
        || existing.signing_key_id != spec.signing_key_id
        || existing.signature_status != "valid"
    {
        return Err(RepositoryError::CatalogProjectionInputInvalid);
    }
    let stored_axes: Option<(String, String, String, String)> = transaction
        .query_row(
            "SELECT artifact_completeness, replica_durability,
                    protection_compliance, restore_eligibility
             FROM catalog_snapshot_projection
             WHERE recovery_point_id = ?1",
            [spec.id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let expected_axes = (
        axes.artifact_completeness.as_str(),
        axes.replica_durability.as_str(),
        axes.protection_compliance.as_str(),
        axes.restore_eligibility.as_str(),
    );
    if stored_axes.as_ref().map(|value| {
        (
            value.0.as_str(),
            value.1.as_str(),
            value.2.as_str(),
            value.3.as_str(),
        )
    }) != Some(expected_axes)
    {
        return Err(RepositoryError::CatalogProjectionInputInvalid);
    }
    let counts: (i64, i64, i64) = transaction.query_row(
        "SELECT
             (SELECT COUNT(*) FROM catalog_artifact_parts WHERE recovery_point_id = ?1),
             (SELECT COUNT(*) FROM catalog_artifact_replicas WHERE recovery_point_id = ?1),
             (SELECT COUNT(*) FROM catalog_stored_objects WHERE recovery_point_id = ?1)",
        [spec.id.as_str()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let expected_part_count = i64::try_from(spec.parts.iter().filter(|part| part.required).count())
        .map_err(|_| RepositoryError::CatalogProjectionInputInvalid)?;
    let expected_total_part_count = i64::try_from(spec.parts.len())
        .map_err(|_| RepositoryError::CatalogProjectionInputInvalid)?;
    let expected_replica_count = i64::try_from(spec.replicas.len())
        .map_err(|_| RepositoryError::CatalogProjectionInputInvalid)?;
    let expected_object_count = i64::try_from(spec.objects.len())
        .map_err(|_| RepositoryError::CatalogProjectionInputInvalid)?;
    if existing.required_part_count != expected_part_count
        || existing.present_part_count != expected_part_count
        || counts
            != (
                expected_total_part_count,
                expected_replica_count,
                expected_object_count,
            )
    {
        return Err(RepositoryError::CatalogProjectionInputInvalid);
    }
    Ok(())
}

const REBUILDABLE_TABLES: [&str; 9] = [
    "workspace_recovery_points",
    "catalog_recovery_points",
    "catalog_artifact_parts",
    "catalog_artifact_replicas",
    "catalog_stored_objects",
    "catalog_snapshot_projection",
    "catalog_target_protection_projection",
    "catalog_verification_projection",
    "catalog_manifest_ingest",
];

const CONTROL_DIGEST_QUERIES: [&str; 10] = [
    "SELECT printf('connection|%s|%s|%s', id, state, source_resource_key)
     FROM connection_profiles ORDER BY id",
    "SELECT printf('target|%s|%s|%s|%s', id, connection_profile_id, state, source_fingerprint)
     FROM database_targets ORDER BY id",
    "SELECT printf('plan|%s|%s|%s', id, state, COALESCE(current_revision_id, ''))
     FROM backup_plans ORDER BY id",
    "SELECT printf('revision|%s|%s|%s', id, backup_plan_id, COALESCE(published_at_ms, -1))
     FROM backup_plan_revisions ORDER BY id",
    "SELECT printf('occurrence|%s|%s|%s|%s', id, plan_revision_id, origin, state)
     FROM plan_occurrences ORDER BY id",
    "SELECT printf('backup-run|%s|%s|%s|%s|%s', id, plan_occurrence_id,
                   database_target_id, state, COALESCE(recovery_point_id, ''))
     FROM backup_runs ORDER BY id",
    "SELECT printf('work|%s|%s|%s|%s|%s', id, kind, state, current_attempt_no, fencing_token)
     FROM work_items ORDER BY id",
    "SELECT printf('attempt|%s|%s|%s|%s|%s', id, work_item_id, attempt_no, fencing_token, state)
     FROM work_attempts ORDER BY work_item_id, attempt_no",
    "SELECT printf('restore|%s|%s|%s', id, recovery_point_id, state)
     FROM restore_runs ORDER BY id",
    "SELECT printf('verification|%s|%s|%s|%s', id, recovery_point_id, requested_level, state)
     FROM verification_runs ORDER BY id",
];

fn count_rows(connection: &rusqlite::Connection, table: &str) -> Result<i64, RepositoryError> {
    connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .map_err(RepositoryError::from)
}

fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_format(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-' | b'.')
        })
}

const fn artifact_part_role_str(role: ArtifactPartRole) -> &'static str {
    match role {
        ArtifactPartRole::DatabaseDump => "database_dump",
        ArtifactPartRole::Globals => "globals",
        ArtifactPartRole::Metadata => "metadata",
        ArtifactPartRole::Wal => "wal",
        ArtifactPartRole::Auxiliary => "auxiliary",
    }
}

const fn engine_str(engine: Engine) -> &'static str {
    match engine {
        Engine::Postgresql => "postgresql",
        Engine::Mysql => "mysql",
        Engine::Mariadb => "mariadb",
        Engine::Sqlserver => "sqlserver",
        Engine::Sqlite => "sqlite",
    }
}

const fn consistency_grade_str(grade: ConsistencyGrade) -> &'static str {
    match grade {
        ConsistencyGrade::CrashConsistent => "crash_consistent",
        ConsistencyGrade::ApplicationConsistent => "application_consistent",
        ConsistencyGrade::TransactionConsistent => "transaction_consistent",
    }
}

const fn map_replica_requirement(requirement: ReplicaRequirementV2) -> DestinationRequirement {
    match requirement {
        ReplicaRequirementV2::Required => DestinationRequirement::Required,
        ReplicaRequirementV2::BestEffort => DestinationRequirement::BestEffort,
    }
}

const fn map_reader_status(status: ReaderStatusV2) -> ReplicaReaderStatus {
    match status {
        ReaderStatusV2::Eligible => ReplicaReaderStatus::Eligible,
        ReaderStatusV2::Unavailable => ReplicaReaderStatus::Unavailable,
        ReaderStatusV2::Unverified => ReplicaReaderStatus::Unverified,
    }
}

fn replica_state_from_outcome(outcome: &ReplicaPublishOutcomeV2) -> ArtifactReplicaState {
    if outcome.result.is_ok() {
        return ArtifactReplicaState::Committed;
    }
    match outcome.result.as_ref().expect_err("failed outcome") {
        ReplicaPublishError::ReaderChecksumMismatch
        | ReplicaPublishError::ManifestRoundTripInvalid => ArtifactReplicaState::Corrupt,
        _ if outcome.manifest_receipt.is_some() || !outcome.payload_receipts.is_empty() => {
            ArtifactReplicaState::Degraded
        }
        _ => ArtifactReplicaState::Missing,
    }
}
