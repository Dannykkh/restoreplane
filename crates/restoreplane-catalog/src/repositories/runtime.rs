use std::str::FromStr;

use ed25519_dalek::VerifyingKey;
use restoreplane_domain::{
    transition_backup_run, transition_plan_occurrence, transition_work_attempt,
    transition_work_item, BackupRunCommand, BackupRunId, BackupRunState, BackupScopeId,
    CatalogRebuildRunId, ConnectionProfileId, DatabaseTargetId, DestinationId, OccurrenceCommand,
    OccurrenceIdentity, OccurrenceOrigin, OperationRunId, PlanOccurrenceId, PlanOccurrenceState,
    PlanRevisionId, RecoveryPointId, RestoreRunId, SecretRefId, VerificationRunId,
    WorkAttemptCommand, WorkAttemptId, WorkAttemptState, WorkItemCommand, WorkItemId, WorkItemKind,
    WorkItemState,
};
use restoreplane_pipeline::{
    ConfirmedRetentionDeleteV2, DeleteObjectDispositionV2, ReaderStatusV2, ReplicaPublishOutcomeV2,
    ReplicaRequirementV2, RetentionDeleteAuthorizationV2, RetentionDeleteReportV2,
    RetentionObjectRoleV2, SignedSetManifestV2,
};
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};
use uuid::Uuid;

use crate::OperationalStore;

use super::retention_runtime::validate_automatic_policy_authorization_in_transaction;
use super::verification::schedule_post_backup_in_transaction;
use super::{
    canonical_server_identity,
    catalog_projection::{
        ingest_complete_projection, project_terminal_verification_run,
        rebuild_verification_projections_from_runs, CompleteRecoveryPointSpec,
    },
    rebuild_v2::{
        CatalogRebuildApplyModeV2, CatalogRebuildCandidateV2, CatalogRebuildDispositionV2,
        CatalogRebuildPlanV2, CatalogRebuildRejectReasonV2,
    },
    server_identity_from_capabilities, ReplicaRepairSecretRefsV2, RepositoryError,
};

type VerificationClaimRow = (String, Option<String>, i64, Option<String>, String, String);
type RestoreClaimRow = (
    String,
    Option<String>,
    String,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    String,
);

#[derive(Clone, Copy, Default)]
struct RestoreTargetObservation<'a> {
    server_version: Option<&'a str>,
    server_identity: Option<&'a str>,
    target_exists: Option<bool>,
}
type ExpiredClaimReconciliationRow = (
    String,
    i64,
    i64,
    String,
    i64,
    Option<i64>,
    Option<String>,
    Option<String>,
);

struct BackupCommitManifestBindingRow {
    occurrence_id: String,
    engine: String,
    backup_state: String,
    recovery_point_id: Option<String>,
    backup_database_target_id: String,
    backup_source_fingerprint: String,
    scope_database_target_id: String,
    backup_scope_id: String,
    scope_mode: String,
    scope_source_fingerprint: String,
    resolved_object_count: i64,
    scope_snapshot_json: String,
    scope_snapshot_hash_blake3: String,
}

#[derive(Clone, Debug)]
pub struct OccurrenceSpec {
    id: PlanOccurrenceId,
    plan_revision_id: PlanRevisionId,
    origin: OccurrenceOrigin,
    scheduled_for_ms: Option<i64>,
    request_idempotency_key: Option<String>,
    requested_at_ms: i64,
}

impl OccurrenceSpec {
    pub fn scheduled(
        id: PlanOccurrenceId,
        plan_revision_id: PlanRevisionId,
        origin: OccurrenceOrigin,
        scheduled_for_ms: i64,
        requested_at_ms: i64,
    ) -> Self {
        Self {
            id,
            plan_revision_id,
            origin,
            scheduled_for_ms: Some(scheduled_for_ms),
            request_idempotency_key: None,
            requested_at_ms,
        }
    }

    pub fn manual(
        id: PlanOccurrenceId,
        plan_revision_id: PlanRevisionId,
        request_idempotency_key: impl Into<String>,
        requested_at_ms: i64,
    ) -> Self {
        Self {
            id,
            plan_revision_id,
            origin: OccurrenceOrigin::Manual,
            scheduled_for_ms: None,
            request_idempotency_key: Some(request_idempotency_key.into()),
            requested_at_ms,
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        match (
            self.origin,
            self.scheduled_for_ms,
            self.request_idempotency_key.as_deref(),
        ) {
            (
                OccurrenceOrigin::Scheduled | OccurrenceOrigin::CatchUp,
                Some(scheduled_for_ms),
                None,
            ) => {
                OccurrenceIdentity::scheduled(
                    self.plan_revision_id.clone(),
                    self.origin,
                    scheduled_for_ms,
                )?;
            }
            (OccurrenceOrigin::Manual, None, Some(key)) => {
                OccurrenceIdentity::manual(key)?;
            }
            _ => return Err(RepositoryError::RuntimeStateInvalid),
        }
        if self.requested_at_ms < 0 {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OccurrenceUpsert {
    id: PlanOccurrenceId,
    created: bool,
}

impl OccurrenceUpsert {
    pub fn id(&self) -> &PlanOccurrenceId {
        &self.id
    }

    pub const fn created(&self) -> bool {
        self.created
    }
}

#[derive(Clone, Debug)]
pub struct OccurrenceMaterializationSpec {
    plan_revision_id: PlanRevisionId,
    origin: OccurrenceOrigin,
    scheduled_for_ms: i64,
    requested_at_ms: i64,
    missed_reason: Option<String>,
}

impl OccurrenceMaterializationSpec {
    pub fn due(
        plan_revision_id: PlanRevisionId,
        origin: OccurrenceOrigin,
        scheduled_for_ms: i64,
        requested_at_ms: i64,
    ) -> Self {
        Self {
            plan_revision_id,
            origin,
            scheduled_for_ms,
            requested_at_ms,
            missed_reason: None,
        }
    }

    pub fn missed(
        plan_revision_id: PlanRevisionId,
        origin: OccurrenceOrigin,
        scheduled_for_ms: i64,
        requested_at_ms: i64,
        missed_reason: impl Into<String>,
    ) -> Self {
        Self {
            plan_revision_id,
            origin,
            scheduled_for_ms,
            requested_at_ms,
            missed_reason: Some(missed_reason.into()),
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        OccurrenceIdentity::scheduled(
            self.plan_revision_id.clone(),
            self.origin,
            self.scheduled_for_ms,
        )?;
        if self.requested_at_ms < 0
            || self.requested_at_ms < self.scheduled_for_ms
            || self
                .missed_reason
                .as_deref()
                .is_some_and(|reason| reason.is_empty() || reason.len() > 256)
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OccurrenceMaterialization {
    occurrence_id: PlanOccurrenceId,
    occurrence_created: bool,
    backup_runs_created: usize,
    work_items_created: usize,
}

impl OccurrenceMaterialization {
    pub fn occurrence_id(&self) -> &PlanOccurrenceId {
        &self.occurrence_id
    }

    pub const fn occurrence_created(&self) -> bool {
        self.occurrence_created
    }

    pub const fn backup_runs_created(&self) -> usize {
        self.backup_runs_created
    }

    pub const fn work_items_created(&self) -> usize {
        self.work_items_created
    }
}

#[derive(Clone, Debug)]
pub struct BackupWorkSpec {
    backup_run_id: BackupRunId,
    work_item_id: WorkItemId,
    plan_occurrence_id: PlanOccurrenceId,
    plan_revision_id: PlanRevisionId,
    database_target_id: DatabaseTargetId,
    engine: String,
    source_fingerprint: String,
    available_at_ms: i64,
    priority: i64,
    attempt_limit: i64,
}

impl BackupWorkSpec {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        backup_run_id: BackupRunId,
        work_item_id: WorkItemId,
        plan_occurrence_id: PlanOccurrenceId,
        plan_revision_id: PlanRevisionId,
        database_target_id: DatabaseTargetId,
        engine: impl Into<String>,
        source_fingerprint: impl Into<String>,
        available_at_ms: i64,
    ) -> Self {
        Self {
            backup_run_id,
            work_item_id,
            plan_occurrence_id,
            plan_revision_id,
            database_target_id,
            engine: engine.into(),
            source_fingerprint: source_fingerprint.into(),
            available_at_ms,
            priority: 100,
            attempt_limit: 3,
        }
    }
}

#[derive(Clone, Debug)]
pub struct OperationWorkSpec {
    operation_run_id: OperationRunId,
    work_item_id: WorkItemId,
    operation_kind: String,
    subject_type: String,
    subject_id: String,
    request_json: String,
    available_at_ms: i64,
    priority: i64,
    attempt_limit: i64,
}

impl OperationWorkSpec {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        operation_run_id: OperationRunId,
        work_item_id: WorkItemId,
        operation_kind: impl Into<String>,
        subject_type: impl Into<String>,
        subject_id: impl Into<String>,
        request_json: impl Into<String>,
        available_at_ms: i64,
    ) -> Self {
        Self {
            operation_run_id,
            work_item_id,
            operation_kind: operation_kind.into(),
            subject_type: subject_type.into(),
            subject_id: subject_id.into(),
            request_json: request_json.into(),
            available_at_ms,
            priority: 100,
            attempt_limit: 3,
        }
    }

    pub(super) fn validate(&self) -> Result<(), RepositoryError> {
        if !is_operation_kind(&self.operation_kind)
            || self.subject_type.is_empty()
            || self.subject_type.len() > 128
            || self.subject_id.is_empty()
            || self.subject_id.len() > 1024
            || self.request_json.len() > 1024 * 1024
            || serde_json::from_str::<serde_json::Value>(&self.request_json).is_err()
            || self.available_at_ms < 0
            || !(0..=1000).contains(&self.priority)
            || self.attempt_limit <= 0
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogRebuildSourceKindV2 {
    Local,
    S3,
    SshSftp,
    AllConfigured,
}

impl CatalogRebuildSourceKindV2 {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::S3 => "s3",
            Self::SshSftp => "ssh_sftp",
            Self::AllConfigured => "all_configured",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogRebuildRunModeV2 {
    DryRun,
    Merge,
    ReplaceProjection,
}

impl CatalogRebuildRunModeV2 {
    const fn as_str(self) -> &'static str {
        match self {
            Self::DryRun => "dry_run",
            Self::Merge => "merge",
            Self::ReplaceProjection => "replace_projection",
        }
    }
}

#[derive(Clone, Debug)]
pub struct CatalogRebuildWorkSpec {
    catalog_rebuild_run_id: CatalogRebuildRunId,
    work_item_id: WorkItemId,
    destination_id: Option<DestinationId>,
    source_kind: CatalogRebuildSourceKindV2,
    mode: CatalogRebuildRunModeV2,
    available_at_ms: i64,
    priority: i64,
    attempt_limit: i64,
}

impl CatalogRebuildWorkSpec {
    pub fn new(
        catalog_rebuild_run_id: CatalogRebuildRunId,
        work_item_id: WorkItemId,
        destination_id: Option<DestinationId>,
        source_kind: CatalogRebuildSourceKindV2,
        mode: CatalogRebuildRunModeV2,
        available_at_ms: i64,
    ) -> Self {
        Self {
            catalog_rebuild_run_id,
            work_item_id,
            destination_id,
            source_kind,
            mode,
            available_at_ms,
            priority: 200,
            attempt_limit: 3,
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if self.available_at_ms < 0
            || !(0..=1000).contains(&self.priority)
            || self.attempt_limit <= 0
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogRebuildWorkUpsert {
    catalog_rebuild_run_id: CatalogRebuildRunId,
    work_item_id: WorkItemId,
    created: bool,
}

impl CatalogRebuildWorkUpsert {
    pub fn catalog_rebuild_run_id(&self) -> &CatalogRebuildRunId {
        &self.catalog_rebuild_run_id
    }

    pub fn work_item_id(&self) -> &WorkItemId {
        &self.work_item_id
    }

    pub const fn created(&self) -> bool {
        self.created
    }
}

struct ExistingCatalogRebuildWork {
    destination_id: Option<String>,
    source_kind: String,
    mode: String,
    work_item_id: Option<String>,
    priority: Option<i64>,
    available_at_ms: Option<i64>,
    attempt_limit: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct ReplicaRepairWorkSpec {
    recovery_point_id: RecoveryPointId,
    source_destination_fingerprint: String,
    target_destination_fingerprint: String,
    secret_refs: ReplicaRepairSecretRefsV2,
    available_at_ms: i64,
}

impl ReplicaRepairWorkSpec {
    pub fn new(
        recovery_point_id: RecoveryPointId,
        source_destination_fingerprint: impl Into<String>,
        target_destination_fingerprint: impl Into<String>,
        secret_refs: ReplicaRepairSecretRefsV2,
        available_at_ms: i64,
    ) -> Self {
        Self {
            recovery_point_id,
            source_destination_fingerprint: source_destination_fingerprint.into(),
            target_destination_fingerprint: target_destination_fingerprint.into(),
            secret_refs,
            available_at_ms,
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if self.source_destination_fingerprint.trim().is_empty()
            || self.source_destination_fingerprint.len() > 512
            || self.target_destination_fingerprint.trim().is_empty()
            || self.target_destination_fingerprint.len() > 512
            || self.source_destination_fingerprint == self.target_destination_fingerprint
            || self.available_at_ms < 0
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        Ok(())
    }

    pub(super) fn operation_work(&self) -> Result<OperationWorkSpec, RepositoryError> {
        self.validate()?;
        let identity = replica_repair_identity_v2(
            &self.source_destination_fingerprint,
            &self.target_destination_fingerprint,
            self.secret_refs.source_reader_secret_ref_id(),
            self.secret_refs.target_writer_secret_ref_id(),
            self.secret_refs.target_reader_secret_ref_id(),
        );
        let operation_run_id =
            OperationRunId::from_stable_name(self.recovery_point_id.as_uuid(), identity.as_bytes());
        let work_item_id = WorkItemId::from_stable_name(
            self.recovery_point_id.as_uuid(),
            format!("{identity}\0work").as_bytes(),
        );
        let request_json = serde_json::to_string(&serde_json::json!({
            "recovery_point_id": self.recovery_point_id.as_str(),
            "schema_version": 2,
            "source_reader_secret_ref_id": self
                .secret_refs
                .source_reader_secret_ref_id()
                .map(SecretRefId::as_str),
            "source_destination_fingerprint": self.source_destination_fingerprint,
            "target_reader_secret_ref_id": self
                .secret_refs
                .target_reader_secret_ref_id()
                .map(SecretRefId::as_str),
            "target_destination_fingerprint": self.target_destination_fingerprint,
            "target_writer_secret_ref_id": self
                .secret_refs
                .target_writer_secret_ref_id()
                .map(SecretRefId::as_str),
        }))
        .map_err(|_| RepositoryError::RuntimeStateInvalid)?;
        Ok(OperationWorkSpec::new(
            operation_run_id,
            work_item_id,
            "replica_repair",
            "recovery_point",
            self.recovery_point_id.as_str(),
            request_json,
            self.available_at_ms,
        ))
    }
}

fn replica_repair_identity_v2(
    source_destination_fingerprint: &str,
    target_destination_fingerprint: &str,
    source_reader_secret_ref_id: Option<&SecretRefId>,
    target_writer_secret_ref_id: Option<&SecretRefId>,
    target_reader_secret_ref_id: Option<&SecretRefId>,
) -> String {
    format!(
        "replica-repair\0{}\0{}\0{}\0{}\0{}",
        source_destination_fingerprint,
        target_destination_fingerprint,
        optional_secret_ref_identity(source_reader_secret_ref_id),
        optional_secret_ref_identity(target_writer_secret_ref_id),
        optional_secret_ref_identity(target_reader_secret_ref_id),
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationWorkUpsert {
    operation_run_id: OperationRunId,
    work_item_id: WorkItemId,
    created: bool,
}

impl OperationWorkUpsert {
    pub fn operation_run_id(&self) -> &OperationRunId {
        &self.operation_run_id
    }

    pub fn work_item_id(&self) -> &WorkItemId {
        &self.work_item_id
    }

    pub const fn created(&self) -> bool {
        self.created
    }
}

struct ExistingOperationWork {
    operation_kind: String,
    subject_type: String,
    subject_id: String,
    request_json: String,
    work_item_id: Option<String>,
    priority: Option<i64>,
    available_at_ms: Option<i64>,
    attempt_limit: Option<i64>,
}

pub(super) fn insert_operation_work_in_transaction(
    transaction: &Transaction<'_>,
    spec: &OperationWorkSpec,
) -> Result<OperationWorkUpsert, RepositoryError> {
    spec.validate()?;
    let existing: Option<ExistingOperationWork> = transaction
        .query_row(
            "SELECT
                 operation_kind, subject_type, subject_id, request_json,
                 work_items.id, work_items.priority,
                 work_items.available_at_ms, work_items.attempt_limit
             FROM operation_runs
             LEFT JOIN work_items
               ON work_items.operation_run_id = operation_runs.id
             WHERE operation_runs.id = ?1",
            [spec.operation_run_id.as_str()],
            |row| {
                Ok(ExistingOperationWork {
                    operation_kind: row.get(0)?,
                    subject_type: row.get(1)?,
                    subject_id: row.get(2)?,
                    request_json: row.get(3)?,
                    work_item_id: row.get(4)?,
                    priority: row.get(5)?,
                    available_at_ms: row.get(6)?,
                    attempt_limit: row.get(7)?,
                })
            },
        )
        .optional()?;
    if let Some(existing) = existing {
        if existing.operation_kind != spec.operation_kind
            || existing.subject_type != spec.subject_type
            || existing.subject_id != spec.subject_id
            || existing.request_json != spec.request_json
            || existing.work_item_id.as_deref() != Some(spec.work_item_id.as_str())
            || existing.priority != Some(spec.priority)
            || existing.available_at_ms != Some(spec.available_at_ms)
            || existing.attempt_limit != Some(spec.attempt_limit)
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        return Ok(OperationWorkUpsert {
            operation_run_id: spec.operation_run_id.clone(),
            work_item_id: spec.work_item_id.clone(),
            created: false,
        });
    }
    let work_id_exists: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_items WHERE id = ?1)",
        [spec.work_item_id.as_str()],
        |row| row.get(0),
    )?;
    if work_id_exists {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    transaction.execute(
        "INSERT INTO operation_runs(
             id, operation_kind, subject_type, subject_id, request_json,
             state, created_at_ms, updated_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6, ?6)",
        params![
            spec.operation_run_id.as_str(),
            spec.operation_kind,
            spec.subject_type,
            spec.subject_id,
            spec.request_json,
            spec.available_at_ms
        ],
    )?;
    transaction.execute(
        "INSERT INTO work_items(
             id, kind, operation_run_id, priority, state,
             available_at_ms, attempt_limit, created_at_ms, updated_at_ms
         ) VALUES (?1, 'operation', ?2, ?3, 'queued', ?4, ?5, ?4, ?4)",
        params![
            spec.work_item_id.as_str(),
            spec.operation_run_id.as_str(),
            spec.priority,
            spec.available_at_ms,
            spec.attempt_limit
        ],
    )?;
    Ok(OperationWorkUpsert {
        operation_run_id: spec.operation_run_id.clone(),
        work_item_id: spec.work_item_id.clone(),
        created: true,
    })
}

#[derive(Clone, Debug)]
pub struct ResourcePoolSpec {
    id: String,
    resource_type: String,
    resource_key: String,
    capacity: i64,
    created_at_ms: i64,
}

impl ResourcePoolSpec {
    pub fn new(
        id: impl Into<String>,
        resource_type: impl Into<String>,
        resource_key: impl Into<String>,
        capacity: i64,
        created_at_ms: i64,
    ) -> Self {
        Self {
            id: id.into(),
            resource_type: resource_type.into(),
            resource_key: resource_key.into(),
            capacity,
            created_at_ms,
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if !is_canonical_uuid(&self.id)
            || !is_resource_type(&self.resource_type)
            || self.resource_key.is_empty()
            || self.resource_key.len() > 1024
            || self.capacity <= 0
            || self.created_at_ms < 0
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct SpoolVolumeSpec {
    id: String,
    volume_key: String,
    root_path: String,
    hard_limit_bytes: i64,
    soft_limit_bytes: i64,
    reserve_floor_bytes: i64,
    last_free_bytes: i64,
    observed_at_ms: i64,
}

impl SpoolVolumeSpec {
    #[allow(clippy::too_many_arguments)]
    pub fn ready(
        id: impl Into<String>,
        volume_key: impl Into<String>,
        root_path: impl Into<String>,
        hard_limit_bytes: i64,
        soft_limit_bytes: i64,
        reserve_floor_bytes: i64,
        last_free_bytes: i64,
        observed_at_ms: i64,
    ) -> Self {
        Self {
            id: id.into(),
            volume_key: volume_key.into(),
            root_path: root_path.into(),
            hard_limit_bytes,
            soft_limit_bytes,
            reserve_floor_bytes,
            last_free_bytes,
            observed_at_ms,
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if !is_canonical_uuid(&self.id)
            || self.volume_key.is_empty()
            || self.root_path.is_empty()
            || self.hard_limit_bytes <= 0
            || self.soft_limit_bytes <= 0
            || self.soft_limit_bytes > self.hard_limit_bytes
            || self.reserve_floor_bytes < 0
            || self.last_free_bytes < 0
            || self.observed_at_ms < 0
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ResourceRequirement {
    resource_pool_id: String,
    units: i64,
}

impl ResourceRequirement {
    pub fn new(resource_pool_id: impl Into<String>, units: i64) -> Self {
        Self {
            resource_pool_id: resource_pool_id.into(),
            units,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SpoolRequirement {
    spool_volume_id: String,
    reserved_bytes: i64,
    spool_path: String,
}

impl SpoolRequirement {
    pub fn new(
        spool_volume_id: impl Into<String>,
        reserved_bytes: i64,
        spool_path: impl Into<String>,
    ) -> Self {
        Self {
            spool_volume_id: spool_volume_id.into(),
            reserved_bytes,
            spool_path: spool_path.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ClaimRequest {
    worker_id: String,
    worker_boot_id: String,
    now_ms: i64,
    lease_duration_ms: i64,
    kind_filter: Option<WorkItemKind>,
    operation_kind_filter: Option<Vec<String>>,
}

impl ClaimRequest {
    pub fn new(
        worker_id: impl Into<String>,
        worker_boot_id: impl Into<String>,
        now_ms: i64,
        lease_duration_ms: i64,
    ) -> Self {
        Self {
            worker_id: worker_id.into(),
            worker_boot_id: worker_boot_id.into(),
            now_ms,
            lease_duration_ms,
            kind_filter: None,
            operation_kind_filter: None,
        }
    }

    pub fn for_kind(mut self, kind: WorkItemKind) -> Self {
        self.kind_filter = Some(kind);
        self
    }

    pub fn for_operation_kinds(
        mut self,
        operation_kinds: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.kind_filter = Some(WorkItemKind::Operation);
        self.operation_kind_filter = Some(operation_kinds.into_iter().map(Into::into).collect());
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimedWork {
    work_item_id: WorkItemId,
    kind: WorkItemKind,
    attempt_id: WorkAttemptId,
    attempt_no: i64,
    fencing_token: i64,
    worker_id: String,
    worker_boot_id: String,
}

impl ClaimedWork {
    pub fn work_item_id(&self) -> &WorkItemId {
        &self.work_item_id
    }

    pub fn attempt_id(&self) -> &WorkAttemptId {
        &self.attempt_id
    }

    pub const fn kind(&self) -> WorkItemKind {
        self.kind
    }

    pub const fn attempt_no(&self) -> i64 {
        self.attempt_no
    }

    pub const fn fencing_token(&self) -> i64 {
        self.fencing_token
    }

    pub fn worker_id(&self) -> &str {
        &self.worker_id
    }

    pub fn worker_boot_id(&self) -> &str {
        &self.worker_boot_id
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationClaimContext {
    operation_run_id: OperationRunId,
    operation_kind: String,
    subject_type: String,
    subject_id: String,
    request_json: String,
}

impl OperationClaimContext {
    pub fn operation_run_id(&self) -> &OperationRunId {
        &self.operation_run_id
    }

    pub fn operation_kind(&self) -> &str {
        &self.operation_kind
    }

    pub fn subject_type(&self) -> &str {
        &self.subject_type
    }

    pub fn subject_id(&self) -> &str {
        &self.subject_id
    }

    pub fn request_json(&self) -> &str {
        &self.request_json
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupClaimContext {
    backup_run_id: BackupRunId,
    plan_occurrence_id: PlanOccurrenceId,
    plan_revision_id: PlanRevisionId,
    database_target_id: DatabaseTargetId,
    backup_scope_id: BackupScopeId,
    connection_profile_id: ConnectionProfileId,
    engine: String,
    engine_object_id: String,
    target_display_name: String,
    scope_display_name: String,
    scope_mode: String,
    source_fingerprint: String,
    database_source_fingerprint: String,
    resolved_object_count: i64,
    scope_snapshot_json: String,
    scope_snapshot_hash_blake3: String,
    target_options_json: String,
    execution_policy_json: String,
}

/// Immutable identity needed to verify an expired Backup commit without
/// accepting a stale worker fence or depending on an unbounded destination scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupCommitRecoveryContext {
    backup_run_id: BackupRunId,
    plan_occurrence_id: PlanOccurrenceId,
    plan_revision_id: PlanRevisionId,
    backup_scope_id: BackupScopeId,
    recovery_point_id: Option<RecoveryPointId>,
    engine: String,
    backup_database_target_id: String,
    backup_source_fingerprint: String,
    scope_database_target_id: String,
    scope_mode: String,
    scope_source_fingerprint: String,
    resolved_object_count: i64,
    scope_snapshot_json: String,
    scope_snapshot_hash_blake3: String,
}

impl BackupCommitRecoveryContext {
    pub fn backup_run_id(&self) -> &BackupRunId {
        &self.backup_run_id
    }

    pub fn plan_occurrence_id(&self) -> &PlanOccurrenceId {
        &self.plan_occurrence_id
    }

    pub fn plan_revision_id(&self) -> &PlanRevisionId {
        &self.plan_revision_id
    }

    pub fn backup_scope_id(&self) -> &BackupScopeId {
        &self.backup_scope_id
    }

    pub fn recovery_point_id(&self) -> Option<&RecoveryPointId> {
        self.recovery_point_id.as_ref()
    }

    pub fn engine(&self) -> &str {
        &self.engine
    }

    pub fn manifest_object_key(&self) -> Option<String> {
        self.recovery_point_id.as_ref().map(|recovery_point_id| {
            format!(
                "v2/{}/{}/{}/set-manifest.json",
                self.plan_occurrence_id, self.backup_scope_id, recovery_point_id
            )
        })
    }

    /// Confirms that a fully verified manifest projection is bound to the
    /// immutable backup/scope facts selected by this exact fenced attempt.
    pub fn matches_verified_spec(&self, spec: &CompleteRecoveryPointSpec) -> bool {
        let Ok(snapshot) = serde_json::from_str::<serde_json::Value>(&self.scope_snapshot_json)
        else {
            return false;
        };
        let Some(snapshot_mode) = snapshot.get("mode").and_then(serde_json::Value::as_str) else {
            return false;
        };
        let Some(snapshot_objects) = snapshot
            .get("objects")
            .and_then(serde_json::Value::as_array)
        else {
            return false;
        };
        let Ok(snapshot_object_count) = i64::try_from(snapshot_objects.len()) else {
            return false;
        };
        self.backup_database_target_id == self.scope_database_target_id
            && self.backup_source_fingerprint == self.scope_source_fingerprint
            && snapshot_mode == self.scope_mode
            && snapshot_object_count == self.resolved_object_count
            && blake3::hash(self.scope_snapshot_json.as_bytes())
                .to_hex()
                .as_str()
                == self.scope_snapshot_hash_blake3
            && spec.matches_backup_origin(
                self.plan_occurrence_id.as_str(),
                self.backup_run_id.as_str(),
                &self.engine,
            )
            && spec.matches_backup_scope(
                self.backup_scope_id.as_str(),
                &self.scope_source_fingerprint,
                &self.scope_mode,
                &self.scope_snapshot_hash_blake3,
                self.resolved_object_count,
            )
            && self
                .recovery_point_id
                .as_ref()
                .is_none_or(|value| value == spec.id())
    }
}

struct BackupClaimContextRow {
    plan_occurrence_id: String,
    plan_revision_id: String,
    database_target_id: String,
    backup_scope_id: String,
    connection_profile_id: String,
    engine: String,
    engine_object_id: String,
    target_display_name: String,
    scope_display_name: String,
    scope_mode: String,
    source_fingerprint: String,
    database_source_fingerprint: String,
    resolved_object_count: i64,
    scope_snapshot_json: String,
    scope_snapshot_hash_blake3: String,
    target_options_json: String,
    execution_policy_json: String,
    state: String,
}

impl BackupClaimContext {
    pub fn backup_run_id(&self) -> &BackupRunId {
        &self.backup_run_id
    }

    pub fn plan_occurrence_id(&self) -> &PlanOccurrenceId {
        &self.plan_occurrence_id
    }

    pub fn plan_revision_id(&self) -> &PlanRevisionId {
        &self.plan_revision_id
    }

    pub fn database_target_id(&self) -> &DatabaseTargetId {
        &self.database_target_id
    }

    pub fn backup_scope_id(&self) -> &BackupScopeId {
        &self.backup_scope_id
    }

    pub fn connection_profile_id(&self) -> &ConnectionProfileId {
        &self.connection_profile_id
    }

    pub fn engine(&self) -> &str {
        &self.engine
    }

    pub fn engine_object_id(&self) -> &str {
        &self.engine_object_id
    }

    pub fn target_display_name(&self) -> &str {
        &self.target_display_name
    }

    pub fn scope_display_name(&self) -> &str {
        &self.scope_display_name
    }

    pub fn scope_mode(&self) -> &str {
        &self.scope_mode
    }

    pub fn source_fingerprint(&self) -> &str {
        &self.source_fingerprint
    }

    pub fn database_source_fingerprint(&self) -> &str {
        &self.database_source_fingerprint
    }

    pub const fn resolved_object_count(&self) -> i64 {
        self.resolved_object_count
    }

    pub fn scope_snapshot_json(&self) -> &str {
        &self.scope_snapshot_json
    }

    pub fn scope_snapshot_hash_blake3(&self) -> &str {
        &self.scope_snapshot_hash_blake3
    }

    pub fn target_options_json(&self) -> &str {
        &self.target_options_json
    }

    pub fn execution_policy_json(&self) -> &str {
        &self.execution_policy_json
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerificationClaimContext {
    verification_run_id: VerificationRunId,
    recovery_point_id: RecoveryPointId,
    source_replica_id: Option<String>,
    requested_level: i64,
    restore_run_id: Option<String>,
    source_engine: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreClaimContext {
    restore_run_id: RestoreRunId,
    recovery_point_id: RecoveryPointId,
    source_replica_id: Option<String>,
    target_connection_profile_id: ConnectionProfileId,
    target_database_name: String,
    target_fingerprint: String,
    target_server_identity: Option<String>,
    purpose: String,
    conflict_policy: String,
    eligibility_snapshot_json: String,
    authoritatively_eligible: bool,
}

impl RestoreClaimContext {
    pub fn restore_run_id(&self) -> &RestoreRunId {
        &self.restore_run_id
    }

    pub fn recovery_point_id(&self) -> &RecoveryPointId {
        &self.recovery_point_id
    }

    pub fn source_replica_id(&self) -> Option<&str> {
        self.source_replica_id.as_deref()
    }

    pub fn target_connection_profile_id(&self) -> &ConnectionProfileId {
        &self.target_connection_profile_id
    }

    pub fn target_database_name(&self) -> &str {
        &self.target_database_name
    }

    pub fn target_fingerprint(&self) -> &str {
        &self.target_fingerprint
    }

    pub fn target_server_identity(&self) -> Option<&str> {
        self.target_server_identity.as_deref()
    }

    pub fn purpose(&self) -> &str {
        &self.purpose
    }

    pub fn conflict_policy(&self) -> &str {
        &self.conflict_policy
    }

    pub fn eligibility_snapshot_json(&self) -> &str {
        &self.eligibility_snapshot_json
    }

    pub const fn authoritatively_eligible(&self) -> bool {
        self.authoritatively_eligible
    }
}

impl VerificationClaimContext {
    pub fn verification_run_id(&self) -> &VerificationRunId {
        &self.verification_run_id
    }

    pub fn recovery_point_id(&self) -> &RecoveryPointId {
        &self.recovery_point_id
    }

    pub fn source_replica_id(&self) -> Option<&str> {
        self.source_replica_id.as_deref()
    }

    pub const fn requested_level(&self) -> i64 {
        self.requested_level
    }

    pub fn restore_run_id(&self) -> Option<&str> {
        self.restore_run_id.as_deref()
    }

    pub fn source_engine(&self) -> &str {
        &self.source_engine
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimTerminalOutcome {
    Succeeded,
    PartiallySucceeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BeginCommitDisposition {
    Committing,
    CancelRequested,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelRequestOutcome {
    Requested,
    AlreadyRequested,
    AlreadyTerminal,
    CommitInProgress,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DestructiveStepDisposition {
    Proceed,
    CancelRequested,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryDisposition {
    Requeued,
    Exhausted,
    Cancelled,
}

#[derive(Clone, Debug)]
pub struct RetentionDeleteSettlementSpec {
    recovery_point_id: RecoveryPointId,
    destination_fingerprint: String,
    report: RetentionDeleteReportV2,
    completed_at_ms: i64,
    retry_available_at_ms: i64,
}

#[derive(Clone, Debug)]
pub struct ClaimedRetentionDeleteV2 {
    recovery_point_id: RecoveryPointId,
    destination_fingerprint: String,
    delete_secret_ref_id: Option<String>,
    confirmed: ConfirmedRetentionDeleteV2,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimedReplicaRepairV2 {
    recovery_point_id: RecoveryPointId,
    source_destination_fingerprint: String,
    target_destination_fingerprint: String,
    source_reader_secret_ref_id: Option<SecretRefId>,
    target_writer_secret_ref_id: Option<SecretRefId>,
    target_reader_secret_ref_id: Option<SecretRefId>,
    source_manifest_object_key: String,
    source_manifest_checksum_blake3: String,
    signing_key_id: String,
}

impl ClaimedReplicaRepairV2 {
    pub fn recovery_point_id(&self) -> &RecoveryPointId {
        &self.recovery_point_id
    }

    pub fn source_destination_fingerprint(&self) -> &str {
        &self.source_destination_fingerprint
    }

    pub fn target_destination_fingerprint(&self) -> &str {
        &self.target_destination_fingerprint
    }

    pub fn source_reader_secret_ref_id(&self) -> Option<&SecretRefId> {
        self.source_reader_secret_ref_id.as_ref()
    }

    pub fn target_writer_secret_ref_id(&self) -> Option<&SecretRefId> {
        self.target_writer_secret_ref_id.as_ref()
    }

    pub fn target_reader_secret_ref_id(&self) -> Option<&SecretRefId> {
        self.target_reader_secret_ref_id.as_ref()
    }

    pub fn source_manifest_object_key(&self) -> &str {
        &self.source_manifest_object_key
    }

    pub fn source_manifest_checksum_blake3(&self) -> &str {
        &self.source_manifest_checksum_blake3
    }

    pub fn signing_key_id(&self) -> &str {
        &self.signing_key_id
    }
}

impl ClaimedRetentionDeleteV2 {
    pub fn recovery_point_id(&self) -> &RecoveryPointId {
        &self.recovery_point_id
    }

    pub fn destination_fingerprint(&self) -> &str {
        &self.destination_fingerprint
    }

    pub fn delete_secret_ref_id(&self) -> Option<&str> {
        self.delete_secret_ref_id.as_deref()
    }

    pub fn confirmed(&self) -> &ConfirmedRetentionDeleteV2 {
        &self.confirmed
    }
}

impl RetentionDeleteSettlementSpec {
    pub fn new(
        recovery_point_id: RecoveryPointId,
        destination_fingerprint: impl Into<String>,
        report: RetentionDeleteReportV2,
        completed_at_ms: i64,
        retry_available_at_ms: i64,
    ) -> Self {
        Self {
            recovery_point_id,
            destination_fingerprint: destination_fingerprint.into(),
            report,
            completed_at_ms,
            retry_available_at_ms,
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if self.destination_fingerprint.trim().is_empty()
            || self.destination_fingerprint.len() > 512
            || self.completed_at_ms < 0
            || self.retry_available_at_ms < self.completed_at_ms
            || self.report.observations.is_empty()
            || self.report.observations.len() > 10_000
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let mut identities = std::collections::BTreeSet::new();
        for observation in &self.report.observations {
            if !is_retention_object_key(&observation.object_key)
                || observation
                    .object_version
                    .as_deref()
                    .is_some_and(|version| {
                        version.is_empty()
                            || version.len() > 1_024
                            || version
                                .chars()
                                .any(|value| matches!(value, '\0' | '\r' | '\n'))
                    })
                || !identities.insert((
                    observation.object_key.as_str(),
                    observation.object_version.as_deref(),
                ))
                || observation
                    .diagnostic
                    .as_deref()
                    .is_some_and(|value| value.is_empty() || value.len() > 4 * 1024)
            {
                return Err(RepositoryError::RuntimeStateInvalid);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetentionDeleteSettlementOutcome {
    Terminal(ClaimTerminalOutcome),
    Retry(RetryDisposition),
}

#[derive(Clone, Debug)]
pub struct ClaimCompletionSpec {
    claim: ClaimedWork,
    outcome: ClaimTerminalOutcome,
    completed_at_ms: i64,
    result_json: Option<String>,
    error_code: Option<String>,
}

impl ClaimCompletionSpec {
    pub fn succeeded(
        claim: ClaimedWork,
        completed_at_ms: i64,
        result_json: Option<String>,
    ) -> Self {
        Self {
            claim,
            outcome: ClaimTerminalOutcome::Succeeded,
            completed_at_ms,
            result_json,
            error_code: None,
        }
    }

    pub fn partially_succeeded(
        claim: ClaimedWork,
        completed_at_ms: i64,
        result_json: Option<String>,
        error_code: Option<String>,
    ) -> Self {
        Self {
            claim,
            outcome: ClaimTerminalOutcome::PartiallySucceeded,
            completed_at_ms,
            result_json,
            error_code,
        }
    }

    pub fn failed(
        claim: ClaimedWork,
        completed_at_ms: i64,
        error_code: impl Into<String>,
        result_json: Option<String>,
    ) -> Self {
        Self {
            claim,
            outcome: ClaimTerminalOutcome::Failed,
            completed_at_ms,
            result_json,
            error_code: Some(error_code.into()),
        }
    }

    pub fn cancelled(
        claim: ClaimedWork,
        completed_at_ms: i64,
        error_code: Option<String>,
        result_json: Option<String>,
    ) -> Self {
        Self {
            claim,
            outcome: ClaimTerminalOutcome::Cancelled,
            completed_at_ms,
            result_json,
            error_code,
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        let result_valid = match self.result_json.as_deref() {
            Some(value) => {
                value.len() <= 1024 * 1024
                    && serde_json::from_str::<serde_json::Value>(value).is_ok()
            }
            None => true,
        };
        let error_valid = match self.error_code.as_deref() {
            Some(value) => is_runtime_error_code(value),
            None => true,
        };
        let outcome_valid = match self.outcome {
            ClaimTerminalOutcome::Succeeded => self.error_code.is_none(),
            ClaimTerminalOutcome::PartiallySucceeded | ClaimTerminalOutcome::Cancelled => true,
            ClaimTerminalOutcome::Failed => self.error_code.is_some(),
        };
        if self.completed_at_ms < 0 || !result_valid || !error_valid || !outcome_valid {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct AdmissionRequest {
    claim: ClaimedWork,
    resources: Vec<ResourceRequirement>,
    spool: Option<SpoolRequirement>,
    now_ms: i64,
}

impl AdmissionRequest {
    pub fn new(
        claim: ClaimedWork,
        resources: Vec<ResourceRequirement>,
        spool: Option<SpoolRequirement>,
        now_ms: i64,
    ) -> Self {
        Self {
            claim,
            resources,
            spool,
            now_ms,
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if self.now_ms < 0 {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let mut pool_ids = std::collections::BTreeSet::new();
        for requirement in &self.resources {
            if !is_canonical_uuid(&requirement.resource_pool_id)
                || requirement.units <= 0
                || !pool_ids.insert(requirement.resource_pool_id.as_str())
            {
                return Err(RepositoryError::RuntimeStateInvalid);
            }
        }
        if let Some(spool) = &self.spool {
            if !is_canonical_uuid(&spool.spool_volume_id)
                || spool.reserved_bytes <= 0
                || spool.spool_path.is_empty()
                || spool.spool_path.len() > 4096
            {
                return Err(RepositoryError::RuntimeStateInvalid);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdmissionOutcome {
    admitted: bool,
    wait_reason: Option<String>,
    allocation_count: usize,
    spool_reserved: bool,
}

impl AdmissionOutcome {
    pub const fn admitted(&self) -> bool {
        self.admitted
    }

    pub fn wait_reason(&self) -> Option<&str> {
        self.wait_reason.as_deref()
    }

    pub const fn allocation_count(&self) -> usize {
        self.allocation_count
    }

    pub const fn spool_reserved(&self) -> bool {
        self.spool_reserved
    }
}

#[derive(Clone, Debug)]
pub struct ProcessStartSpec {
    process_instance_id: String,
    claim: ClaimedWork,
    phase: String,
    process_id: i64,
    process_start_time_ms: i64,
    service_boot_id: String,
    job_object_key: String,
    executable_path: String,
    executable_sha256: Option<String>,
    started_at_ms: i64,
}

impl ProcessStartSpec {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        process_instance_id: impl Into<String>,
        claim: ClaimedWork,
        phase: impl Into<String>,
        process_id: i64,
        process_start_time_ms: i64,
        service_boot_id: impl Into<String>,
        job_object_key: impl Into<String>,
        executable_path: impl Into<String>,
        executable_sha256: Option<String>,
        started_at_ms: i64,
    ) -> Self {
        Self {
            process_instance_id: process_instance_id.into(),
            claim,
            phase: phase.into(),
            process_id,
            process_start_time_ms,
            service_boot_id: service_boot_id.into(),
            job_object_key: job_object_key.into(),
            executable_path: executable_path.into(),
            executable_sha256,
            started_at_ms,
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if !is_canonical_uuid(&self.process_instance_id)
            || self.phase.is_empty()
            || self.phase.len() > 256
            || self.process_id <= 0
            || self.process_start_time_ms < 0
            || self.process_start_time_ms > self.started_at_ms
            || self.service_boot_id != self.claim.worker_boot_id
            || self.job_object_key.is_empty()
            || self.job_object_key.len() > 1024
            || self.executable_path.len() > 4096
            || !std::path::Path::new(&self.executable_path).is_absolute()
            || self
                .executable_sha256
                .as_deref()
                .is_some_and(|digest| !is_hex_digest(digest))
            || self.started_at_ms < 0
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ProcessExitSpec {
    process_instance_id: String,
    ended_at_ms: i64,
    exit_code: i64,
    termination_reason: String,
}

impl ProcessExitSpec {
    pub fn new(
        process_instance_id: impl Into<String>,
        ended_at_ms: i64,
        exit_code: i64,
        termination_reason: impl Into<String>,
    ) -> Self {
        Self {
            process_instance_id: process_instance_id.into(),
            ended_at_ms,
            exit_code,
            termination_reason: termination_reason.into(),
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if !is_canonical_uuid(&self.process_instance_id)
            || self.ended_at_ms < 0
            || self.termination_reason.is_empty()
            || self.termination_reason.len() > 1024
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedProcessIdentity {
    process_instance_id: String,
    process_id: i64,
    process_start_time_ms: i64,
    service_boot_id: String,
    job_object_key: String,
    executable_path: String,
    executable_sha256: Option<String>,
    state: String,
}

impl RecordedProcessIdentity {
    pub fn process_instance_id(&self) -> &str {
        &self.process_instance_id
    }

    pub const fn process_id(&self) -> i64 {
        self.process_id
    }

    pub const fn process_start_time_ms(&self) -> i64 {
        self.process_start_time_ms
    }

    pub fn service_boot_id(&self) -> &str {
        &self.service_boot_id
    }

    pub fn job_object_key(&self) -> &str {
        &self.job_object_key
    }

    pub fn executable_path(&self) -> &str {
        &self.executable_path
    }

    pub fn executable_sha256(&self) -> Option<&str> {
        self.executable_sha256.as_deref()
    }

    pub fn state(&self) -> &str {
        &self.state
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpiredClaim {
    claim: ClaimedWork,
    work_state: WorkItemState,
    attempt_state: WorkAttemptState,
    lease_expires_at_ms: i64,
    process: Option<RecordedProcessIdentity>,
    restore_destructive_step_started: bool,
}

impl ExpiredClaim {
    pub fn claim(&self) -> &ClaimedWork {
        &self.claim
    }

    pub fn work_state(&self) -> &str {
        self.work_state.as_str()
    }

    pub fn attempt_state(&self) -> &str {
        self.attempt_state.as_str()
    }

    pub const fn lease_expires_at_ms(&self) -> i64 {
        self.lease_expires_at_ms
    }

    pub const fn process(&self) -> Option<&RecordedProcessIdentity> {
        self.process.as_ref()
    }

    pub const fn restore_destructive_step_started(&self) -> bool {
        self.restore_destructive_step_started
    }
}

struct ResourcePoolRow {
    id: String,
    resource_type: String,
    resource_key: String,
    capacity: i64,
    enabled: bool,
    units: i64,
}

struct SpoolVolumeRow {
    id: String,
    volume_key: String,
    hard_limit_bytes: i64,
    reserve_floor_bytes: i64,
    state: String,
    last_free_bytes: Option<i64>,
}

#[derive(Clone)]
pub struct RuntimeRepository {
    store: OperationalStore,
}

impl OperationalStore {
    pub fn runtime(&self) -> RuntimeRepository {
        RuntimeRepository {
            store: self.clone(),
        }
    }
}

impl RuntimeRepository {
    pub fn upsert_resource_pool(&self, spec: ResourcePoolSpec) -> Result<String, RepositoryError> {
        spec.validate()?;
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let existing: Option<(String, i64)> = transaction
                .query_row(
                    "SELECT id, created_at_ms
                     FROM resource_pools
                     WHERE resource_type = ?1 AND resource_key = ?2",
                    params![spec.resource_type, spec.resource_key],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((id, created_at_ms)) = existing {
                if spec.created_at_ms < created_at_ms {
                    return Err(RepositoryError::RuntimeStateInvalid);
                }
                transaction.execute(
                    "UPDATE resource_pools
                     SET capacity = ?2, updated_at_ms = ?3
                     WHERE id = ?1",
                    params![id, spec.capacity, spec.created_at_ms],
                )?;
                transaction.commit()?;
                return Ok(id);
            }
            transaction.execute(
                "INSERT INTO resource_pools(
                     id, resource_type, resource_key, capacity, enabled,
                     created_at_ms, updated_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, 1, ?5, ?5)",
                params![
                    spec.id,
                    spec.resource_type,
                    spec.resource_key,
                    spec.capacity,
                    spec.created_at_ms
                ],
            )?;
            transaction.commit()?;
            Ok(spec.id)
        })
    }

    pub fn upsert_spool_volume(&self, spec: SpoolVolumeSpec) -> Result<String, RepositoryError> {
        spec.validate()?;
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let existing: Option<(String, String, i64)> = transaction
                .query_row(
                    "SELECT id, root_path, created_at_ms
                     FROM spool_volumes
                     WHERE volume_key = ?1",
                    [spec.volume_key.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            if let Some((id, root_path, created_at_ms)) = existing {
                if root_path != spec.root_path || spec.observed_at_ms < created_at_ms {
                    return Err(RepositoryError::RuntimeStateInvalid);
                }
                transaction.execute(
                    "UPDATE spool_volumes
                     SET hard_limit_bytes = ?2, soft_limit_bytes = ?3,
                         reserve_floor_bytes = ?4,
                         state = CASE WHEN ?5 <= ?4 THEN 'full' ELSE 'ready' END,
                         last_free_bytes = ?5, last_probed_at_ms = ?6,
                         updated_at_ms = ?6
                     WHERE id = ?1",
                    params![
                        id,
                        spec.hard_limit_bytes,
                        spec.soft_limit_bytes,
                        spec.reserve_floor_bytes,
                        spec.last_free_bytes,
                        spec.observed_at_ms
                    ],
                )?;
                transaction.commit()?;
                return Ok(id);
            }
            transaction.execute(
                "INSERT INTO spool_volumes(
                     id, volume_key, root_path, hard_limit_bytes, soft_limit_bytes,
                     reserve_floor_bytes, state, last_free_bytes, last_probed_at_ms,
                     created_at_ms, updated_at_ms
                 ) VALUES (
                     ?1, ?2, ?3, ?4, ?5,
                     ?6, CASE WHEN ?7 <= ?6 THEN 'full' ELSE 'ready' END, ?7, ?8,
                     ?8, ?8
                 )",
                params![
                    spec.id,
                    spec.volume_key,
                    spec.root_path,
                    spec.hard_limit_bytes,
                    spec.soft_limit_bytes,
                    spec.reserve_floor_bytes,
                    spec.last_free_bytes,
                    spec.observed_at_ms
                ],
            )?;
            transaction.commit()?;
            Ok(spec.id)
        })
    }

    pub fn register_resource_pool(&self, spec: ResourcePoolSpec) -> Result<(), RepositoryError> {
        spec.validate()?;
        self.write(move |connection| {
            let transaction = connection.transaction()?;
            transaction.execute(
                "INSERT INTO resource_pools(
                     id, resource_type, resource_key, capacity, enabled,
                     created_at_ms, updated_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, 1, ?5, ?5)",
                params![
                    spec.id,
                    spec.resource_type,
                    spec.resource_key,
                    spec.capacity,
                    spec.created_at_ms
                ],
            )?;
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn register_spool_volume(&self, spec: SpoolVolumeSpec) -> Result<(), RepositoryError> {
        spec.validate()?;
        self.write(move |connection| {
            let transaction = connection.transaction()?;
            transaction.execute(
                "INSERT INTO spool_volumes(
                     id, volume_key, root_path, hard_limit_bytes, soft_limit_bytes,
                     reserve_floor_bytes, state, last_free_bytes, last_probed_at_ms,
                     created_at_ms, updated_at_ms
                 ) VALUES (
                     ?1, ?2, ?3, ?4, ?5,
                     ?6, 'ready', ?7, ?8,
                     ?8, ?8
                 )",
                params![
                    spec.id,
                    spec.volume_key,
                    spec.root_path,
                    spec.hard_limit_bytes,
                    spec.soft_limit_bytes,
                    spec.reserve_floor_bytes,
                    spec.last_free_bytes,
                    spec.observed_at_ms
                ],
            )?;
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn upsert_occurrence(
        &self,
        spec: OccurrenceSpec,
    ) -> Result<OccurrenceUpsert, RepositoryError> {
        spec.validate()?;
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            ensure_revision_published(&transaction, spec.plan_revision_id.as_str())?;
            let inserted = transaction.execute(
                "INSERT INTO plan_occurrences(
                     id, plan_revision_id, origin, scheduled_for_ms, requested_at_ms,
                     request_idempotency_key, state, created_at_ms, updated_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?5, ?5)
                 ON CONFLICT DO NOTHING",
                params![
                    spec.id.as_str(),
                    spec.plan_revision_id.as_str(),
                    spec.origin.as_str(),
                    spec.scheduled_for_ms,
                    spec.requested_at_ms,
                    spec.request_idempotency_key
                ],
            )?;
            if inserted == 1 {
                transaction.commit()?;
                return Ok(OccurrenceUpsert {
                    id: spec.id,
                    created: true,
                });
            }

            let existing = select_occurrence_identity(&transaction, &spec)?
                .ok_or(RepositoryError::OccurrenceIdentityConflict)?;
            if existing.0 != spec.id.as_str() || existing.1 != spec.plan_revision_id.as_str() {
                return Err(RepositoryError::OccurrenceIdentityConflict);
            }
            let id = PlanOccurrenceId::from_str(&existing.0)?;
            transaction.commit()?;
            Ok(OccurrenceUpsert { id, created: false })
        })
    }

    pub fn materialize_occurrence(
        &self,
        spec: OccurrenceMaterializationSpec,
    ) -> Result<OccurrenceMaterialization, RepositoryError> {
        spec.validate()?;
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            ensure_revision_published(&transaction, spec.plan_revision_id.as_str())?;
            let identity = OccurrenceIdentity::scheduled(
                spec.plan_revision_id.clone(),
                spec.origin,
                spec.scheduled_for_ms,
            )?;
            let occurrence_id = identity.deterministic_id(Uuid::NAMESPACE_URL);
            let initial_state = if spec.missed_reason.is_some() {
                "missed"
            } else {
                "pending"
            };
            let occurrence_inserted = transaction.execute(
                "INSERT INTO plan_occurrences(
                     id, plan_revision_id, origin, scheduled_for_ms, requested_at_ms,
                     request_idempotency_key, state, missed_reason, created_at_ms, updated_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, ?7, ?5, ?5)
                 ON CONFLICT DO NOTHING",
                params![
                    occurrence_id.as_str(),
                    spec.plan_revision_id.as_str(),
                    spec.origin.as_str(),
                    spec.scheduled_for_ms,
                    spec.requested_at_ms,
                    initial_state,
                    spec.missed_reason.as_deref()
                ],
            )?;
            if occurrence_inserted == 0 {
                let existing: Option<(String, String, String, Option<String>)> = transaction
                    .query_row(
                        "SELECT id, plan_revision_id, state, missed_reason
                         FROM plan_occurrences
                         WHERE plan_revision_id = ?1
                           AND scheduled_for_ms = ?2
                           AND origin IN ('scheduled', 'catch_up')",
                        params![spec.plan_revision_id.as_str(), spec.scheduled_for_ms],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )
                    .optional()?;
                let Some((id, revision_id, state, missed_reason)) = existing else {
                    return Err(RepositoryError::OccurrenceIdentityConflict);
                };
                if id != occurrence_id.as_str()
                    || revision_id != spec.plan_revision_id.as_str()
                    || (spec.missed_reason.is_some()
                        && (state != "missed" || missed_reason != spec.missed_reason))
                    || (spec.missed_reason.is_none() && state == "missed")
                {
                    return Err(RepositoryError::OccurrenceIdentityConflict);
                }
            }

            if spec.missed_reason.is_some() {
                transaction.commit()?;
                return Ok(OccurrenceMaterialization {
                    occurrence_id,
                    occurrence_created: occurrence_inserted == 1,
                    backup_runs_created: 0,
                    work_items_created: 0,
                });
            }

            let scopes = {
                let mut statement = transaction.prepare(
                    "SELECT
                         database_targets.id,
                         revision_scope.backup_scope_id,
                         connection_profiles.engine,
                         revision_scope.source_fingerprint
                     FROM plan_revision_targets
                     JOIN plan_revision_scopes AS revision_scope
                       ON revision_scope.plan_revision_id = plan_revision_targets.plan_revision_id
                      AND revision_scope.database_target_id = plan_revision_targets.database_target_id
                     JOIN database_targets
                       ON database_targets.id =
                          plan_revision_targets.database_target_id
                     JOIN connection_profiles
                       ON connection_profiles.id =
                          database_targets.connection_profile_id
                     WHERE plan_revision_targets.plan_revision_id = ?1
                     ORDER BY database_targets.id",
                )?;
                let rows = statement
                    .query_map([spec.plan_revision_id.as_str()], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                rows
            };
            if scopes.is_empty() || scopes.len() > 1_000 {
                return Err(RepositoryError::RuntimeStateInvalid);
            }
            let mut backup_runs_created = 0usize;
            let mut work_items_created = 0usize;
            for (target_id, scope_id, engine, source_fingerprint) in scopes {
                let backup_run_id = BackupRunId::from_stable_name(
                    Uuid::NAMESPACE_URL,
                    format!("backup-run:{occurrence_id}:{scope_id}").as_bytes(),
                );
                let work_item_id = WorkItemId::from_stable_name(
                    Uuid::NAMESPACE_URL,
                    format!("backup-work:{occurrence_id}:{scope_id}").as_bytes(),
                );
                let run_inserted = transaction.execute(
                    "INSERT INTO backup_runs(
                         id, plan_occurrence_id, plan_revision_id, database_target_id,
                         state, engine, source_fingerprint, created_at_ms, updated_at_ms
                     ) VALUES (?1, ?2, ?3, ?4, 'queued', ?5, ?6, ?7, ?7)
                     ON CONFLICT DO NOTHING",
                    params![
                        backup_run_id.as_str(),
                        occurrence_id.as_str(),
                        spec.plan_revision_id.as_str(),
                        target_id,
                        engine,
                        source_fingerprint,
                        spec.requested_at_ms
                    ],
                )?;
                if run_inserted == 0 {
                    let existing: Option<(String, String, String, String, String)> = transaction
                        .query_row(
                            "SELECT
                                 id, plan_revision_id, database_target_id,
                                 engine, source_fingerprint
                             FROM backup_runs
                             WHERE plan_occurrence_id = ?1
                               AND database_target_id = ?2",
                            params![occurrence_id.as_str(), target_id],
                            |row| {
                                Ok((
                                    row.get(0)?,
                                    row.get(1)?,
                                    row.get(2)?,
                                    row.get(3)?,
                                    row.get(4)?,
                                ))
                            },
                        )
                        .optional()?;
                    if existing
                        != Some((
                            backup_run_id.to_string(),
                            spec.plan_revision_id.to_string(),
                            target_id.clone(),
                            engine.clone(),
                            source_fingerprint.clone(),
                        ))
                    {
                        return Err(RepositoryError::RuntimeStateInvalid);
                    }
                } else {
                    backup_runs_created += 1;
                }

                let work_inserted = transaction.execute(
                    "INSERT INTO work_items(
                         id, kind, backup_run_id, priority, state,
                         available_at_ms, attempt_limit, created_at_ms, updated_at_ms
                     ) VALUES (?1, 'backup', ?2, 100, 'queued', ?3, 3, ?3, ?3)
                     ON CONFLICT DO NOTHING",
                    params![
                        work_item_id.as_str(),
                        backup_run_id.as_str(),
                        spec.requested_at_ms
                    ],
                )?;
                if work_inserted == 0 {
                    let existing: Option<(String, String, String)> = transaction
                        .query_row(
                            "SELECT id, kind, backup_run_id
                             FROM work_items
                             WHERE backup_run_id = ?1",
                            [backup_run_id.as_str()],
                            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                        )
                        .optional()?;
                    if existing
                        != Some((
                            work_item_id.to_string(),
                            "backup".to_owned(),
                            backup_run_id.to_string(),
                        ))
                    {
                        return Err(RepositoryError::RuntimeStateInvalid);
                    }
                } else {
                    work_items_created += 1;
                }
            }
            transaction.execute(
                "UPDATE plan_occurrences
                 SET state = 'enqueued', updated_at_ms = MAX(updated_at_ms, ?2)
                 WHERE id = ?1 AND state = 'pending'",
                params![occurrence_id.as_str(), spec.requested_at_ms],
            )?;
            transaction.commit()?;
            Ok(OccurrenceMaterialization {
                occurrence_id,
                occurrence_created: occurrence_inserted == 1,
                backup_runs_created,
                work_items_created,
            })
        })
    }

    pub fn enqueue_backup_work(&self, spec: BackupWorkSpec) -> Result<(), RepositoryError> {
        if spec.available_at_ms < 0
            || !(0..=1000).contains(&spec.priority)
            || spec.attempt_limit <= 0
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let (revision_id, occurrence_state): (String, String) = transaction
                .query_row(
                    "SELECT plan_revision_id, state
                     FROM plan_occurrences
                     WHERE id = ?1",
                    [spec.plan_occurrence_id.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?
                .ok_or(RepositoryError::NotFound)?;
            if revision_id != spec.plan_revision_id.as_str() {
                return Err(RepositoryError::RuntimeStateInvalid);
            }
            let occurrence_state = PlanOccurrenceState::from_str(&occurrence_state)?;
            match occurrence_state {
                PlanOccurrenceState::Pending => {
                    let next =
                        transition_plan_occurrence(occurrence_state, OccurrenceCommand::Enqueue)?;
                    transaction.execute(
                        "UPDATE plan_occurrences
                         SET state = ?2, updated_at_ms = ?3
                         WHERE id = ?1 AND state = 'pending'",
                        params![
                            spec.plan_occurrence_id.as_str(),
                            next.as_str(),
                            spec.available_at_ms
                        ],
                    )?;
                }
                PlanOccurrenceState::Enqueued => {}
                _ => return Err(RepositoryError::RuntimeStateInvalid),
            }

            transaction.execute(
                "INSERT INTO backup_runs(
                     id, plan_occurrence_id, plan_revision_id, database_target_id,
                     state, engine, source_fingerprint, created_at_ms, updated_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, 'queued', ?5, ?6, ?7, ?7)",
                params![
                    spec.backup_run_id.as_str(),
                    spec.plan_occurrence_id.as_str(),
                    spec.plan_revision_id.as_str(),
                    spec.database_target_id.as_str(),
                    spec.engine,
                    spec.source_fingerprint,
                    spec.available_at_ms
                ],
            )?;
            transaction.execute(
                "INSERT INTO work_items(
                     id, kind, backup_run_id, priority, state, available_at_ms,
                     attempt_limit, created_at_ms, updated_at_ms
                 ) VALUES (?1, 'backup', ?2, ?3, 'queued', ?4, ?5, ?4, ?4)",
                params![
                    spec.work_item_id.as_str(),
                    spec.backup_run_id.as_str(),
                    spec.priority,
                    spec.available_at_ms,
                    spec.attempt_limit
                ],
            )?;
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn enqueue_operation_work(
        &self,
        spec: OperationWorkSpec,
    ) -> Result<OperationWorkUpsert, RepositoryError> {
        spec.validate()?;
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let result = insert_operation_work_in_transaction(&transaction, &spec)?;
            transaction.commit()?;
            Ok(result)
        })
    }

    pub fn enqueue_catalog_rebuild(
        &self,
        spec: CatalogRebuildWorkSpec,
    ) -> Result<CatalogRebuildWorkUpsert, RepositoryError> {
        spec.validate()?;
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let existing: Option<ExistingCatalogRebuildWork> = transaction
                .query_row(
                    "SELECT
                         rebuild.destination_id, rebuild.source_kind, rebuild.mode,
                         work.id, work.priority, work.available_at_ms, work.attempt_limit
                     FROM catalog_rebuild_runs AS rebuild
                     LEFT JOIN work_items AS work
                       ON work.catalog_rebuild_run_id = rebuild.id
                     WHERE rebuild.id = ?1",
                    [spec.catalog_rebuild_run_id.as_str()],
                    |row| {
                        Ok(ExistingCatalogRebuildWork {
                            destination_id: row.get(0)?,
                            source_kind: row.get(1)?,
                            mode: row.get(2)?,
                            work_item_id: row.get(3)?,
                            priority: row.get(4)?,
                            available_at_ms: row.get(5)?,
                            attempt_limit: row.get(6)?,
                        })
                    },
                )
                .optional()?;
            if let Some(existing) = existing {
                if existing.destination_id.as_deref()
                    != spec.destination_id.as_ref().map(DestinationId::as_str)
                    || existing.source_kind != spec.source_kind.as_str()
                    || existing.mode != spec.mode.as_str()
                    || existing.work_item_id.as_deref() != Some(spec.work_item_id.as_str())
                    || existing.priority != Some(spec.priority)
                    || existing.available_at_ms != Some(spec.available_at_ms)
                    || existing.attempt_limit != Some(spec.attempt_limit)
                {
                    return Err(RepositoryError::RuntimeStateInvalid);
                }
                transaction.commit()?;
                return Ok(CatalogRebuildWorkUpsert {
                    catalog_rebuild_run_id: spec.catalog_rebuild_run_id,
                    work_item_id: spec.work_item_id,
                    created: false,
                });
            }

            let work_id_exists: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM work_items WHERE id = ?1)",
                [spec.work_item_id.as_str()],
                |row| row.get(0),
            )?;
            if work_id_exists {
                return Err(RepositoryError::RuntimeStateInvalid);
            }
            transaction.execute(
                "INSERT INTO catalog_rebuild_runs(
                     id, destination_id, source_kind, mode, status, created_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, 'queued', ?5)",
                params![
                    spec.catalog_rebuild_run_id.as_str(),
                    spec.destination_id.as_ref().map(DestinationId::as_str),
                    spec.source_kind.as_str(),
                    spec.mode.as_str(),
                    spec.available_at_ms
                ],
            )?;
            transaction.execute(
                "INSERT INTO work_items(
                     id, kind, catalog_rebuild_run_id, priority, state,
                     available_at_ms, attempt_limit, created_at_ms, updated_at_ms
                 ) VALUES (?1, 'catalog_rebuild', ?2, ?3, 'queued', ?4, ?5, ?4, ?4)",
                params![
                    spec.work_item_id.as_str(),
                    spec.catalog_rebuild_run_id.as_str(),
                    spec.priority,
                    spec.available_at_ms,
                    spec.attempt_limit
                ],
            )?;
            transaction.commit()?;
            Ok(CatalogRebuildWorkUpsert {
                catalog_rebuild_run_id: spec.catalog_rebuild_run_id,
                work_item_id: spec.work_item_id,
                created: true,
            })
        })
    }

    pub fn enqueue_replica_repair(
        &self,
        spec: ReplicaRepairWorkSpec,
    ) -> Result<OperationWorkUpsert, RepositoryError> {
        self.enqueue_operation_work(spec.operation_work()?)
    }

    pub fn claim_next(
        &self,
        request: ClaimRequest,
    ) -> Result<Option<ClaimedWork>, RepositoryError> {
        if request.worker_id.is_empty()
            || request.worker_boot_id.is_empty()
            || request.now_ms < 0
            || request.lease_duration_ms <= 0
            || request.operation_kind_filter.as_ref().is_some_and(|kinds| {
                request.kind_filter != Some(WorkItemKind::Operation)
                    || kinds.is_empty()
                    || kinds.len() > 32
                    || kinds.iter().any(|kind| !is_operation_kind(kind))
            })
        {
            return Err(RepositoryError::LeaseInvalid);
        }
        let operation_kind_filter_json = request
            .operation_kind_filter
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|_| RepositoryError::LeaseInvalid)?;
        let lease_expires_at_ms = request
            .now_ms
            .checked_add(request.lease_duration_ms)
            .ok_or(RepositoryError::LeaseInvalid)?;
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let candidate: Option<(String, String, String, i64, i64, String)> = transaction
                .query_row(
                    "SELECT work.id, work.kind, work.state,
                            work.current_attempt_no, work.fencing_token,
                            ownership.workspace_id
                     FROM work_items AS work
                     JOIN workspace_work_items AS ownership ON ownership.work_item_id = work.id
                     JOIN workspace_dispatch_state AS dispatch
                       ON dispatch.workspace_id = ownership.workspace_id
                     WHERE work.state IN ('queued', 'delayed')
                       AND work.available_at_ms <= ?1
                       AND (?2 IS NULL OR work.kind = ?2)
                       AND (
                           ?3 IS NULL
                           OR EXISTS(
                               SELECT 1
                               FROM operation_runs, json_each(?3) AS accepted
                               WHERE operation_runs.id = work.operation_run_id
                                 AND operation_runs.operation_kind = accepted.value
                           )
                       )
                       AND (
                           SELECT COUNT(*)
                           FROM work_attempts
                           WHERE work_attempts.work_item_id = work.id
                             AND work_attempts.state <> 'abandoned'
                       ) < work.attempt_limit
                     ORDER BY dispatch.last_claimed_at_ms,
                              dispatch.claim_count,
                              ownership.workspace_id,
                              work.priority DESC,
                              work.available_at_ms,
                              work.created_at_ms,
                              work.id
                     LIMIT 1",
                    params![
                        request.now_ms,
                        request.kind_filter.map(WorkItemKind::as_str),
                        operation_kind_filter_json
                    ],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                        ))
                    },
                )
                .optional()?;
            let Some((
                work_item_id,
                work_kind,
                observed_state,
                observed_attempt_no,
                observed_fence,
                workspace_id,
            )) = candidate
            else {
                transaction.commit()?;
                return Ok(None);
            };

            let current_state = WorkItemState::from_str(&observed_state)?;
            let next_state = transition_work_item(current_state, WorkItemCommand::Claim)?;
            let attempt_no = observed_attempt_no + 1;
            let fencing_token = observed_fence + 1;
            let affected = transaction.execute(
                "UPDATE work_items
                 SET state = ?2, current_attempt_no = ?3, fencing_token = ?4,
                     lease_owner = ?5, lease_boot_id = ?6, lease_expires_at_ms = ?7,
                     heartbeat_at_ms = ?8, updated_at_ms = ?8
                 WHERE id = ?1
                   AND state = ?9
                   AND current_attempt_no = ?10
                   AND fencing_token = ?11
                   AND available_at_ms <= ?8
                   AND (
                       SELECT COUNT(*)
                       FROM work_attempts
                       WHERE work_attempts.work_item_id = work_items.id
                         AND work_attempts.state <> 'abandoned'
                   ) < attempt_limit",
                params![
                    work_item_id,
                    next_state.as_str(),
                    attempt_no,
                    fencing_token,
                    request.worker_id,
                    request.worker_boot_id,
                    lease_expires_at_ms,
                    request.now_ms,
                    observed_state,
                    observed_attempt_no,
                    observed_fence
                ],
            )?;
            if affected != 1 {
                return Err(RepositoryError::StaleFence);
            }

            let kind = WorkItemKind::from_str(&work_kind)?;
            let attempt_id = WorkAttemptId::new();
            transaction.execute(
                "INSERT INTO work_attempts(
                     id, work_item_id, attempt_no, fencing_token, worker_id, worker_boot_id,
                     state, lease_acquired_at_ms, last_heartbeat_at_ms, lease_expires_at_ms,
                     created_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'claimed', ?7, ?7, ?8, ?7)",
                params![
                    attempt_id.as_str(),
                    work_item_id,
                    attempt_no,
                    fencing_token,
                    request.worker_id,
                    request.worker_boot_id,
                    request.now_ms,
                    lease_expires_at_ms
                ],
            )?;
            let dispatch_updated = transaction.execute(
                "UPDATE workspace_dispatch_state
                 SET last_claimed_at_ms = ?2, claim_count = claim_count + 1
                 WHERE workspace_id = ?1",
                params![workspace_id, request.now_ms],
            )?;
            if dispatch_updated != 1 {
                return Err(RepositoryError::RuntimeStateInvalid);
            }
            let work_item_id = WorkItemId::from_str(&work_item_id)?;
            transaction.commit()?;
            Ok(Some(ClaimedWork {
                work_item_id,
                kind,
                attempt_id,
                attempt_no,
                fencing_token,
                worker_id: request.worker_id,
                worker_boot_id: request.worker_boot_id,
            }))
        })
    }

    pub fn operation_claim_context(
        &self,
        claim: &ClaimedWork,
        observed_at_ms: i64,
    ) -> Result<OperationClaimContext, RepositoryError> {
        if claim.kind != WorkItemKind::Operation || observed_at_ms < 0 {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let claim = claim.clone();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let current = load_current_claim(&transaction, &claim, observed_at_ms)?;
            let operation_run_id = current
                .operation_run_id
                .ok_or(RepositoryError::RuntimeStateInvalid)?;
            let row = transaction
                .query_row(
                    "SELECT operation_kind, subject_type, subject_id, request_json
                     FROM operation_runs
                     WHERE id = ?1",
                    [operation_run_id.as_str()],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    },
                )
                .optional()?
                .ok_or(RepositoryError::NotFound)?;
            transaction.commit()?;
            Ok(OperationClaimContext {
                operation_run_id: OperationRunId::from_str(&operation_run_id)?,
                operation_kind: row.0,
                subject_type: row.1,
                subject_id: row.2,
                request_json: row.3,
            })
        })
    }

    /// Reads only the immutable identity of the current committing Backup
    /// attempt. Unlike `backup_claim_context`, this deliberately does not
    /// require a live lease: startup reconciliation calls it only after the
    /// durable expired-claim scan has selected the exact fenced attempt.
    pub fn backup_commit_recovery_context(
        &self,
        claim: &ClaimedWork,
    ) -> Result<BackupCommitRecoveryContext, RepositoryError> {
        if claim.kind != WorkItemKind::Backup {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        type Row = (
            String,
            String,
            String,
            String,
            Option<String>,
            String,
            String,
            String,
            String,
            String,
            String,
            i64,
            String,
            String,
        );
        let reader = self.store.open_reader()?;
        let row: Option<Row> = reader
            .connection()
            .query_row(
                "SELECT backup.id, backup.plan_occurrence_id, backup.plan_revision_id,
                        run_scope.backup_scope_id, backup.recovery_point_id, backup.engine,
                        backup.database_target_id, backup.source_fingerprint,
                        run_scope.database_target_id, run_scope.scope_mode,
                        run_scope.source_fingerprint, run_scope.resolved_object_count,
                        run_scope.scope_snapshot_json,
                        run_scope.scope_snapshot_hash_blake3
                 FROM work_items AS work
                 JOIN work_attempts AS attempt
                   ON attempt.id = ?2
                  AND attempt.work_item_id = work.id
                 JOIN backup_runs AS backup
                   ON backup.id = work.backup_run_id
                 JOIN backup_run_scopes AS run_scope
                   ON run_scope.backup_run_id = backup.id
                 WHERE work.id = ?1
                   AND work.kind = 'backup'
                   AND work.current_attempt_no = ?3
                   AND work.fencing_token = ?4
                   AND work.lease_owner = ?5
                   AND work.lease_boot_id = ?6
                   AND work.state = 'committing'
                   AND attempt.attempt_no = ?3
                   AND attempt.fencing_token = ?4
                   AND attempt.worker_id = ?5
                   AND attempt.worker_boot_id = ?6
                   AND attempt.state = 'committing'
                   AND backup.state = 'committing'",
                params![
                    claim.work_item_id.as_str(),
                    claim.attempt_id.as_str(),
                    claim.attempt_no,
                    claim.fencing_token,
                    claim.worker_id.as_str(),
                    claim.worker_boot_id.as_str(),
                ],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                        row.get(9)?,
                        row.get(10)?,
                        row.get(11)?,
                        row.get(12)?,
                        row.get(13)?,
                    ))
                },
            )
            .optional()?;
        let Some(row) = row else {
            return Err(RepositoryError::StaleFence);
        };
        if !matches!(
            row.5.as_str(),
            "postgresql" | "mysql" | "mariadb" | "sqlite"
        ) {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        Ok(BackupCommitRecoveryContext {
            backup_run_id: BackupRunId::from_str(&row.0)?,
            plan_occurrence_id: PlanOccurrenceId::from_str(&row.1)?,
            plan_revision_id: PlanRevisionId::from_str(&row.2)?,
            backup_scope_id: BackupScopeId::from_str(&row.3)?,
            recovery_point_id: row
                .4
                .map(|value| RecoveryPointId::from_str(&value))
                .transpose()?,
            engine: row.5,
            backup_database_target_id: row.6,
            backup_source_fingerprint: row.7,
            scope_database_target_id: row.8,
            scope_mode: row.9,
            scope_source_fingerprint: row.10,
            resolved_object_count: row.11,
            scope_snapshot_json: row.12,
            scope_snapshot_hash_blake3: row.13,
        })
    }

    pub fn backup_claim_context(
        &self,
        claim: &ClaimedWork,
        observed_at_ms: i64,
    ) -> Result<BackupClaimContext, RepositoryError> {
        if claim.kind != WorkItemKind::Backup || observed_at_ms < 0 {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let claim = claim.clone();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let current = load_current_claim(&transaction, &claim, observed_at_ms)?;
            if current.kind != WorkItemKind::Backup.as_str()
                || !matches!(
                    (current.work_state, current.attempt_state),
                    (WorkItemState::Leased, WorkAttemptState::Claimed)
                        | (WorkItemState::Starting, WorkAttemptState::Starting)
                        | (WorkItemState::Running, WorkAttemptState::Running)
                )
            {
                return Err(RepositoryError::StaleFence);
            }
            let backup_run_id = current
                .backup_run_id
                .as_deref()
                .ok_or(RepositoryError::RuntimeStateInvalid)?;
            let row: Option<BackupClaimContextRow> = transaction
                .query_row(
                    "SELECT backup.plan_occurrence_id, backup.plan_revision_id,
                            backup.database_target_id, run_scope.backup_scope_id,
                            target.connection_profile_id,
                            backup.engine, target.engine_object_id, target.display_name,
                            scope.display_name, run_scope.scope_mode,
                            backup.source_fingerprint, run_scope.database_source_fingerprint,
                            run_scope.resolved_object_count, run_scope.scope_snapshot_json,
                            run_scope.scope_snapshot_hash_blake3,
                            membership.target_options_json,
                            revision.execution_policy_json, backup.state
                     FROM backup_runs AS backup
                     JOIN backup_run_scopes AS run_scope
                       ON run_scope.backup_run_id = backup.id
                     JOIN backup_scopes AS scope
                       ON scope.id = run_scope.backup_scope_id
                     JOIN database_targets AS target
                       ON target.id = backup.database_target_id
                     JOIN connection_profiles AS profile
                       ON profile.id = target.connection_profile_id
                     JOIN plan_revision_targets AS membership
                       ON membership.plan_revision_id = backup.plan_revision_id
                      AND membership.database_target_id = backup.database_target_id
                     JOIN backup_plan_revisions AS revision
                       ON revision.id = backup.plan_revision_id
                     WHERE backup.id = ?1
                       AND profile.engine = backup.engine
                       AND revision.published_at_ms IS NOT NULL
                       AND profile.deleted_at_ms IS NULL",
                    [backup_run_id],
                    |row| {
                        Ok(BackupClaimContextRow {
                            plan_occurrence_id: row.get(0)?,
                            plan_revision_id: row.get(1)?,
                            database_target_id: row.get(2)?,
                            backup_scope_id: row.get(3)?,
                            connection_profile_id: row.get(4)?,
                            engine: row.get(5)?,
                            engine_object_id: row.get(6)?,
                            target_display_name: row.get(7)?,
                            scope_display_name: row.get(8)?,
                            scope_mode: row.get(9)?,
                            source_fingerprint: row.get(10)?,
                            database_source_fingerprint: row.get(11)?,
                            resolved_object_count: row.get(12)?,
                            scope_snapshot_json: row.get(13)?,
                            scope_snapshot_hash_blake3: row.get(14)?,
                            target_options_json: row.get(15)?,
                            execution_policy_json: row.get(16)?,
                            state: row.get(17)?,
                        })
                    },
                )
                .optional()?;
            let Some(row) = row else {
                return Err(RepositoryError::NotFound);
            };
            if !matches!(row.state.as_str(), "queued" | "running") {
                return Err(RepositoryError::RuntimeStateInvalid);
            }
            transaction.commit()?;
            Ok(BackupClaimContext {
                backup_run_id: BackupRunId::from_str(backup_run_id)?,
                plan_occurrence_id: PlanOccurrenceId::from_str(&row.plan_occurrence_id)?,
                plan_revision_id: PlanRevisionId::from_str(&row.plan_revision_id)?,
                database_target_id: DatabaseTargetId::from_str(&row.database_target_id)?,
                backup_scope_id: BackupScopeId::from_str(&row.backup_scope_id)?,
                connection_profile_id: ConnectionProfileId::from_str(&row.connection_profile_id)?,
                engine: row.engine,
                engine_object_id: row.engine_object_id,
                target_display_name: row.target_display_name,
                scope_display_name: row.scope_display_name,
                scope_mode: row.scope_mode,
                source_fingerprint: row.source_fingerprint,
                database_source_fingerprint: row.database_source_fingerprint,
                resolved_object_count: row.resolved_object_count,
                scope_snapshot_json: row.scope_snapshot_json,
                scope_snapshot_hash_blake3: row.scope_snapshot_hash_blake3,
                target_options_json: row.target_options_json,
                execution_policy_json: row.execution_policy_json,
            })
        })
    }

    pub fn verification_claim_context(
        &self,
        claim: &ClaimedWork,
        observed_at_ms: i64,
    ) -> Result<VerificationClaimContext, RepositoryError> {
        if claim.kind != WorkItemKind::Verification || observed_at_ms < 0 {
            return Err(RepositoryError::VerificationInputInvalid);
        }
        let claim = claim.clone();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let current = load_current_claim(&transaction, &claim, observed_at_ms)?;
            if current.kind != WorkItemKind::Verification.as_str() {
                return Err(RepositoryError::StaleFence);
            }
            let verification_run_id = current
                .verification_run_id
                .ok_or(RepositoryError::RuntimeStateInvalid)?;
            let row: Option<VerificationClaimRow> = transaction
                .query_row(
                    "SELECT verification.recovery_point_id,
                            verification.source_replica_id,
                            verification.requested_level,
                            verification.restore_run_id,
                            recovery.engine,
                            verification.state
                     FROM verification_runs AS verification
                     JOIN catalog_recovery_points AS recovery
                       ON recovery.id = verification.recovery_point_id
                     WHERE verification.id = ?1",
                    [verification_run_id.as_str()],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                        ))
                    },
                )
                .optional()?;
            let Some((
                recovery_point_id,
                source_replica_id,
                requested_level,
                restore_run_id,
                source_engine,
                state,
            )) = row
            else {
                return Err(RepositoryError::NotFound);
            };
            if !matches!(state.as_str(), "queued" | "running") {
                return Err(RepositoryError::RuntimeStateInvalid);
            }
            transaction.commit()?;
            Ok(VerificationClaimContext {
                verification_run_id: VerificationRunId::from_str(&verification_run_id)?,
                recovery_point_id: RecoveryPointId::from_str(&recovery_point_id)?,
                source_replica_id,
                requested_level,
                restore_run_id,
                source_engine,
            })
        })
    }

    pub fn restore_claim_context(
        &self,
        claim: &ClaimedWork,
        observed_at_ms: i64,
    ) -> Result<RestoreClaimContext, RepositoryError> {
        if claim.kind != WorkItemKind::Restore || observed_at_ms < 0 {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let claim = claim.clone();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let current = load_current_claim(&transaction, &claim, observed_at_ms)?;
            if current.kind != WorkItemKind::Restore.as_str()
                || !matches!(
                    (current.work_state, current.attempt_state),
                    (WorkItemState::Leased, WorkAttemptState::Claimed)
                        | (WorkItemState::Starting, WorkAttemptState::Starting)
                        | (WorkItemState::Running, WorkAttemptState::Running)
                )
            {
                return Err(RepositoryError::StaleFence);
            }
            let restore_run_id = current
                .restore_run_id
                .as_deref()
                .ok_or(RepositoryError::RuntimeStateInvalid)?;
            let row = load_restore_claim_row(&transaction, restore_run_id)?;
            if !matches!(row.9.as_str(), "queued" | "running") {
                return Err(RepositoryError::RuntimeStateInvalid);
            }
            let authoritatively_eligible = restore_context_is_authoritatively_eligible(
                &transaction,
                restore_run_id,
                &row,
                observed_at_ms,
                observed_at_ms,
                RestoreTargetObservation::default(),
            )?;
            let target_server_identity = serde_json::from_str::<serde_json::Value>(&row.7)
                .ok()
                .and_then(|snapshot| {
                    snapshot
                        .get("serverIdentity")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                });
            transaction.commit()?;
            Ok(RestoreClaimContext {
                restore_run_id: RestoreRunId::from_str(restore_run_id)?,
                recovery_point_id: RecoveryPointId::from_str(&row.0)?,
                source_replica_id: row.1,
                target_connection_profile_id: ConnectionProfileId::from_str(&row.2)?,
                target_database_name: row.3,
                target_fingerprint: row.4,
                target_server_identity,
                purpose: row.5,
                conflict_policy: row.6,
                eligibility_snapshot_json: row.7,
                authoritatively_eligible,
            })
        })
    }

    pub fn begin_restore_validation(
        &self,
        claim: &ClaimedWork,
        validation_started_at_ms: i64,
    ) -> Result<(), RepositoryError> {
        if claim.kind != WorkItemKind::Restore || validation_started_at_ms < 0 {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let claim = claim.clone();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let current = load_current_claim(&transaction, &claim, validation_started_at_ms)?;
            ensure_claim_phase(&current, WorkItemState::Running, WorkAttemptState::Running)?;
            ensure_no_active_process(&transaction, &claim)?;
            let restore_run_id = current
                .restore_run_id
                .as_deref()
                .ok_or(RepositoryError::RuntimeStateInvalid)?;
            let affected = transaction.execute(
                "UPDATE restore_runs
                 SET state = 'validating', updated_at_ms = ?2
                 WHERE id = ?1 AND state = 'running'",
                params![restore_run_id, validation_started_at_ms],
            )?;
            if affected != 1 {
                return Err(RepositoryError::StaleFence);
            }
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn admit_claim(
        &self,
        request: AdmissionRequest,
    ) -> Result<AdmissionOutcome, RepositoryError> {
        request.validate()?;
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let lease_expires_at_ms =
                current_claim_lease(&transaction, &request.claim, request.now_ms)?;

            let mut pools = Vec::with_capacity(request.resources.len());
            for requirement in &request.resources {
                let pool = transaction
                    .query_row(
                        "SELECT id, resource_type, resource_key, capacity, enabled
                         FROM resource_pools
                         WHERE id = ?1",
                        [requirement.resource_pool_id.as_str()],
                        |row| {
                            Ok(ResourcePoolRow {
                                id: row.get(0)?,
                                resource_type: row.get(1)?,
                                resource_key: row.get(2)?,
                                capacity: row.get(3)?,
                                enabled: row.get::<_, i64>(4)? == 1,
                                units: requirement.units,
                            })
                        },
                    )
                    .optional()?;
                let Some(pool) = pool else {
                    let reason = format!("resource_missing:{}", requirement.resource_pool_id);
                    record_admission_wait(&transaction, &request.claim, request.now_ms, &reason)?;
                    transaction.commit()?;
                    return Ok(waiting_admission(reason));
                };
                pools.push(pool);
            }
            pools.sort_by(|left, right| {
                (&left.resource_type, &left.resource_key, &left.id).cmp(&(
                    &right.resource_type,
                    &right.resource_key,
                    &right.id,
                ))
            });

            for pool in &pools {
                if !pool.enabled {
                    let reason = format!(
                        "resource_disabled:{}:{}",
                        pool.resource_type, pool.resource_key
                    );
                    record_admission_wait(&transaction, &request.claim, request.now_ms, &reason)?;
                    transaction.commit()?;
                    return Ok(waiting_admission(reason));
                }
                let used_units: i64 = transaction.query_row(
                    "SELECT COALESCE(SUM(units), 0)
                     FROM resource_allocations
                     WHERE resource_pool_id = ?1
                       AND work_attempt_id <> ?2
                       AND released_at_ms IS NULL
                       AND expires_at_ms >= ?3",
                    params![pool.id, request.claim.attempt_id.as_str(), request.now_ms],
                    |row| row.get(0),
                )?;
                let has_capacity = used_units
                    .checked_add(pool.units)
                    .map(|required| required <= pool.capacity)
                    .unwrap_or(false);
                if !has_capacity {
                    let reason = format!(
                        "resource_capacity:{}:{}",
                        pool.resource_type, pool.resource_key
                    );
                    record_admission_wait(&transaction, &request.claim, request.now_ms, &reason)?;
                    transaction.commit()?;
                    return Ok(waiting_admission(reason));
                }
            }

            let spool_row = if let Some(spool) = &request.spool {
                let volume = transaction
                    .query_row(
                        "SELECT
                             id, volume_key, hard_limit_bytes, reserve_floor_bytes,
                             state, last_free_bytes
                         FROM spool_volumes
                         WHERE id = ?1",
                        [spool.spool_volume_id.as_str()],
                        |row| {
                            Ok(SpoolVolumeRow {
                                id: row.get(0)?,
                                volume_key: row.get(1)?,
                                hard_limit_bytes: row.get(2)?,
                                reserve_floor_bytes: row.get(3)?,
                                state: row.get(4)?,
                                last_free_bytes: row.get(5)?,
                            })
                        },
                    )
                    .optional()?;
                let Some(volume) = volume else {
                    let reason = format!("spool_missing:{}", spool.spool_volume_id);
                    record_admission_wait(&transaction, &request.claim, request.now_ms, &reason)?;
                    transaction.commit()?;
                    return Ok(waiting_admission(reason));
                };
                if volume.state != "ready" {
                    let reason = format!("spool_state:{}:{}", volume.volume_key, volume.state);
                    record_admission_wait(&transaction, &request.claim, request.now_ms, &reason)?;
                    transaction.commit()?;
                    return Ok(waiting_admission(reason));
                }
                let active_reserved: i64 = transaction.query_row(
                    "SELECT COALESCE(SUM(reserved_bytes), 0)
                     FROM spool_reservations
                     WHERE spool_volume_id = ?1
                       AND work_attempt_id <> ?2
                       AND released_at_ms IS NULL
                       AND state <> 'released'",
                    params![volume.id, request.claim.attempt_id.as_str()],
                    |row| row.get(0),
                )?;
                let hard_available = volume
                    .hard_limit_bytes
                    .checked_sub(active_reserved)
                    .unwrap_or(0);
                let observed_available = volume
                    .last_free_bytes
                    .and_then(|free| free.checked_sub(volume.reserve_floor_bytes))
                    .and_then(|free_above_floor| free_above_floor.checked_sub(active_reserved))
                    .unwrap_or(0);
                if spool.reserved_bytes > hard_available
                    || spool.reserved_bytes > observed_available
                {
                    let reason = format!("spool_capacity:{}", volume.volume_key);
                    record_admission_wait(&transaction, &request.claim, request.now_ms, &reason)?;
                    transaction.commit()?;
                    return Ok(waiting_admission(reason));
                }
                Some(volume)
            } else {
                None
            };

            for pool in &pools {
                insert_or_validate_allocation(
                    &transaction,
                    &request.claim,
                    pool,
                    request.now_ms,
                    lease_expires_at_ms,
                )?;
            }
            if let (Some(spool), Some(volume)) = (&request.spool, &spool_row) {
                insert_or_validate_spool_reservation(
                    &transaction,
                    &request.claim,
                    spool,
                    volume,
                    request.now_ms,
                )?;
            }
            let affected = transaction.execute(
                "UPDATE work_items
                 SET wait_reason = NULL, updated_at_ms = ?7
                 WHERE id = ?1
                   AND current_attempt_no = ?2
                   AND fencing_token = ?3
                   AND lease_owner = ?4
                   AND lease_boot_id = ?5
                   AND state = 'leased'
                   AND lease_expires_at_ms >= ?6",
                params![
                    request.claim.work_item_id.as_str(),
                    request.claim.attempt_no,
                    request.claim.fencing_token,
                    request.claim.worker_id,
                    request.claim.worker_boot_id,
                    request.now_ms,
                    request.now_ms
                ],
            )?;
            if affected != 1 {
                return Err(RepositoryError::StaleFence);
            }
            transaction.commit()?;
            Ok(AdmissionOutcome {
                admitted: true,
                wait_reason: None,
                allocation_count: pools.len(),
                spool_reserved: spool_row.is_some(),
            })
        })
    }

    pub fn defer_claim(
        &self,
        claim: &ClaimedWork,
        deferred_at_ms: i64,
        available_at_ms: i64,
        wait_reason: &str,
    ) -> Result<(), RepositoryError> {
        if deferred_at_ms < 0
            || available_at_ms < deferred_at_ms
            || wait_reason.is_empty()
            || wait_reason.len() > 1024
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let claim = claim.clone();
        let wait_reason = wait_reason.to_owned();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let current = load_current_claim(&transaction, &claim, deferred_at_ms)?;
            ensure_claim_phase(&current, WorkItemState::Leased, WorkAttemptState::Claimed)?;
            ensure_no_active_process(&transaction, &claim)?;
            let queued = transition_work_item(current.work_state, WorkItemCommand::Release)?;
            let delayed = transition_work_item(queued, WorkItemCommand::Delay)?;
            let abandoned =
                transition_work_attempt(current.attempt_state, WorkAttemptCommand::Abandon)?;

            let item_affected = transaction.execute(
                "UPDATE work_items
                 SET state = ?7, available_at_ms = ?8, lease_owner = NULL,
                     lease_boot_id = NULL, lease_expires_at_ms = NULL,
                     heartbeat_at_ms = NULL, wait_reason = ?9, updated_at_ms = ?10
                 WHERE id = ?1
                   AND current_attempt_no = ?2
                   AND fencing_token = ?3
                   AND lease_owner = ?4
                   AND lease_boot_id = ?5
                   AND state = ?6",
                params![
                    claim.work_item_id.as_str(),
                    claim.attempt_no,
                    claim.fencing_token,
                    claim.worker_id.as_str(),
                    claim.worker_boot_id.as_str(),
                    current.work_state.as_str(),
                    delayed.as_str(),
                    available_at_ms,
                    wait_reason,
                    deferred_at_ms
                ],
            )?;
            let attempt_affected = transaction.execute(
                "UPDATE work_attempts
                 SET state = ?7, completed_at_ms = ?8, retry_class = 'reconcile',
                     retry_after_ms = ?9, error_code = 'ADMISSION_DEFERRED'
                 WHERE id = ?1
                   AND work_item_id = ?2
                   AND attempt_no = ?3
                   AND fencing_token = ?4
                   AND worker_id = ?5
                   AND worker_boot_id = ?6
                   AND state = ?10",
                params![
                    claim.attempt_id.as_str(),
                    claim.work_item_id.as_str(),
                    claim.attempt_no,
                    claim.fencing_token,
                    claim.worker_id.as_str(),
                    claim.worker_boot_id.as_str(),
                    abandoned.as_str(),
                    deferred_at_ms,
                    available_at_ms,
                    current.attempt_state.as_str()
                ],
            )?;
            if item_affected != 1 || attempt_affected != 1 {
                return Err(RepositoryError::StaleFence);
            }
            release_claim_capacity(&transaction, &claim, deferred_at_ms, "reconciled", false)?;
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn start_claim(
        &self,
        claim: &ClaimedWork,
        started_at_ms: i64,
    ) -> Result<(), RepositoryError> {
        if started_at_ms < 0 {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let claim = claim.clone();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let current = load_current_claim(&transaction, &claim, started_at_ms)?;
            ensure_claim_phase(&current, WorkItemState::Leased, WorkAttemptState::Claimed)?;
            let next_work = transition_work_item(current.work_state, WorkItemCommand::Start)?;
            let next_attempt =
                transition_work_attempt(current.attempt_state, WorkAttemptCommand::Start)?;
            advance_subject_start(&transaction, &current, started_at_ms)?;

            let item_affected = transaction.execute(
                "UPDATE work_items
                 SET state = ?7, updated_at_ms = ?8
                 WHERE id = ?1
                   AND current_attempt_no = ?2
                   AND fencing_token = ?3
                   AND lease_owner = ?4
                   AND lease_boot_id = ?5
                   AND state = ?6
                   AND lease_expires_at_ms >= ?8",
                params![
                    claim.work_item_id.as_str(),
                    claim.attempt_no,
                    claim.fencing_token,
                    claim.worker_id.as_str(),
                    claim.worker_boot_id.as_str(),
                    current.work_state.as_str(),
                    next_work.as_str(),
                    started_at_ms
                ],
            )?;
            let attempt_affected = transaction.execute(
                "UPDATE work_attempts
                 SET state = ?7, started_at_ms = ?8
                 WHERE id = ?1
                   AND work_item_id = ?2
                   AND attempt_no = ?3
                   AND fencing_token = ?4
                   AND worker_id = ?5
                   AND worker_boot_id = ?6
                   AND state = ?9
                   AND lease_expires_at_ms >= ?8",
                params![
                    claim.attempt_id.as_str(),
                    claim.work_item_id.as_str(),
                    claim.attempt_no,
                    claim.fencing_token,
                    claim.worker_id.as_str(),
                    claim.worker_boot_id.as_str(),
                    next_attempt.as_str(),
                    started_at_ms,
                    current.attempt_state.as_str()
                ],
            )?;
            if item_affected != 1 || attempt_affected != 1 {
                return Err(RepositoryError::StaleFence);
            }
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn mark_in_process_claim_running(
        &self,
        claim: &ClaimedWork,
        observed_at_ms: i64,
    ) -> Result<(), RepositoryError> {
        if !matches!(
            claim.kind,
            WorkItemKind::Backup
                | WorkItemKind::Restore
                | WorkItemKind::Verification
                | WorkItemKind::Operation
        ) || observed_at_ms < 0
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let claim = claim.clone();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let current = load_current_claim(&transaction, &claim, observed_at_ms)?;
            ensure_no_active_process(&transaction, &claim)?;
            ensure_claim_phase(
                &current,
                WorkItemState::Starting,
                WorkAttemptState::Starting,
            )?;
            let next_work = transition_work_item(current.work_state, WorkItemCommand::MarkRunning)?;
            let next_attempt =
                transition_work_attempt(current.attempt_state, WorkAttemptCommand::MarkRunning)?;
            update_claim_phase(
                &transaction,
                &claim,
                &current,
                next_work,
                next_attempt,
                observed_at_ms,
            )?;
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn record_process_start(&self, spec: ProcessStartSpec) -> Result<(), RepositoryError> {
        spec.validate()?;
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let current = load_current_claim(&transaction, &spec.claim, spec.started_at_ms)?;
            if !matches!(
                (current.work_state, current.attempt_state),
                (WorkItemState::Starting, WorkAttemptState::Starting)
                    | (WorkItemState::Running, WorkAttemptState::Running)
            ) {
                return Err(RepositoryError::StaleFence);
            }
            ensure_no_active_process(&transaction, &spec.claim)?;
            transaction.execute(
                "INSERT INTO process_instances(
                     id, work_attempt_id, phase, process_id, process_start_time_ms,
                     service_boot_id, job_object_key, executable_path,
                     executable_sha256, state, started_at_ms, ended_at_ms,
                     exit_code, termination_reason, created_at_ms
                 ) VALUES (
                     ?1, ?2, ?3, ?4, ?5,
                     ?6, ?7, ?8,
                     ?9, 'starting', ?10, NULL,
                     NULL, NULL, ?10
                 )",
                params![
                    spec.process_instance_id,
                    spec.claim.attempt_id.as_str(),
                    spec.phase,
                    spec.process_id,
                    spec.process_start_time_ms,
                    spec.service_boot_id,
                    spec.job_object_key,
                    spec.executable_path,
                    spec.executable_sha256,
                    spec.started_at_ms
                ],
            )?;
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn mark_process_running(
        &self,
        claim: &ClaimedWork,
        process_instance_id: &str,
        observed_at_ms: i64,
    ) -> Result<(), RepositoryError> {
        if !is_canonical_uuid(process_instance_id) || observed_at_ms < 0 {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let claim = claim.clone();
        let process_instance_id = process_instance_id.to_owned();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let current = load_current_claim(&transaction, &claim, observed_at_ms)?;
            let first_process = matches!(
                (current.work_state, current.attempt_state),
                (WorkItemState::Starting, WorkAttemptState::Starting)
            );
            if !first_process
                && !matches!(
                    (current.work_state, current.attempt_state),
                    (WorkItemState::Running, WorkAttemptState::Running)
                )
            {
                return Err(RepositoryError::StaleFence);
            }
            let process_affected = transaction.execute(
                "UPDATE process_instances
                 SET state = 'running'
                 WHERE id = ?1
                   AND work_attempt_id = ?2
                   AND service_boot_id = ?3
                   AND state = 'starting'",
                params![
                    process_instance_id,
                    claim.attempt_id.as_str(),
                    claim.worker_boot_id
                ],
            )?;
            if process_affected != 1 {
                return Err(RepositoryError::StaleFence);
            }
            if first_process {
                let next_work =
                    transition_work_item(current.work_state, WorkItemCommand::MarkRunning)?;
                let next_attempt = transition_work_attempt(
                    current.attempt_state,
                    WorkAttemptCommand::MarkRunning,
                )?;
                update_claim_phase(
                    &transaction,
                    &claim,
                    &current,
                    next_work,
                    next_attempt,
                    observed_at_ms,
                )?;
            }
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn record_process_exit(
        &self,
        claim: &ClaimedWork,
        spec: ProcessExitSpec,
    ) -> Result<(), RepositoryError> {
        spec.validate()?;
        let claim = claim.clone();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let current = load_current_claim(&transaction, &claim, spec.ended_at_ms)?;
            let valid_phase = matches!(
                (current.work_state, current.attempt_state),
                (WorkItemState::Starting, WorkAttemptState::Starting)
                    | (WorkItemState::Running, WorkAttemptState::Running)
                    | (WorkItemState::CancelRequested, WorkAttemptState::Starting)
                    | (WorkItemState::CancelRequested, WorkAttemptState::Running)
            );
            if !valid_phase {
                return Err(RepositoryError::StaleFence);
            }
            let started_at_ms: Option<i64> = transaction
                .query_row(
                    "SELECT started_at_ms
                     FROM process_instances
                     WHERE id = ?1
                       AND work_attempt_id = ?2
                       AND service_boot_id = ?3
                       AND state IN ('starting', 'running')",
                    params![
                        spec.process_instance_id,
                        claim.attempt_id.as_str(),
                        claim.worker_boot_id
                    ],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(started_at_ms) = started_at_ms else {
                return Err(RepositoryError::StaleFence);
            };
            if spec.ended_at_ms < started_at_ms {
                return Err(RepositoryError::RuntimeStateInvalid);
            }
            let affected = transaction.execute(
                "UPDATE process_instances
                 SET state = 'exited', ended_at_ms = ?4, exit_code = ?5,
                     termination_reason = ?6
                 WHERE id = ?1
                   AND work_attempt_id = ?2
                   AND service_boot_id = ?3
                   AND state IN ('starting', 'running')",
                params![
                    spec.process_instance_id,
                    claim.attempt_id.as_str(),
                    claim.worker_boot_id,
                    spec.ended_at_ms,
                    spec.exit_code,
                    spec.termination_reason
                ],
            )?;
            if affected != 1 {
                return Err(RepositoryError::StaleFence);
            }
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn begin_claim_commit(
        &self,
        claim: &ClaimedWork,
        observed_at_ms: i64,
    ) -> Result<BeginCommitDisposition, RepositoryError> {
        self.begin_claim_commit_with_recovery_intent(claim, None, observed_at_ms)
    }

    pub fn begin_backup_claim_commit(
        &self,
        claim: &ClaimedWork,
        recovery_point_id: &RecoveryPointId,
        observed_at_ms: i64,
    ) -> Result<BeginCommitDisposition, RepositoryError> {
        if claim.kind != WorkItemKind::Backup {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        self.begin_claim_commit_with_recovery_intent(
            claim,
            Some(recovery_point_id.clone()),
            observed_at_ms,
        )
    }

    fn begin_claim_commit_with_recovery_intent(
        &self,
        claim: &ClaimedWork,
        recovery_point_id: Option<RecoveryPointId>,
        observed_at_ms: i64,
    ) -> Result<BeginCommitDisposition, RepositoryError> {
        if observed_at_ms < 0 {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let claim = claim.clone();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let current = load_current_claim(&transaction, &claim, observed_at_ms)?;
            ensure_no_active_process(&transaction, &claim)?;
            if current.work_state == WorkItemState::CancelRequested {
                if !matches!(
                    current.attempt_state,
                    WorkAttemptState::Starting | WorkAttemptState::Running
                ) {
                    return Err(RepositoryError::StaleFence);
                }
                transaction.commit()?;
                return Ok(BeginCommitDisposition::CancelRequested);
            }
            ensure_claim_phase(&current, WorkItemState::Running, WorkAttemptState::Running)?;
            let next_work = transition_work_item(current.work_state, WorkItemCommand::BeginCommit)?;
            let next_attempt =
                transition_work_attempt(current.attempt_state, WorkAttemptCommand::BeginCommit)?;
            advance_subject_commit(&transaction, &current, observed_at_ms)?;
            if let Some(recovery_point_id) = recovery_point_id {
                let backup_run_id = current
                    .backup_run_id
                    .as_deref()
                    .ok_or(RepositoryError::RuntimeStateInvalid)?;
                let affected = transaction.execute(
                    "UPDATE backup_runs
                     SET recovery_point_id = ?2, updated_at_ms = ?3
                     WHERE id = ?1
                       AND state = 'committing'
                       AND recovery_point_id IS NULL",
                    params![backup_run_id, recovery_point_id.as_str(), observed_at_ms],
                )?;
                if affected != 1 {
                    return Err(RepositoryError::StaleFence);
                }
            }
            update_claim_phase(
                &transaction,
                &claim,
                &current,
                next_work,
                next_attempt,
                observed_at_ms,
            )?;
            transaction.commit()?;
            Ok(BeginCommitDisposition::Committing)
        })
    }

    pub fn complete_claim(
        &self,
        mut spec: ClaimCompletionSpec,
    ) -> Result<ClaimTerminalOutcome, RepositoryError> {
        spec.validate()?;
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let current = load_current_claim(&transaction, &spec.claim, spec.completed_at_ms)?;
            ensure_no_active_process(&transaction, &spec.claim)?;
            if current.work_state == WorkItemState::CancelRequested
                && spec.outcome != ClaimTerminalOutcome::Cancelled
            {
                spec.outcome = ClaimTerminalOutcome::Cancelled;
                spec.error_code = Some("CANCEL_REQUESTED".to_owned());
            }
            settle_claim_terminal(&transaction, &current, &spec)?;
            let outcome = spec.outcome;
            transaction.commit()?;
            Ok(outcome)
        })
    }

    pub fn commit_backup_claim(
        &self,
        claim: &ClaimedWork,
        spec: CompleteRecoveryPointSpec,
        outcome: ClaimTerminalOutcome,
        completed_at_ms: i64,
        result_json: Option<String>,
    ) -> Result<ClaimTerminalOutcome, RepositoryError> {
        if claim.kind != WorkItemKind::Backup
            || !matches!(
                outcome,
                ClaimTerminalOutcome::Succeeded | ClaimTerminalOutcome::PartiallySucceeded
            )
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        spec.validate()?;
        let plaintext_bytes = spec.plaintext_bytes()?;
        let completion = match outcome {
            ClaimTerminalOutcome::Succeeded => {
                ClaimCompletionSpec::succeeded(claim.clone(), completed_at_ms, result_json)
            }
            ClaimTerminalOutcome::PartiallySucceeded => ClaimCompletionSpec::partially_succeeded(
                claim.clone(),
                completed_at_ms,
                result_json,
                Some("BACKUP_REPLICA_PARTIAL".to_owned()),
            ),
            ClaimTerminalOutcome::Failed | ClaimTerminalOutcome::Cancelled => unreachable!(),
        };
        completion.validate()?;
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let current = load_current_claim(&transaction, &completion.claim, completed_at_ms)?;
            ensure_no_active_process(&transaction, &completion.claim)?;
            ensure_claim_phase(
                &current,
                WorkItemState::Committing,
                WorkAttemptState::Committing,
            )?;
            if current.kind != WorkItemKind::Backup.as_str() {
                return Err(RepositoryError::StaleFence);
            }
            let backup_run_id = current
                .backup_run_id
                .as_deref()
                .ok_or(RepositoryError::RuntimeStateInvalid)?;
            validate_backup_commit_manifest_binding(&transaction, backup_run_id, &spec)?;

            ingest_complete_projection(&transaction, &spec)?;
            schedule_post_backup_in_transaction(&transaction, spec.id(), completed_at_ms)?;
            let linked = transaction.execute(
                "UPDATE backup_runs
                 SET recovery_point_id = ?2, plaintext_bytes = ?3, updated_at_ms = ?4
                 WHERE id = ?1
                   AND state = 'committing'
                   AND (recovery_point_id IS NULL OR recovery_point_id = ?2)",
                params![
                    backup_run_id,
                    spec.id().as_str(),
                    plaintext_bytes,
                    completed_at_ms
                ],
            )?;
            if linked != 1 {
                return Err(RepositoryError::StaleFence);
            }
            settle_claim_terminal(&transaction, &current, &completion)?;
            transaction.commit()?;
            Ok(outcome)
        })
    }

    pub fn heartbeat_claim(
        &self,
        claim: &ClaimedWork,
        heartbeat_at_ms: i64,
        lease_expires_at_ms: i64,
    ) -> Result<(), RepositoryError> {
        if heartbeat_at_ms < 0 || lease_expires_at_ms < heartbeat_at_ms {
            return Err(RepositoryError::LeaseInvalid);
        }
        let claim = claim.clone();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let item_affected = transaction.execute(
                "UPDATE work_items
                 SET heartbeat_at_ms = ?7, lease_expires_at_ms = ?8, updated_at_ms = ?7
                 WHERE id = ?1
                   AND current_attempt_no = ?2
                   AND fencing_token = ?3
                   AND lease_owner = ?4
                   AND lease_boot_id = ?5
                   AND state IN ('leased', 'starting', 'running', 'committing', 'cancel_requested')
                   AND lease_expires_at_ms >= ?6",
                params![
                    claim.work_item_id.as_str(),
                    claim.attempt_no,
                    claim.fencing_token,
                    claim.worker_id,
                    claim.worker_boot_id,
                    heartbeat_at_ms,
                    heartbeat_at_ms,
                    lease_expires_at_ms
                ],
            )?;
            if item_affected != 1 {
                return Err(RepositoryError::StaleFence);
            }
            let attempt_affected = transaction.execute(
                "UPDATE work_attempts
                 SET last_heartbeat_at_ms = ?7, lease_expires_at_ms = ?8
                 WHERE id = ?1
                   AND work_item_id = ?2
                   AND attempt_no = ?3
                   AND fencing_token = ?4
                   AND worker_id = ?5
                   AND worker_boot_id = ?6
                   AND state IN ('claimed', 'starting', 'running', 'committing')",
                params![
                    claim.attempt_id.as_str(),
                    claim.work_item_id.as_str(),
                    claim.attempt_no,
                    claim.fencing_token,
                    claim.worker_id,
                    claim.worker_boot_id,
                    heartbeat_at_ms,
                    lease_expires_at_ms
                ],
            )?;
            if attempt_affected != 1 {
                return Err(RepositoryError::StaleFence);
            }
            renew_claim_allocations(&transaction, &claim, lease_expires_at_ms)?;
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn renew_destructive_step(
        &self,
        claim: &ClaimedWork,
        heartbeat_at_ms: i64,
        lease_expires_at_ms: i64,
    ) -> Result<DestructiveStepDisposition, RepositoryError> {
        if heartbeat_at_ms < 0 || lease_expires_at_ms <= heartbeat_at_ms {
            return Err(RepositoryError::LeaseInvalid);
        }
        let claim = claim.clone();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let current = load_current_claim(&transaction, &claim, heartbeat_at_ms)?;
            if current.work_state == WorkItemState::CancelRequested
                && matches!(
                    current.attempt_state,
                    WorkAttemptState::Starting | WorkAttemptState::Running
                )
            {
                transaction.commit()?;
                return Ok(DestructiveStepDisposition::CancelRequested);
            }
            if !matches!(
                (current.work_state, current.attempt_state),
                (WorkItemState::Starting, WorkAttemptState::Starting)
                    | (WorkItemState::Running, WorkAttemptState::Running)
            ) {
                return Err(RepositoryError::StaleFence);
            }
            validate_automatic_retention_destructive_step(&transaction, &current, heartbeat_at_ms)?;
            let item_affected = transaction.execute(
                "UPDATE work_items
                 SET heartbeat_at_ms = ?7, lease_expires_at_ms = ?8, updated_at_ms = ?7
                 WHERE id = ?1
                   AND current_attempt_no = ?2
                   AND fencing_token = ?3
                   AND lease_owner = ?4
                   AND lease_boot_id = ?5
                   AND state = ?6
                   AND lease_expires_at_ms >= ?7",
                params![
                    claim.work_item_id.as_str(),
                    claim.attempt_no,
                    claim.fencing_token,
                    claim.worker_id.as_str(),
                    claim.worker_boot_id.as_str(),
                    current.work_state.as_str(),
                    heartbeat_at_ms,
                    lease_expires_at_ms
                ],
            )?;
            let attempt_affected = transaction.execute(
                "UPDATE work_attempts
                 SET last_heartbeat_at_ms = ?7, lease_expires_at_ms = ?8
                 WHERE id = ?1
                   AND work_item_id = ?2
                   AND attempt_no = ?3
                   AND fencing_token = ?4
                   AND worker_id = ?5
                   AND worker_boot_id = ?6
                   AND state = ?9
                   AND lease_expires_at_ms >= ?7",
                params![
                    claim.attempt_id.as_str(),
                    claim.work_item_id.as_str(),
                    claim.attempt_no,
                    claim.fencing_token,
                    claim.worker_id.as_str(),
                    claim.worker_boot_id.as_str(),
                    heartbeat_at_ms,
                    lease_expires_at_ms,
                    current.attempt_state.as_str()
                ],
            )?;
            if item_affected != 1 || attempt_affected != 1 {
                return Err(RepositoryError::StaleFence);
            }
            renew_claim_allocations(&transaction, &claim, lease_expires_at_ms)?;
            transaction.commit()?;
            Ok(DestructiveStepDisposition::Proceed)
        })
    }

    pub fn begin_restore_destructive_step(
        &self,
        claim: &ClaimedWork,
        heartbeat_at_ms: i64,
        lease_expires_at_ms: i64,
        live_server_version: Option<&str>,
        live_server_identity: Option<&str>,
        live_target_exists: bool,
    ) -> Result<DestructiveStepDisposition, RepositoryError> {
        if claim.kind != WorkItemKind::Restore
            || heartbeat_at_ms < 0
            || lease_expires_at_ms <= heartbeat_at_ms
            || live_server_version.is_some_and(|value| {
                value.trim().is_empty() || value.len() > 512 || value.contains(['\0', '\r', '\n'])
            })
            || live_server_identity.is_some_and(|value| {
                value.trim().is_empty() || value.len() > 256 || value.contains(['\0', '\r', '\n'])
            })
        {
            return Err(RepositoryError::LeaseInvalid);
        }
        let claim = claim.clone();
        let live_server_version = live_server_version.map(str::to_owned);
        let live_server_identity = live_server_identity.map(str::to_owned);
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let current = load_current_claim(&transaction, &claim, heartbeat_at_ms)?;
            if current.work_state == WorkItemState::CancelRequested
                && current.attempt_state == WorkAttemptState::Running
            {
                transaction.commit()?;
                return Ok(DestructiveStepDisposition::CancelRequested);
            }
            ensure_claim_phase(&current, WorkItemState::Running, WorkAttemptState::Running)?;
            ensure_no_active_process(&transaction, &claim)?;
            let restore_run_id = current
                .restore_run_id
                .as_deref()
                .ok_or(RepositoryError::RuntimeStateInvalid)?;
            let authorization_at_ms: i64 = transaction.query_row(
                "SELECT lease_acquired_at_ms
                 FROM work_attempts
                 WHERE id = ?1 AND work_item_id = ?2",
                params![claim.attempt_id.as_str(), claim.work_item_id.as_str()],
                |row| row.get(0),
            )?;
            let restore_row = load_restore_claim_row(&transaction, restore_run_id)?;
            if restore_row.9 != "running" {
                return Err(RepositoryError::StaleFence);
            }
            if !restore_context_is_authoritatively_eligible(
                &transaction,
                restore_run_id,
                &restore_row,
                authorization_at_ms,
                heartbeat_at_ms,
                RestoreTargetObservation {
                    server_version: live_server_version.as_deref(),
                    server_identity: live_server_identity.as_deref(),
                    target_exists: Some(live_target_exists),
                },
            )? {
                return Err(RepositoryError::RestorePreflightStale);
            }
            let marker = serde_json::json!({
                "schemaVersion": 1,
                "phase": "destructive_pending",
                "attemptId": claim.attempt_id.as_str(),
                "fencingToken": claim.fencing_token,
                "cleanup": {
                    "required": true,
                    "reason": "RESTORE_DESTRUCTIVE_STEP_ARMED",
                    "targetPreserved": true
                }
            })
            .to_string();
            let restore_affected = transaction.execute(
                "UPDATE restore_runs
                 SET validation_result_json = ?2, updated_at_ms = ?3
                 WHERE id = ?1
                   AND state = 'running'
                   AND validation_result_json IS NULL",
                params![restore_run_id, marker, heartbeat_at_ms],
            )?;
            let item_affected = transaction.execute(
                "UPDATE work_items
                 SET heartbeat_at_ms = ?7, lease_expires_at_ms = ?8, updated_at_ms = ?7
                 WHERE id = ?1
                   AND current_attempt_no = ?2
                   AND fencing_token = ?3
                   AND lease_owner = ?4
                   AND lease_boot_id = ?5
                   AND state = 'running'
                   AND lease_expires_at_ms >= ?6",
                params![
                    claim.work_item_id.as_str(),
                    claim.attempt_no,
                    claim.fencing_token,
                    claim.worker_id.as_str(),
                    claim.worker_boot_id.as_str(),
                    heartbeat_at_ms,
                    heartbeat_at_ms,
                    lease_expires_at_ms
                ],
            )?;
            let attempt_affected = transaction.execute(
                "UPDATE work_attempts
                 SET last_heartbeat_at_ms = ?7, lease_expires_at_ms = ?8
                 WHERE id = ?1
                   AND work_item_id = ?2
                   AND attempt_no = ?3
                   AND fencing_token = ?4
                   AND worker_id = ?5
                   AND worker_boot_id = ?6
                   AND state = 'running'
                   AND lease_expires_at_ms >= ?7",
                params![
                    claim.attempt_id.as_str(),
                    claim.work_item_id.as_str(),
                    claim.attempt_no,
                    claim.fencing_token,
                    claim.worker_id.as_str(),
                    claim.worker_boot_id.as_str(),
                    heartbeat_at_ms,
                    lease_expires_at_ms
                ],
            )?;
            if restore_affected != 1 || item_affected != 1 || attempt_affected != 1 {
                return Err(RepositoryError::StaleFence);
            }
            renew_claim_allocations(&transaction, &claim, lease_expires_at_ms)?;
            transaction.commit()?;
            Ok(DestructiveStepDisposition::Proceed)
        })
    }

    pub fn request_cancel(
        &self,
        work_item_id: &WorkItemId,
        expected_fencing_token: i64,
        requested_at_ms: i64,
    ) -> Result<CancelRequestOutcome, RepositoryError> {
        if expected_fencing_token < 0 || requested_at_ms < 0 {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let work_item_id = work_item_id.clone();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let current: Option<(String, i64, i64)> = transaction
                .query_row(
                    "SELECT state, fencing_token, updated_at_ms
                     FROM work_items
                     WHERE id = ?1",
                    [work_item_id.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            let Some((state, fencing_token, updated_at_ms)) = current else {
                return Err(RepositoryError::NotFound);
            };
            if fencing_token != expected_fencing_token {
                return Err(RepositoryError::StaleFence);
            }
            let state = WorkItemState::from_str(&state)?;
            let outcome = match state {
                WorkItemState::CancelRequested => CancelRequestOutcome::AlreadyRequested,
                WorkItemState::Succeeded
                | WorkItemState::PartiallySucceeded
                | WorkItemState::FailedTerminal
                | WorkItemState::Cancelled => CancelRequestOutcome::AlreadyTerminal,
                WorkItemState::Committing => CancelRequestOutcome::CommitInProgress,
                WorkItemState::Leased | WorkItemState::Starting | WorkItemState::Running => {
                    if requested_at_ms < updated_at_ms {
                        return Err(RepositoryError::RuntimeStateInvalid);
                    }
                    let next = transition_work_item(state, WorkItemCommand::RequestCancel)?;
                    let affected = transaction.execute(
                        "UPDATE work_items
                         SET state = ?4,
                             cancellation_requested_at_ms =
                                 COALESCE(cancellation_requested_at_ms, ?3),
                             updated_at_ms = ?3
                         WHERE id = ?1
                           AND fencing_token = ?2
                           AND state = ?5",
                        params![
                            work_item_id.as_str(),
                            expected_fencing_token,
                            requested_at_ms,
                            next.as_str(),
                            state.as_str()
                        ],
                    )?;
                    if affected != 1 {
                        return Err(RepositoryError::StaleFence);
                    }
                    CancelRequestOutcome::Requested
                }
                WorkItemState::Queued | WorkItemState::Delayed => {
                    if requested_at_ms < updated_at_ms {
                        return Err(RepositoryError::RuntimeStateInvalid);
                    }
                    let affected = transaction.execute(
                        "UPDATE work_items
                         SET state = 'cancelled',
                             cancellation_requested_at_ms =
                                 COALESCE(cancellation_requested_at_ms, ?3),
                             terminal_at_ms = ?3,
                             updated_at_ms = ?3
                         WHERE id = ?1
                           AND fencing_token = ?2
                           AND state = ?4",
                        params![
                            work_item_id.as_str(),
                            expected_fencing_token,
                            requested_at_ms,
                            state.as_str()
                        ],
                    )?;
                    if affected != 1 {
                        return Err(RepositoryError::StaleFence);
                    }
                    transaction.execute(
                        "UPDATE backup_runs
                         SET state = 'cancelled', completed_at_ms = ?2, updated_at_ms = ?2
                         WHERE id = (
                             SELECT backup_run_id FROM work_items WHERE id = ?1
                         )",
                        params![work_item_id.as_str(), requested_at_ms],
                    )?;
                    transaction.execute(
                        "UPDATE restore_runs
                         SET state = 'cancelled', completed_at_ms = ?2, updated_at_ms = ?2
                         WHERE id = (
                             SELECT restore_run_id FROM work_items WHERE id = ?1
                         )",
                        params![work_item_id.as_str(), requested_at_ms],
                    )?;
                    transaction.execute(
                        "UPDATE verification_runs
                         SET state = 'cancelled',
                             completed_at_ms = ?2,
                             error_code = 'RESTORE_CANCELLED',
                             updated_at_ms = ?2
                         WHERE restore_run_id = (
                             SELECT restore_run_id FROM work_items WHERE id = ?1
                         )
                           AND trigger_kind = 'restore_drill'
                           AND requested_level = 3
                           AND state = 'queued'",
                        params![work_item_id.as_str(), requested_at_ms],
                    )?;
                    transaction.execute(
                        "UPDATE verification_runs
                         SET state = 'cancelled', completed_at_ms = ?2, updated_at_ms = ?2
                         WHERE id = (
                             SELECT verification_run_id FROM work_items WHERE id = ?1
                         )",
                        params![work_item_id.as_str(), requested_at_ms],
                    )?;
                    let terminal_verification_ids = {
                        let mut statement = transaction.prepare(
                            "SELECT verification.id
                             FROM work_items AS work
                             JOIN verification_runs AS verification
                               ON verification.id = work.verification_run_id
                             WHERE work.id = ?1 AND verification.state = 'cancelled'
                             UNION ALL
                             SELECT verification.id
                             FROM work_items AS work
                             JOIN verification_runs AS verification
                               ON verification.restore_run_id = work.restore_run_id
                              AND verification.trigger_kind = 'restore_drill'
                              AND verification.requested_level = 3
                             WHERE work.id = ?1 AND verification.state = 'cancelled'",
                        )?;
                        let rows = statement
                            .query_map([work_item_id.as_str()], |row| row.get::<_, String>(0))?
                            .collect::<Result<Vec<_>, _>>()?;
                        rows
                    };
                    for verification_id in terminal_verification_ids {
                        project_terminal_verification_run(
                            &transaction,
                            &verification_id,
                            requested_at_ms,
                        )?;
                    }
                    transaction.execute(
                        "UPDATE catalog_rebuild_runs
                         SET status = 'cancelled', completed_at_ms = ?2
                         WHERE id = (
                             SELECT catalog_rebuild_run_id FROM work_items WHERE id = ?1
                         )",
                        params![work_item_id.as_str(), requested_at_ms],
                    )?;
                    transaction.execute(
                        "UPDATE operation_runs
                         SET state = 'cancelled', completed_at_ms = ?2, updated_at_ms = ?2
                         WHERE id = (
                             SELECT operation_run_id FROM work_items WHERE id = ?1
                         )",
                        params![work_item_id.as_str(), requested_at_ms],
                    )?;
                    CancelRequestOutcome::Requested
                }
            };
            transaction.commit()?;
            Ok(outcome)
        })
    }

    pub fn claim_cancel_requested(
        &self,
        claim: &ClaimedWork,
        observed_at_ms: i64,
    ) -> Result<bool, RepositoryError> {
        if observed_at_ms < 0 {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let reader = self.store.open_reader()?;
        let state: Option<String> = reader
            .connection()
            .query_row(
                "SELECT work_items.state
                 FROM work_items
                 JOIN work_attempts
                   ON work_attempts.id = ?2
                  AND work_attempts.work_item_id = work_items.id
                 WHERE work_items.id = ?1
                   AND work_items.current_attempt_no = ?3
                   AND work_items.fencing_token = ?4
                   AND work_items.lease_owner = ?5
                   AND work_items.lease_boot_id = ?6
                   AND work_items.lease_expires_at_ms >= ?7
                   AND work_attempts.attempt_no = ?3
                   AND work_attempts.fencing_token = ?4
                   AND work_attempts.worker_id = ?5
                   AND work_attempts.worker_boot_id = ?6
                   AND work_attempts.lease_expires_at_ms >= ?7
                   AND work_attempts.state IN ('claimed', 'starting', 'running', 'committing')",
                params![
                    claim.work_item_id.as_str(),
                    claim.attempt_id.as_str(),
                    claim.attempt_no,
                    claim.fencing_token,
                    claim.worker_id.as_str(),
                    claim.worker_boot_id.as_str(),
                    observed_at_ms
                ],
                |row| row.get(0),
            )
            .optional()?;
        let Some(state) = state else {
            return Err(RepositoryError::StaleFence);
        };
        Ok(WorkItemState::from_str(&state)? == WorkItemState::CancelRequested)
    }

    pub fn requeue_claim(
        &self,
        claim: &ClaimedWork,
        completed_at_ms: i64,
        available_at_ms: i64,
    ) -> Result<(), RepositoryError> {
        self.retry_claim(
            claim,
            completed_at_ms,
            available_at_ms,
            "RETRY_REQUESTED",
            None,
        )
        .map(|_| ())
    }

    pub fn retry_claim(
        &self,
        claim: &ClaimedWork,
        completed_at_ms: i64,
        available_at_ms: i64,
        retry_error_code: impl Into<String>,
        terminal_result_json: Option<String>,
    ) -> Result<RetryDisposition, RepositoryError> {
        if completed_at_ms < 0 || available_at_ms < completed_at_ms {
            return Err(RepositoryError::LeaseInvalid);
        }
        let retry_error_code = retry_error_code.into();
        if !is_runtime_error_code(&retry_error_code) {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let claim = claim.clone();
        let terminal_spec = ClaimCompletionSpec::failed(
            claim.clone(),
            completed_at_ms,
            "RETRY_BUDGET_EXHAUSTED",
            terminal_result_json.clone(),
        );
        let cancel_spec = ClaimCompletionSpec::cancelled(
            claim.clone(),
            completed_at_ms,
            Some("CANCEL_REQUESTED".to_owned()),
            terminal_result_json,
        );
        terminal_spec.validate()?;
        cancel_spec.validate()?;
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let current = load_current_claim(&transaction, &claim, completed_at_ms)?;
            ensure_no_active_process(&transaction, &claim)?;
            let disposition = retry_claim_in_transaction(
                &transaction,
                &current,
                &claim,
                completed_at_ms,
                available_at_ms,
                &retry_error_code,
                &terminal_spec,
                &cancel_spec,
            )?;
            transaction.commit()?;
            Ok(disposition)
        })
    }

    pub fn load_claimed_replica_repair_v2(
        &self,
        claim: &ClaimedWork,
        observed_at_ms: i64,
    ) -> Result<ClaimedReplicaRepairV2, RepositoryError> {
        let claim = claim.clone();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let current = load_current_claim(&transaction, &claim, observed_at_ms)?;
            if current.kind != "operation"
                || !matches!(
                    (current.work_state, current.attempt_state),
                    (WorkItemState::Leased, WorkAttemptState::Claimed)
                        | (WorkItemState::Starting, WorkAttemptState::Starting)
                        | (WorkItemState::Running, WorkAttemptState::Running)
                )
            {
                return Err(RepositoryError::StaleFence);
            }
            let operation_run_id = current
                .operation_run_id
                .as_deref()
                .ok_or(RepositoryError::RuntimeStateInvalid)?;
            let operation: Option<(String, String, String, String, String)> = transaction
                .query_row(
                    "SELECT operation_kind, subject_type, subject_id, request_json, state
                     FROM operation_runs
                     WHERE id = ?1",
                    [operation_run_id],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                        ))
                    },
                )
                .optional()?;
            let Some((kind, subject_type, subject_id, request_json, operation_state)) = operation
            else {
                return Err(RepositoryError::NotFound);
            };
            if kind != "replica_repair"
                || subject_type != "recovery_point"
                || !matches!(operation_state.as_str(), "queued" | "running")
            {
                return Err(RepositoryError::RuntimeStateInvalid);
            }
            let request = parse_replica_repair_request(&request_json, &subject_id)?;
            if operation_run_id != replica_repair_operation_run_id(&request).as_str() {
                return Err(RepositoryError::RuntimeStateInvalid);
            }
            let recovery_point_id = request.recovery_point_id;
            let source_destination_fingerprint = request.source_destination_fingerprint;
            let target_destination_fingerprint = request.target_destination_fingerprint;
            type RepairSourceTruth = (Option<String>, Option<String>, Option<String>, i64);
            let source_truth: RepairSourceTruth = transaction.query_row(
                "SELECT MIN(points.signing_key_id),
                        MIN(points.set_manifest_checksum_blake3),
                        MIN(manifest.object_key),
                        COUNT(DISTINCT manifest.object_key)
                 FROM catalog_recovery_points AS points
                 JOIN catalog_artifact_replicas AS source
                   ON source.recovery_point_id = points.id
                 JOIN catalog_stored_objects AS manifest
                   ON manifest.artifact_replica_id = source.id
                  AND manifest.object_role = 'set_manifest'
                  AND manifest.state = 'present'
                 WHERE points.id = ?1
                   AND points.state = 'complete'
                   AND points.signature_status = 'valid'
                   AND source.destination_fingerprint = ?2
                   AND source.state = 'committed'
                   AND source.reader_status = 'eligible'
                   AND NOT EXISTS (
                       SELECT 1
                       FROM catalog_artifact_parts AS parts
                       WHERE parts.recovery_point_id = source.recovery_point_id
                         AND parts.required = 1
                         AND NOT EXISTS (
                             SELECT 1
                             FROM catalog_stored_objects AS payload
                             WHERE payload.artifact_replica_id = source.id
                               AND payload.artifact_part_id = parts.id
                               AND payload.object_role = 'payload'
                               AND payload.state = 'present'
                         )
                   )",
                params![recovery_point_id.as_str(), source_destination_fingerprint],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
            let target_valid: bool = transaction.query_row(
                "SELECT EXISTS(
                     SELECT 1
                     FROM catalog_artifact_replicas
                     WHERE recovery_point_id = ?1
                       AND destination_fingerprint = ?2
                       AND state IN ('degraded', 'missing', 'corrupt', 'quarantined')
                 )",
                params![recovery_point_id.as_str(), target_destination_fingerprint],
                |row| row.get(0),
            )?;
            let (
                Some(signing_key_id),
                Some(source_manifest_checksum_blake3),
                Some(source_manifest_object_key),
                1,
            ) = source_truth
            else {
                return Err(RepositoryError::RuntimeStateInvalid);
            };
            if !target_valid
                || signing_key_id.trim().is_empty()
                || signing_key_id.len() > 256
                || source_manifest_checksum_blake3.len() != 64
                || !source_manifest_checksum_blake3
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                || source_manifest_object_key.trim().is_empty()
                || source_manifest_object_key.len() > 4 * 1024
            {
                return Err(RepositoryError::RuntimeStateInvalid);
            }
            transaction.commit()?;
            Ok(ClaimedReplicaRepairV2 {
                recovery_point_id,
                source_destination_fingerprint,
                target_destination_fingerprint,
                source_reader_secret_ref_id: request.source_reader_secret_ref_id,
                target_writer_secret_ref_id: request.target_writer_secret_ref_id,
                target_reader_secret_ref_id: request.target_reader_secret_ref_id,
                source_manifest_object_key,
                source_manifest_checksum_blake3,
                signing_key_id,
            })
        })
    }

    pub fn load_claimed_retention_delete_v2(
        &self,
        claim: &ClaimedWork,
        observed_at_ms: i64,
    ) -> Result<ClaimedRetentionDeleteV2, RepositoryError> {
        let claim = claim.clone();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let current = load_current_claim(&transaction, &claim, observed_at_ms)?;
            if current.kind != "operation"
                || !matches!(
                    (current.work_state, current.attempt_state),
                    (WorkItemState::Leased, WorkAttemptState::Claimed)
                        | (WorkItemState::Starting, WorkAttemptState::Starting)
                        | (WorkItemState::Running, WorkAttemptState::Running)
                )
            {
                return Err(RepositoryError::StaleFence);
            }
            let operation_run_id = current
                .operation_run_id
                .as_deref()
                .ok_or(RepositoryError::RuntimeStateInvalid)?;
            let operation: Option<(String, String, String, String, String)> = transaction
                .query_row(
                    "SELECT operation_kind, subject_type, subject_id, request_json, state
                     FROM operation_runs
                     WHERE id = ?1",
                    [operation_run_id],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                        ))
                    },
                )
                .optional()?;
            let Some((kind, subject_type, subject_id, request_json, operation_state)) = operation
            else {
                return Err(RepositoryError::NotFound);
            };
            if kind != "retention_delete"
                || subject_type != "artifact_replica"
                || !matches!(operation_state.as_str(), "queued" | "running")
            {
                return Err(RepositoryError::RuntimeStateInvalid);
            }
            let request: serde_json::Value = serde_json::from_str(&request_json)
                .map_err(|_| RepositoryError::RuntimeStateInvalid)?;
            let schema_version = request
                .get("schema_version")
                .and_then(serde_json::Value::as_u64);
            if schema_version == Some(3) {
                let authorization = validate_automatic_policy_authorization_in_transaction(
                    &transaction,
                    operation_run_id,
                    &subject_id,
                    &request_json,
                    Some(observed_at_ms),
                )?;
                let expected_recovery_point_id =
                    RecoveryPointId::from_str(&subject_id).map_err(RepositoryError::from)?;
                let destination_fingerprint = authorization.destination_fingerprint().to_owned();
                let delete_secret_ref_id = authorization.delete_secret_ref_id().map(str::to_owned);
                let digest = authorization.digest_blake3().to_owned();
                let confirmed = authorization
                    .confirm(
                        &digest,
                        expected_recovery_point_id.as_uuid(),
                        &destination_fingerprint,
                    )
                    .map_err(|_| RepositoryError::RuntimeStateInvalid)?;
                transaction.commit()?;
                return Ok(ClaimedRetentionDeleteV2 {
                    recovery_point_id: expected_recovery_point_id,
                    destination_fingerprint,
                    delete_secret_ref_id,
                    confirmed,
                });
            }
            let authorized_at_ms = request
                .get("authorized_at_ms")
                .and_then(serde_json::Value::as_i64);
            let confirmation_token_id = request
                .get("confirmation_token_id")
                .and_then(serde_json::Value::as_str);
            let destination_fingerprint = request
                .get("destination_fingerprint")
                .and_then(serde_json::Value::as_str);
            let delete_secret_ref_id = request
                .get("delete_secret_ref_id")
                .and_then(serde_json::Value::as_str);
            let dry_run_digest = request
                .get("dry_run_digest_blake3")
                .and_then(serde_json::Value::as_str);
            let confirmation_digest = request
                .get("confirmation_digest_blake3")
                .and_then(serde_json::Value::as_str);
            let request_payload_digest = request
                .get("request_payload_digest_blake3")
                .and_then(serde_json::Value::as_str);
            let recovery_point_id = request
                .get("recovery_point_id")
                .and_then(serde_json::Value::as_str);
            let tombstoned_at_ms = request
                .get("tombstoned_at_ms")
                .and_then(serde_json::Value::as_i64);
            let authorization_value = request
                .get("authorization")
                .ok_or(RepositoryError::RuntimeStateInvalid)?;
            let authorization_json = serde_json::to_string(authorization_value)
                .map_err(|_| RepositoryError::RuntimeStateInvalid)?;
            let authorization = RetentionDeleteAuthorizationV2::from_json(&authorization_json)
                .map_err(|_| RepositoryError::RuntimeStateInvalid)?;
            let confirmation_authorization_value = request
                .get("confirmation_authorization")
                .ok_or(RepositoryError::RuntimeStateInvalid)?;
            let confirmation_authorization_json =
                serde_json::to_string(confirmation_authorization_value)
                    .map_err(|_| RepositoryError::RuntimeStateInvalid)?;
            let confirmation_authorization =
                RetentionDeleteAuthorizationV2::from_json(&confirmation_authorization_json)
                    .map_err(|_| RepositoryError::RuntimeStateInvalid)?;
            let expected_recovery_point_id =
                RecoveryPointId::from_str(&subject_id).map_err(RepositoryError::from)?;
            let expected_recovery_point_uuid = authorization.recovery_point_id();
            if schema_version != Some(2)
                || confirmation_token_id.is_none()
                || destination_fingerprint != Some(authorization.destination_fingerprint())
                || delete_secret_ref_id != authorization.delete_secret_ref_id()
                || dry_run_digest != Some(authorization.digest_blake3())
                || confirmation_digest != Some(confirmation_authorization.digest_blake3())
                || !request_payload_digest.is_some_and(is_hex_digest)
                || recovery_point_id != Some(subject_id.as_str())
                || expected_recovery_point_uuid.to_string() != subject_id
                || tombstoned_at_ms != authorization.tombstoned_at_ms()
                || authorized_at_ms != Some(authorization.evaluated_at_ms())
                || confirmation_authorization.recovery_point_id() != expected_recovery_point_uuid
                || confirmation_authorization.destination_fingerprint()
                    != authorization.destination_fingerprint()
                || !confirmation_authorization
                    .has_same_delete_scope(&authorization)
                    .map_err(|_| RepositoryError::RuntimeStateInvalid)?
            {
                return Err(RepositoryError::RuntimeStateInvalid);
            }
            let confirmation: Option<RetentionConfirmationBindingRow> = transaction
                .query_row(
                    "SELECT tokens.state, tokens.action, tokens.resource_type,
                            tokens.resource_id, tokens.resource_fingerprint,
                            tokens.payload_digest_blake3,
                            ledger.command_name, ledger.payload_digest_blake3,
                            ledger.state, ledger.result_ref_type, ledger.result_ref_id
                     FROM confirmation_tokens AS tokens
                     JOIN ipc_request_ledger AS ledger
                       ON ledger.request_id = tokens.consumed_by_request_id
                     WHERE tokens.id = ?1",
                    [confirmation_token_id],
                    |row| {
                        Ok(RetentionConfirmationBindingRow {
                            token_state: row.get(0)?,
                            token_action: row.get(1)?,
                            token_resource_type: row.get(2)?,
                            token_resource_id: row.get(3)?,
                            token_resource_fingerprint: row.get(4)?,
                            token_payload_digest: row.get(5)?,
                            ledger_command: row.get(6)?,
                            ledger_payload_digest: row.get(7)?,
                            ledger_state: row.get(8)?,
                            ledger_result_ref_type: row.get(9)?,
                            ledger_result_ref_id: row.get(10)?,
                        })
                    },
                )
                .optional()?;
            let Some(confirmation) = confirmation else {
                return Err(RepositoryError::RuntimeStateInvalid);
            };
            if confirmation.token_state != "consumed"
                || confirmation.token_action != "retention_delete"
                || confirmation.token_resource_type != "artifact_replica"
                || confirmation.token_resource_id != subject_id
                || confirmation.token_resource_fingerprint
                    != authorization.destination_fingerprint()
                || confirmation.token_payload_digest != confirmation_authorization.digest_blake3()
                || confirmation.ledger_command != "retention.delete"
                || confirmation.ledger_payload_digest
                    != request_payload_digest.ok_or(RepositoryError::RuntimeStateInvalid)?
                || confirmation.ledger_state != "succeeded"
                || confirmation.ledger_result_ref_type.as_deref() != Some("work_item")
                || confirmation.ledger_result_ref_id.as_deref() != Some(claim.work_item_id.as_str())
            {
                return Err(RepositoryError::RuntimeStateInvalid);
            }
            let destination_fingerprint = authorization.destination_fingerprint().to_owned();
            let delete_secret_ref_id = authorization.delete_secret_ref_id().map(str::to_owned);
            let digest = authorization.digest_blake3().to_owned();
            let confirmed = authorization
                .confirm(
                    &digest,
                    expected_recovery_point_uuid,
                    &destination_fingerprint,
                )
                .map_err(|_| RepositoryError::RuntimeStateInvalid)?;
            transaction.commit()?;
            Ok(ClaimedRetentionDeleteV2 {
                recovery_point_id: expected_recovery_point_id,
                destination_fingerprint,
                delete_secret_ref_id,
                confirmed,
            })
        })
    }

    pub fn complete_replica_repair_v2(
        &self,
        claim: &ClaimedWork,
        signed: SignedSetManifestV2,
        expected_verifying_key: VerifyingKey,
        outcome: ReplicaPublishOutcomeV2,
        completed_at_ms: i64,
    ) -> Result<ClaimTerminalOutcome, RepositoryError> {
        if completed_at_ms < 0
            || outcome.result.is_err()
            || outcome.reader_status != ReaderStatusV2::Eligible
            || outcome.attempts == 0
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        signed
            .verify(&expected_verifying_key)
            .map_err(|_| RepositoryError::RuntimeStateInvalid)?;
        let signed_json = signed
            .to_json()
            .map_err(|_| RepositoryError::RuntimeStateInvalid)?;
        let result_json = serde_json::to_string(&serde_json::json!({
            "attempts": outcome.attempts,
            "destination_fingerprint": outcome.destination_fingerprint.as_str(),
            "manifest_committed": outcome.manifest_receipt.is_some(),
            "payload_count": outcome.payload_receipts.len(),
            "schema_version": 1,
        }))
        .map_err(|_| RepositoryError::RuntimeStateInvalid)?;
        let claim = claim.clone();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut current = load_current_claim(&transaction, &claim, completed_at_ms)?;
            ensure_no_active_process(&transaction, &claim)?;
            if current.kind != "operation"
                || !matches!(
                    (current.work_state, current.attempt_state),
                    (WorkItemState::Starting, WorkAttemptState::Starting)
                        | (WorkItemState::Running, WorkAttemptState::Running)
                        | (
                            WorkItemState::CancelRequested,
                            WorkAttemptState::Starting | WorkAttemptState::Running
                        )
                )
            {
                return Err(RepositoryError::StaleFence);
            }
            let binding = validate_replica_repair_operation(&transaction, &current)?;
            if current.work_state == WorkItemState::Starting {
                let next_work =
                    transition_work_item(current.work_state, WorkItemCommand::MarkRunning)?;
                let next_attempt = transition_work_attempt(
                    current.attempt_state,
                    WorkAttemptCommand::MarkRunning,
                )?;
                update_claim_phase(
                    &transaction,
                    &claim,
                    &current,
                    next_work,
                    next_attempt,
                    completed_at_ms,
                )?;
                current.work_state = next_work;
                current.attempt_state = next_attempt;
            }
            if current.work_state == WorkItemState::CancelRequested {
                let completion = ClaimCompletionSpec::cancelled(
                    claim,
                    completed_at_ms,
                    Some("CANCEL_REQUESTED".to_owned()),
                    None,
                );
                completion.validate()?;
                settle_claim_terminal(&transaction, &current, &completion)?;
                transaction.commit()?;
                return Ok(ClaimTerminalOutcome::Cancelled);
            }

            apply_replica_repair_success(
                &transaction,
                &binding,
                &signed,
                &signed_json,
                &outcome,
                completed_at_ms,
            )?;
            let next_work = transition_work_item(current.work_state, WorkItemCommand::BeginCommit)?;
            let next_attempt =
                transition_work_attempt(current.attempt_state, WorkAttemptCommand::BeginCommit)?;
            advance_subject_commit(&transaction, &current, completed_at_ms)?;
            update_claim_phase(
                &transaction,
                &claim,
                &current,
                next_work,
                next_attempt,
                completed_at_ms,
            )?;
            current.work_state = next_work;
            current.attempt_state = next_attempt;
            let completion =
                ClaimCompletionSpec::succeeded(claim, completed_at_ms, Some(result_json));
            completion.validate()?;
            settle_claim_terminal(&transaction, &current, &completion)?;
            transaction.commit()?;
            Ok(ClaimTerminalOutcome::Succeeded)
        })
    }

    pub fn settle_retention_delete(
        &self,
        claim: &ClaimedWork,
        spec: RetentionDeleteSettlementSpec,
    ) -> Result<RetentionDeleteSettlementOutcome, RepositoryError> {
        spec.validate()?;
        let result_json = retention_delete_result_json(&spec.report)?;
        if result_json.len() > 1024 * 1024 {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let claim = claim.clone();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut current = load_current_claim(&transaction, &claim, spec.completed_at_ms)?;
            ensure_no_active_process(&transaction, &claim)?;
            if current.kind != "operation"
                || !matches!(
                    (current.work_state, current.attempt_state),
                    (WorkItemState::Starting, WorkAttemptState::Starting)
                        | (WorkItemState::Running, WorkAttemptState::Running)
                        | (
                            WorkItemState::CancelRequested,
                            WorkAttemptState::Starting | WorkAttemptState::Running
                        )
                )
            {
                return Err(RepositoryError::StaleFence);
            }
            let operation_run_id =
                validate_retention_delete_operation(&transaction, &current, &spec)?;
            if current.work_state == WorkItemState::Starting {
                let next_work =
                    transition_work_item(current.work_state, WorkItemCommand::MarkRunning)?;
                let next_attempt = transition_work_attempt(
                    current.attempt_state,
                    WorkAttemptCommand::MarkRunning,
                )?;
                update_claim_phase(
                    &transaction,
                    &claim,
                    &current,
                    next_work,
                    next_attempt,
                    spec.completed_at_ms,
                )?;
                current.work_state = next_work;
                current.attempt_state = next_attempt;
            }
            let replica_id = apply_retention_delete_report(&transaction, &spec)?;
            recompute_snapshot_after_retention(
                &transaction,
                &spec.recovery_point_id,
                spec.completed_at_ms,
            )?;

            let deleted_count =
                count_retention_disposition(&spec.report, DeleteObjectDispositionV2::Deleted);
            let retained_count = count_retention_disposition(
                &spec.report,
                DeleteObjectDispositionV2::RetainedByLock,
            );
            let retry_count =
                count_retention_disposition(&spec.report, DeleteObjectDispositionV2::RetryRequired);
            let terminal_failure_count = count_retention_disposition(
                &spec.report,
                DeleteObjectDispositionV2::FailedTerminal,
            );
            let blocked_count = count_retention_disposition(
                &spec.report,
                DeleteObjectDispositionV2::NotAttemptedMarkerBlocked,
            );
            let incomplete =
                retained_count + retry_count + terminal_failure_count + blocked_count > 0;
            record_retention_evidence(
                &transaction,
                &claim,
                &operation_run_id,
                &replica_id,
                &result_json,
                incomplete,
                retry_count > 0,
                spec.completed_at_ms,
            )?;

            if retry_count > 0 {
                let updated = transaction.execute(
                    "UPDATE operation_runs
                     SET result_json = ?2, error_code = 'RETENTION_DELETE_RETRY_REQUIRED',
                         updated_at_ms = ?3
                     WHERE id = ?1 AND state = 'running'",
                    params![operation_run_id, result_json, spec.completed_at_ms],
                )?;
                if updated != 1 {
                    return Err(RepositoryError::StaleFence);
                }
                let terminal_spec = ClaimCompletionSpec::failed(
                    claim.clone(),
                    spec.completed_at_ms,
                    "RETRY_BUDGET_EXHAUSTED",
                    Some(result_json),
                );
                let cancel_spec = ClaimCompletionSpec::cancelled(
                    claim.clone(),
                    spec.completed_at_ms,
                    Some("CANCEL_REQUESTED".to_owned()),
                    None,
                );
                terminal_spec.validate()?;
                cancel_spec.validate()?;
                let disposition = retry_claim_in_transaction(
                    &transaction,
                    &current,
                    &claim,
                    spec.completed_at_ms,
                    spec.retry_available_at_ms,
                    "RETENTION_DELETE_RETRY_REQUIRED",
                    &terminal_spec,
                    &cancel_spec,
                )?;
                transaction.commit()?;
                return Ok(RetentionDeleteSettlementOutcome::Retry(disposition));
            }

            let (outcome, completion) =
                if terminal_failure_count > 0 && deleted_count == 0 && retained_count == 0 {
                    (
                        ClaimTerminalOutcome::Failed,
                        ClaimCompletionSpec::failed(
                            claim.clone(),
                            spec.completed_at_ms,
                            "RETENTION_DELETE_FAILED",
                            Some(result_json),
                        ),
                    )
                } else if incomplete {
                    (
                        ClaimTerminalOutcome::PartiallySucceeded,
                        ClaimCompletionSpec::partially_succeeded(
                            claim.clone(),
                            spec.completed_at_ms,
                            Some(result_json),
                            Some("RETENTION_DELETE_INCOMPLETE".to_owned()),
                        ),
                    )
                } else {
                    (
                        ClaimTerminalOutcome::Succeeded,
                        ClaimCompletionSpec::succeeded(
                            claim.clone(),
                            spec.completed_at_ms,
                            Some(result_json),
                        ),
                    )
                };
            completion.validate()?;
            let effective_outcome = if current.work_state == WorkItemState::CancelRequested {
                let cancelled = ClaimCompletionSpec::cancelled(
                    claim,
                    spec.completed_at_ms,
                    Some("CANCEL_REQUESTED".to_owned()),
                    None,
                );
                cancelled.validate()?;
                settle_claim_terminal(&transaction, &current, &cancelled)?;
                ClaimTerminalOutcome::Cancelled
            } else {
                if matches!(
                    outcome,
                    ClaimTerminalOutcome::Succeeded | ClaimTerminalOutcome::PartiallySucceeded
                ) {
                    let next_work =
                        transition_work_item(current.work_state, WorkItemCommand::BeginCommit)?;
                    let next_attempt = transition_work_attempt(
                        current.attempt_state,
                        WorkAttemptCommand::BeginCommit,
                    )?;
                    advance_subject_commit(&transaction, &current, spec.completed_at_ms)?;
                    update_claim_phase(
                        &transaction,
                        &claim,
                        &current,
                        next_work,
                        next_attempt,
                        spec.completed_at_ms,
                    )?;
                    current.work_state = next_work;
                    current.attempt_state = next_attempt;
                }
                settle_claim_terminal(&transaction, &current, &completion)?;
                outcome
            };
            transaction.commit()?;
            Ok(RetentionDeleteSettlementOutcome::Terminal(
                effective_outcome,
            ))
        })
    }

    pub fn settle_catalog_rebuild_v2(
        &self,
        claim: &ClaimedWork,
        plan: CatalogRebuildPlanV2,
        mode: CatalogRebuildApplyModeV2,
        source_locator: &str,
        completed_at_ms: i64,
    ) -> Result<usize, RepositoryError> {
        plan.validate_for_apply(mode)?;
        if source_locator.trim().is_empty()
            || source_locator.len() > 2_048
            || source_locator.contains(['\0', '\r', '\n'])
            || completed_at_ms < 0
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let accepted_count = plan.accepted_count();
        let rejected_count = plan.rejected_count();
        let unknown_key_count = plan.unknown_key_count();
        let candidates = plan.candidates().to_vec();
        let accepted_specs = plan.accepted_specs;
        let result_json = catalog_rebuild_result_json(
            accepted_count,
            rejected_count,
            unknown_key_count,
            source_locator,
            mode,
        )?;
        let applied_count = if mode == CatalogRebuildApplyModeV2::DryRun {
            0
        } else {
            accepted_specs.len()
        };
        let claim = claim.clone();
        let source_locator = source_locator.to_owned();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut current = load_current_claim(&transaction, &claim, completed_at_ms)?;
            ensure_no_active_process(&transaction, &claim)?;
            if current.kind != "catalog_rebuild"
                || !matches!(
                    (current.work_state, current.attempt_state),
                    (WorkItemState::Starting, WorkAttemptState::Starting)
                        | (WorkItemState::Running, WorkAttemptState::Running)
                        | (
                            WorkItemState::CancelRequested,
                            WorkAttemptState::Starting | WorkAttemptState::Running
                        )
                )
            {
                return Err(RepositoryError::StaleFence);
            }
            let (rebuild_run_id, source_kind) =
                validate_catalog_rebuild_run(&transaction, &current, mode)?;
            if current.work_state == WorkItemState::Starting {
                let next_work =
                    transition_work_item(current.work_state, WorkItemCommand::MarkRunning)?;
                let next_attempt = transition_work_attempt(
                    current.attempt_state,
                    WorkAttemptCommand::MarkRunning,
                )?;
                update_claim_phase(
                    &transaction,
                    &claim,
                    &current,
                    next_work,
                    next_attempt,
                    completed_at_ms,
                )?;
                current.work_state = next_work;
                current.attempt_state = next_attempt;
            }
            if current.work_state == WorkItemState::CancelRequested {
                let cancelled = ClaimCompletionSpec::cancelled(
                    claim,
                    completed_at_ms,
                    Some("CANCEL_REQUESTED".to_owned()),
                    None,
                );
                cancelled.validate()?;
                settle_claim_terminal(&transaction, &current, &cancelled)?;
                transaction.commit()?;
                return Ok(0);
            }

            if mode == CatalogRebuildApplyModeV2::Replace {
                clear_rebuildable_catalog(&transaction)?;
            }
            if mode != CatalogRebuildApplyModeV2::DryRun {
                for spec in &accepted_specs {
                    ingest_complete_projection(&transaction, spec)?;
                }
                rebuild_verification_projections_from_runs(&transaction, completed_at_ms)?;
            }
            record_catalog_rebuild_candidates(
                &transaction,
                &rebuild_run_id,
                &source_kind,
                &source_locator,
                &candidates,
                mode,
                completed_at_ms,
            )?;

            let next_work = transition_work_item(current.work_state, WorkItemCommand::BeginCommit)?;
            let next_attempt =
                transition_work_attempt(current.attempt_state, WorkAttemptCommand::BeginCommit)?;
            advance_subject_commit(&transaction, &current, completed_at_ms)?;
            update_claim_phase(
                &transaction,
                &claim,
                &current,
                next_work,
                next_attempt,
                completed_at_ms,
            )?;
            current.work_state = next_work;
            current.attempt_state = next_attempt;
            let completion =
                ClaimCompletionSpec::succeeded(claim, completed_at_ms, Some(result_json));
            completion.validate()?;
            settle_claim_terminal(&transaction, &current, &completion)?;
            transaction.commit()?;
            Ok(applied_count)
        })
    }

    pub fn expired_claims(
        &self,
        now_ms: i64,
        limit: usize,
    ) -> Result<Vec<ExpiredClaim>, RepositoryError> {
        if now_ms < 0 || limit == 0 || limit > 1_000 {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let reader = self.store.open_reader()?;
        let mut statement = reader.connection().prepare(
            "SELECT
                 work_items.id,
                 work_items.kind,
                 work_attempts.id,
                 work_items.current_attempt_no,
                 work_items.fencing_token,
                 work_items.lease_owner,
                 work_items.lease_boot_id,
                 work_items.state,
                 work_attempts.state,
                 MIN(work_items.lease_expires_at_ms, work_attempts.lease_expires_at_ms),
                 process_instances.id,
                 process_instances.process_id,
                 process_instances.process_start_time_ms,
                 process_instances.service_boot_id,
                 process_instances.job_object_key,
                 process_instances.executable_path,
                 process_instances.executable_sha256,
                 process_instances.state,
                 CASE
                     WHEN work_items.kind = 'restore'
                      AND json_extract(restore_runs.validation_result_json, '$.phase') = 'destructive_pending'
                      AND json_extract(restore_runs.validation_result_json, '$.attemptId') = work_attempts.id
                      AND json_extract(restore_runs.validation_result_json, '$.fencingToken') = work_items.fencing_token
                     THEN 1 ELSE 0
                 END
             FROM work_items
             JOIN work_attempts
               ON work_attempts.id = (
                   SELECT current_attempt.id
                   FROM work_attempts AS current_attempt
                   WHERE current_attempt.work_item_id = work_items.id
                     AND current_attempt.attempt_no = work_items.current_attempt_no
                     AND current_attempt.fencing_token = work_items.fencing_token
                   LIMIT 1
               )
             LEFT JOIN process_instances
               ON process_instances.id = (
                   SELECT latest_process.id
                   FROM process_instances AS latest_process
                   WHERE latest_process.work_attempt_id = work_attempts.id
                   ORDER BY latest_process.started_at_ms DESC, latest_process.id DESC
                   LIMIT 1
               )
             LEFT JOIN restore_runs
               ON restore_runs.id = work_items.restore_run_id
             WHERE work_items.state IN (
                 'leased', 'starting', 'running', 'committing', 'cancel_requested'
             )
               AND work_items.lease_expires_at_ms < ?1
               AND work_attempts.lease_expires_at_ms < ?1
             ORDER BY work_items.lease_expires_at_ms, work_items.id
             LIMIT ?2",
        )?;
        let rows = statement.query_map(params![now_ms, limit as i64], |row| {
            Ok(ExpiredClaimSqlRow {
                work_item_id: row.get(0)?,
                kind: row.get(1)?,
                attempt_id: row.get(2)?,
                attempt_no: row.get(3)?,
                fencing_token: row.get(4)?,
                worker_id: row.get(5)?,
                worker_boot_id: row.get(6)?,
                work_state: row.get(7)?,
                attempt_state: row.get(8)?,
                lease_expires_at_ms: row.get(9)?,
                process_instance_id: row.get(10)?,
                process_id: row.get(11)?,
                process_start_time_ms: row.get(12)?,
                process_boot_id: row.get(13)?,
                job_object_key: row.get(14)?,
                executable_path: row.get(15)?,
                executable_sha256: row.get(16)?,
                process_state: row.get(17)?,
                restore_destructive_step_started: row.get::<_, i64>(18)? == 1,
            })
        })?;
        let mut candidates = Vec::new();
        for row in rows {
            candidates.push(row?.try_into()?);
        }
        Ok(candidates)
    }

    pub fn active_spool_paths(&self, limit: usize) -> Result<Vec<String>, RepositoryError> {
        if limit == 0 || limit > 10_000 {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let reader = self.store.open_reader()?;
        let mut statement = reader.connection().prepare(
            "SELECT spool_path
             FROM spool_reservations
             WHERE released_at_ms IS NULL
               AND state IN ('reserved', 'materializing', 'ready', 'quarantined')
             ORDER BY acquired_at_ms, id
             LIMIT ?1",
        )?;
        let rows = statement.query_map(params![limit as i64], |row| row.get(0))?;
        let mut paths = Vec::new();
        for row in rows {
            paths.push(row?);
        }
        Ok(paths)
    }

    pub fn reconcile_expired_without_process(
        &self,
        candidate: &ExpiredClaim,
        reconciled_at_ms: i64,
        available_at_ms: i64,
    ) -> Result<(), RepositoryError> {
        if candidate.process.is_some() {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        self.reconcile_expired_claim(candidate, reconciled_at_ms, available_at_ms, false)
    }

    pub fn reconcile_expired_process_gone(
        &self,
        candidate: &ExpiredClaim,
        reconciled_at_ms: i64,
        available_at_ms: i64,
    ) -> Result<(), RepositoryError> {
        if candidate.process.is_none() {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        self.reconcile_expired_claim(candidate, reconciled_at_ms, available_at_ms, true)
    }

    pub fn reconcile_expired_backup_commit(
        &self,
        candidate: &ExpiredClaim,
        spec: CompleteRecoveryPointSpec,
        reconciled_at_ms: i64,
        process_gone_confirmed: bool,
    ) -> Result<(), RepositoryError> {
        spec.validate()?;
        let plaintext_bytes = spec.plaintext_bytes()?;
        if candidate.claim.kind != WorkItemKind::Backup
            || reconciled_at_ms <= candidate.lease_expires_at_ms
            || (candidate.process.is_some() && !process_gone_confirmed)
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let candidate = candidate.clone();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let current: Option<ExpiredBackupCommitRow> = transaction
                .query_row(
                    "SELECT
                         work_items.state,
                         work_items.fencing_token,
                         work_items.lease_expires_at_ms,
                         work_attempts.state,
                         work_attempts.lease_expires_at_ms,
                         work_items.kind,
                         work_items.backup_run_id,
                         work_items.operation_run_id
                     FROM work_items
                     JOIN work_attempts
                       ON work_attempts.id = ?2
                      AND work_attempts.work_item_id = work_items.id
                     WHERE work_items.id = ?1
                       AND work_items.current_attempt_no = ?3
                       AND work_items.fencing_token = ?4
                       AND work_items.lease_owner = ?5
                       AND work_items.lease_boot_id = ?6
                       AND work_attempts.attempt_no = ?3
                       AND work_attempts.fencing_token = ?4
                       AND work_attempts.worker_id = ?5
                       AND work_attempts.worker_boot_id = ?6",
                    params![
                        candidate.claim.work_item_id.as_str(),
                        candidate.claim.attempt_id.as_str(),
                        candidate.claim.attempt_no,
                        candidate.claim.fencing_token,
                        candidate.claim.worker_id.as_str(),
                        candidate.claim.worker_boot_id.as_str()
                    ],
                    |row| {
                        Ok(ExpiredBackupCommitRow {
                            work_state: row.get(0)?,
                            fencing_token: row.get(1)?,
                            work_lease: row.get(2)?,
                            attempt_state: row.get(3)?,
                            attempt_lease: row.get(4)?,
                            kind: row.get(5)?,
                            backup_run_id: row.get(6)?,
                            operation_run_id: row.get(7)?,
                        })
                    },
                )
                .optional()?;
            let Some(current) = current else {
                return Err(RepositoryError::StaleFence);
            };
            if current.work_lease >= reconciled_at_ms
                || current.attempt_lease >= reconciled_at_ms
                || current.fencing_token != candidate.claim.fencing_token
                || current.work_state != candidate.work_state.as_str()
                || current.attempt_state != candidate.attempt_state.as_str()
                || current.work_state != WorkItemState::Committing.as_str()
                || current.attempt_state != WorkAttemptState::Committing.as_str()
                || current.kind != WorkItemKind::Backup.as_str()
                || current.operation_run_id.is_some()
            {
                return Err(RepositoryError::StaleFence);
            }
            reconcile_commit_process(
                &transaction,
                &candidate,
                reconciled_at_ms,
                process_gone_confirmed,
            )?;

            let backup_run_id = current
                .backup_run_id
                .ok_or(RepositoryError::RuntimeStateInvalid)?;
            validate_backup_commit_manifest_binding(&transaction, &backup_run_id, &spec)?;

            ingest_complete_projection(&transaction, &spec)?;
            schedule_post_backup_in_transaction(&transaction, spec.id(), reconciled_at_ms)?;
            let linked = transaction.execute(
                "UPDATE backup_runs
                 SET recovery_point_id = ?2, plaintext_bytes = ?3, updated_at_ms = ?4
                 WHERE id = ?1
                   AND state = 'committing'
                   AND (recovery_point_id IS NULL OR recovery_point_id = ?2)",
                params![
                    backup_run_id,
                    spec.id().as_str(),
                    plaintext_bytes,
                    reconciled_at_ms
                ],
            )?;
            if linked != 1 {
                return Err(RepositoryError::StaleFence);
            }
            let current = CurrentClaimRow {
                work_state: WorkItemState::Committing,
                attempt_state: WorkAttemptState::Committing,
                lease_expires_at_ms: current.work_lease.min(current.attempt_lease),
                kind: current.kind,
                backup_run_id: Some(backup_run_id),
                restore_run_id: None,
                verification_run_id: None,
                catalog_rebuild_run_id: None,
                operation_run_id: None,
            };
            let completion =
                ClaimCompletionSpec::succeeded(candidate.claim.clone(), reconciled_at_ms, None);
            completion.validate()?;
            settle_claim_terminal(&transaction, &current, &completion)?;
            transaction.commit()?;
            Ok(())
        })
    }

    fn reconcile_expired_claim(
        &self,
        candidate: &ExpiredClaim,
        reconciled_at_ms: i64,
        available_at_ms: i64,
        expect_recorded_process: bool,
    ) -> Result<(), RepositoryError> {
        if reconciled_at_ms < 0 || available_at_ms < reconciled_at_ms {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let candidate = candidate.clone();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let current: Option<ExpiredClaimReconciliationRow> = transaction
                .query_row(
                    "SELECT
                         work_items.state,
                         work_items.fencing_token,
                         work_items.lease_expires_at_ms,
                         work_attempts.state,
                         work_attempts.lease_expires_at_ms,
                         work_attempts.started_at_ms,
                         work_items.restore_run_id,
                         restore_runs.validation_result_json
                     FROM work_items
                     JOIN work_attempts
                       ON work_attempts.id = ?2
                     AND work_attempts.work_item_id = work_items.id
                     LEFT JOIN restore_runs
                       ON restore_runs.id = work_items.restore_run_id
                     WHERE work_items.id = ?1
                       AND work_items.current_attempt_no = ?3
                       AND work_items.fencing_token = ?4
                       AND work_items.lease_owner = ?5
                       AND work_items.lease_boot_id = ?6
                       AND work_attempts.attempt_no = ?3
                       AND work_attempts.fencing_token = ?4
                       AND work_attempts.worker_id = ?5
                       AND work_attempts.worker_boot_id = ?6",
                    params![
                        candidate.claim.work_item_id.as_str(),
                        candidate.claim.attempt_id.as_str(),
                        candidate.claim.attempt_no,
                        candidate.claim.fencing_token,
                        candidate.claim.worker_id.as_str(),
                        candidate.claim.worker_boot_id.as_str()
                    ],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                            row.get(7)?,
                        ))
                    },
                )
                .optional()?;
            let Some((
                work_state,
                fencing_token,
                work_lease,
                attempt_state,
                attempt_lease,
                started_at_ms,
                restore_run_id,
                restore_marker_json,
            )) = current
            else {
                return Err(RepositoryError::StaleFence);
            };
            if work_lease >= reconciled_at_ms
                || attempt_lease >= reconciled_at_ms
                || work_state != candidate.work_state.as_str()
                || attempt_state != candidate.attempt_state.as_str()
            {
                return Err(RepositoryError::StaleFence);
            }
            let destructive_step_started = restore_marker_json
                .as_deref()
                .is_some_and(|marker| restore_destructive_marker_matches(marker, &candidate));
            if candidate.claim.kind == WorkItemKind::Restore
                && destructive_step_started != candidate.restore_destructive_step_started
            {
                return Err(RepositoryError::StaleFence);
            }
            if expect_recorded_process {
                let process = candidate
                    .process
                    .as_ref()
                    .ok_or(RepositoryError::RuntimeStateInvalid)?;
                let process_row: Option<(i64, i64, String, String, String)> = transaction
                    .query_row(
                        "SELECT
                             process_id, process_start_time_ms, service_boot_id,
                             job_object_key, state
                         FROM process_instances
                         WHERE id = ?1 AND work_attempt_id = ?2",
                        params![
                            process.process_instance_id.as_str(),
                            candidate.claim.attempt_id.as_str()
                        ],
                        |row| {
                            Ok((
                                row.get(0)?,
                                row.get(1)?,
                                row.get(2)?,
                                row.get(3)?,
                                row.get(4)?,
                            ))
                        },
                    )
                    .optional()?;
                let Some((
                    process_id,
                    process_start_time_ms,
                    service_boot_id,
                    job_object_key,
                    process_state,
                )) = process_row
                else {
                    return Err(RepositoryError::StaleFence);
                };
                if process_id != process.process_id
                    || process_start_time_ms != process.process_start_time_ms
                    || service_boot_id != process.service_boot_id.as_str()
                    || job_object_key != process.job_object_key.as_str()
                    || process_state != process.state.as_str()
                {
                    return Err(RepositoryError::StaleFence);
                }
                let other_active: i64 = transaction.query_row(
                    "SELECT COUNT(*)
                     FROM process_instances
                     WHERE work_attempt_id = ?1
                       AND id <> ?2
                       AND state IN ('starting', 'running', 'orphaned')",
                    params![
                        candidate.claim.attempt_id.as_str(),
                        process.process_instance_id.as_str()
                    ],
                    |row| row.get(0),
                )?;
                if other_active != 0 {
                    return Err(RepositoryError::StaleFence);
                }
                if matches!(process_state.as_str(), "starting" | "running" | "orphaned") {
                    let affected = transaction.execute(
                        "UPDATE process_instances
                         SET state = 'killed', ended_at_ms = ?2,
                             termination_reason = 'reconciled_process_gone'
                         WHERE id = ?1
                           AND state = ?3
                           AND started_at_ms <= ?2",
                        params![
                            process.process_instance_id.as_str(),
                            reconciled_at_ms,
                            process_state
                        ],
                    )?;
                    if affected != 1 {
                        return Err(RepositoryError::StaleFence);
                    }
                } else if !matches!(process_state.as_str(), "exited" | "killed") {
                    return Err(RepositoryError::RuntimeStateInvalid);
                }
            } else {
                let process_count: i64 = transaction.query_row(
                    "SELECT COUNT(*) FROM process_instances WHERE work_attempt_id = ?1",
                    [candidate.claim.attempt_id.as_str()],
                    |row| row.get(0),
                )?;
                if process_count != 0 {
                    return Err(RepositoryError::StaleFence);
                }
            }

            let current_work = WorkItemState::from_str(&work_state)?;
            let current_attempt = WorkAttemptState::from_str(&attempt_state)?;
            if candidate.claim.kind == WorkItemKind::Restore && destructive_step_started {
                let restore_run_id = restore_run_id.ok_or(RepositoryError::RuntimeStateInvalid)?;
                let result_json = serde_json::json!({
                    "schemaVersion": 1,
                    "phase": "reconcile",
                    "restore": {
                        "nativeExitCode": null,
                        "targetCreated": true,
                        "evidence": {
                            "targetCreation": "unknown_after_crash"
                        }
                    },
                    "validation": {
                        "status": "not_run",
                        "evidence": {}
                    },
                    "cleanup": {
                        "required": true,
                        "reason": "RESTORE_CRASH_CLEANUP_REQUIRED",
                        "targetPreserved": true
                    }
                })
                .to_string();
                let current = CurrentClaimRow {
                    work_state: current_work,
                    attempt_state: current_attempt,
                    lease_expires_at_ms: work_lease.min(attempt_lease),
                    kind: WorkItemKind::Restore.as_str().to_owned(),
                    backup_run_id: None,
                    restore_run_id: Some(restore_run_id),
                    verification_run_id: None,
                    catalog_rebuild_run_id: None,
                    operation_run_id: None,
                };
                let completion = if current_work == WorkItemState::CancelRequested {
                    ClaimCompletionSpec::cancelled(
                        candidate.claim.clone(),
                        reconciled_at_ms,
                        Some("CANCEL_REQUESTED".to_owned()),
                        Some(result_json),
                    )
                } else {
                    ClaimCompletionSpec::failed(
                        candidate.claim.clone(),
                        reconciled_at_ms,
                        "RESTORE_CRASH_CLEANUP_REQUIRED",
                        Some(result_json),
                    )
                };
                completion.validate()?;
                settle_claim_terminal(&transaction, &current, &completion)?;
                transaction.commit()?;
                return Ok(());
            }
            let queued = transition_work_item(current_work, WorkItemCommand::Release)?;
            let next_work = if available_at_ms > reconciled_at_ms {
                transition_work_item(queued, WorkItemCommand::Delay)?
            } else {
                queued
            };
            let (next_attempt, error_code) = if started_at_ms.is_some() {
                let next = transition_work_attempt(current_attempt, WorkAttemptCommand::Fail)?;
                (next, "RECONCILE_PROCESS_MISSING")
            } else {
                let next = transition_work_attempt(current_attempt, WorkAttemptCommand::Abandon)?;
                (next, "RECONCILE_CLAIM_ABANDONED")
            };
            let next_fence = fencing_token
                .checked_add(1)
                .ok_or(RepositoryError::RuntimeStateInvalid)?;
            let item_affected = transaction.execute(
                "UPDATE work_items
                 SET state = ?7, available_at_ms = ?8, fencing_token = ?9,
                     lease_owner = NULL, lease_boot_id = NULL,
                     lease_expires_at_ms = NULL, heartbeat_at_ms = NULL,
                     wait_reason = 'reconciled', updated_at_ms = ?10
                 WHERE id = ?1
                   AND current_attempt_no = ?2
                   AND fencing_token = ?3
                   AND lease_owner = ?4
                   AND lease_boot_id = ?5
                   AND state = ?6",
                params![
                    candidate.claim.work_item_id.as_str(),
                    candidate.claim.attempt_no,
                    candidate.claim.fencing_token,
                    candidate.claim.worker_id.as_str(),
                    candidate.claim.worker_boot_id.as_str(),
                    work_state,
                    next_work.as_str(),
                    available_at_ms,
                    next_fence,
                    reconciled_at_ms
                ],
            )?;
            let attempt_affected = transaction.execute(
                "UPDATE work_attempts
                 SET state = ?7, completed_at_ms = ?8, retry_class = 'reconcile',
                     retry_after_ms = ?9, error_code = ?10
                 WHERE id = ?1
                   AND work_item_id = ?2
                   AND attempt_no = ?3
                   AND fencing_token = ?4
                   AND worker_id = ?5
                   AND worker_boot_id = ?6
                   AND state = ?11",
                params![
                    candidate.claim.attempt_id.as_str(),
                    candidate.claim.work_item_id.as_str(),
                    candidate.claim.attempt_no,
                    candidate.claim.fencing_token,
                    candidate.claim.worker_id.as_str(),
                    candidate.claim.worker_boot_id.as_str(),
                    next_attempt.as_str(),
                    reconciled_at_ms,
                    available_at_ms,
                    error_code,
                    attempt_state
                ],
            )?;
            if item_affected != 1 || attempt_affected != 1 {
                return Err(RepositoryError::StaleFence);
            }
            release_claim_capacity(
                &transaction,
                &candidate.claim,
                reconciled_at_ms,
                "reconciled",
                false,
            )?;
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn occurrence_count(&self) -> Result<i64, RepositoryError> {
        let reader = self.store.open_reader()?;
        reader
            .connection()
            .query_row("SELECT COUNT(*) FROM plan_occurrences", [], |row| {
                row.get(0)
            })
            .map_err(RepositoryError::from)
    }

    pub fn attempt_count(&self, work_item_id: &WorkItemId) -> Result<i64, RepositoryError> {
        let reader = self.store.open_reader()?;
        reader
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM work_attempts WHERE work_item_id = ?1",
                [work_item_id.as_str()],
                |row| row.get(0),
            )
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

fn validate_backup_commit_manifest_binding(
    transaction: &Transaction<'_>,
    backup_run_id: &str,
    spec: &CompleteRecoveryPointSpec,
) -> Result<(), RepositoryError> {
    let row = transaction
        .query_row(
            "SELECT backup.plan_occurrence_id, backup.engine, backup.state,
                    backup.recovery_point_id, backup.database_target_id,
                    backup.source_fingerprint, run_scope.database_target_id,
                    run_scope.backup_scope_id, run_scope.scope_mode,
                    run_scope.source_fingerprint, run_scope.resolved_object_count,
                    run_scope.scope_snapshot_json,
                    run_scope.scope_snapshot_hash_blake3
             FROM backup_runs AS backup
             JOIN backup_run_scopes AS run_scope
               ON run_scope.backup_run_id = backup.id
             WHERE backup.id = ?1",
            [backup_run_id],
            |row| {
                Ok(BackupCommitManifestBindingRow {
                    occurrence_id: row.get(0)?,
                    engine: row.get(1)?,
                    backup_state: row.get(2)?,
                    recovery_point_id: row.get(3)?,
                    backup_database_target_id: row.get(4)?,
                    backup_source_fingerprint: row.get(5)?,
                    scope_database_target_id: row.get(6)?,
                    backup_scope_id: row.get(7)?,
                    scope_mode: row.get(8)?,
                    scope_source_fingerprint: row.get(9)?,
                    resolved_object_count: row.get(10)?,
                    scope_snapshot_json: row.get(11)?,
                    scope_snapshot_hash_blake3: row.get(12)?,
                })
            },
        )
        .optional()?
        .ok_or(RepositoryError::RuntimeStateInvalid)?;
    let snapshot: serde_json::Value = serde_json::from_str(&row.scope_snapshot_json)
        .map_err(|_| RepositoryError::RuntimeStateInvalid)?;
    let snapshot_mode = snapshot
        .get("mode")
        .and_then(serde_json::Value::as_str)
        .ok_or(RepositoryError::RuntimeStateInvalid)?;
    let snapshot_objects = snapshot
        .get("objects")
        .and_then(serde_json::Value::as_array)
        .ok_or(RepositoryError::RuntimeStateInvalid)?;
    let snapshot_object_count =
        i64::try_from(snapshot_objects.len()).map_err(|_| RepositoryError::RuntimeStateInvalid)?;
    if row.backup_state != BackupRunState::Committing.as_str()
        || row.backup_database_target_id != row.scope_database_target_id
        || row.backup_source_fingerprint != row.scope_source_fingerprint
        || snapshot_mode != row.scope_mode
        || snapshot_object_count != row.resolved_object_count
        || blake3::hash(row.scope_snapshot_json.as_bytes())
            .to_hex()
            .as_str()
            != row.scope_snapshot_hash_blake3
        || !spec.matches_backup_origin(&row.occurrence_id, backup_run_id, &row.engine)
        || !spec.matches_backup_scope(
            &row.backup_scope_id,
            &row.scope_source_fingerprint,
            &row.scope_mode,
            &row.scope_snapshot_hash_blake3,
            row.resolved_object_count,
        )
        || row
            .recovery_point_id
            .as_deref()
            .is_some_and(|value| value != spec.id().as_str())
    {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    Ok(())
}

fn reconcile_commit_process(
    transaction: &Transaction<'_>,
    candidate: &ExpiredClaim,
    reconciled_at_ms: i64,
    process_gone_confirmed: bool,
) -> Result<(), RepositoryError> {
    let Some(process) = candidate.process.as_ref() else {
        let process_count: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM process_instances WHERE work_attempt_id = ?1",
            [candidate.claim.attempt_id.as_str()],
            |row| row.get(0),
        )?;
        return if process_count == 0 {
            Ok(())
        } else {
            Err(RepositoryError::StaleFence)
        };
    };
    if !process_gone_confirmed {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    let process_row: Option<ReconcileProcessRow> = transaction
        .query_row(
            "SELECT
                 process_id, process_start_time_ms, service_boot_id,
                 job_object_key, executable_path, executable_sha256, state
             FROM process_instances
             WHERE id = ?1 AND work_attempt_id = ?2",
            params![
                process.process_instance_id.as_str(),
                candidate.claim.attempt_id.as_str()
            ],
            |row| {
                Ok(ReconcileProcessRow {
                    process_id: row.get(0)?,
                    process_start_time_ms: row.get(1)?,
                    service_boot_id: row.get(2)?,
                    job_object_key: row.get(3)?,
                    executable_path: row.get(4)?,
                    executable_sha256: row.get(5)?,
                    process_state: row.get(6)?,
                })
            },
        )
        .optional()?;
    let Some(ReconcileProcessRow {
        process_id,
        process_start_time_ms,
        service_boot_id,
        job_object_key,
        executable_path,
        executable_sha256,
        process_state,
    }) = process_row
    else {
        return Err(RepositoryError::StaleFence);
    };
    if process_id != process.process_id
        || process_start_time_ms != process.process_start_time_ms
        || service_boot_id != process.service_boot_id
        || job_object_key != process.job_object_key
        || executable_path != process.executable_path
        || executable_sha256 != process.executable_sha256
        || process_state != process.state
    {
        return Err(RepositoryError::StaleFence);
    }
    let other_active: i64 = transaction.query_row(
        "SELECT COUNT(*)
         FROM process_instances
         WHERE work_attempt_id = ?1
           AND id <> ?2
           AND state IN ('starting', 'running', 'orphaned')",
        params![
            candidate.claim.attempt_id.as_str(),
            process.process_instance_id.as_str()
        ],
        |row| row.get(0),
    )?;
    if other_active != 0 {
        return Err(RepositoryError::StaleFence);
    }
    match process_state.as_str() {
        "starting" | "running" | "orphaned" => {
            let affected = transaction.execute(
                "UPDATE process_instances
                 SET state = 'killed', ended_at_ms = ?2,
                     termination_reason = 'reconciled_manifest_commit'
                 WHERE id = ?1
                   AND state = ?3
                   AND started_at_ms <= ?2",
                params![
                    process.process_instance_id.as_str(),
                    reconciled_at_ms,
                    process_state
                ],
            )?;
            if affected != 1 {
                return Err(RepositoryError::StaleFence);
            }
            Ok(())
        }
        "exited" | "killed" => Ok(()),
        _ => Err(RepositoryError::RuntimeStateInvalid),
    }
}

fn ensure_revision_published(
    transaction: &Transaction<'_>,
    revision_id: &str,
) -> Result<(), RepositoryError> {
    let published_at: Option<Option<i64>> = transaction
        .query_row(
            "SELECT published_at_ms
             FROM backup_plan_revisions
             WHERE id = ?1",
            [revision_id],
            |row| row.get(0),
        )
        .optional()?;
    match published_at {
        None => Err(RepositoryError::NotFound),
        Some(None) => Err(RepositoryError::RuntimeStateInvalid),
        Some(Some(_)) => Ok(()),
    }
}

fn select_occurrence_identity(
    transaction: &Transaction<'_>,
    spec: &OccurrenceSpec,
) -> Result<Option<(String, String)>, RepositoryError> {
    match (
        spec.scheduled_for_ms,
        spec.request_idempotency_key.as_deref(),
    ) {
        (Some(scheduled_for_ms), None) => transaction
            .query_row(
                "SELECT id, plan_revision_id
                 FROM plan_occurrences
                 WHERE plan_revision_id = ?1
                   AND scheduled_for_ms = ?2
                   AND origin IN ('scheduled', 'catch_up')",
                params![spec.plan_revision_id.as_str(), scheduled_for_ms],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(RepositoryError::from),
        (None, Some(key)) => transaction
            .query_row(
                "SELECT id, plan_revision_id
                 FROM plan_occurrences
                 WHERE request_idempotency_key = ?1",
                [key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(RepositoryError::from),
        _ => Err(RepositoryError::RuntimeStateInvalid),
    }
}

fn is_operation_kind(value: &str) -> bool {
    matches!(
        value,
        "connection_probe"
            | "connection_discovery"
            | "destination_probe"
            | "replica_repair"
            | "retention_delete"
            | "orphan_cleanup"
            | "tool_rescan"
            | "notification_test"
            | "notification_delivery"
            | "config_import"
            | "config_cutover"
            | "control_store_backup"
            | "control_store_restore"
            | "support_bundle"
            | "service_drain"
    )
}

fn is_resource_type(value: &str) -> bool {
    matches!(
        value,
        "global_job" | "source" | "cpu_encode" | "destination" | "restore" | "database_exclusive"
    )
}

fn is_canonical_uuid(value: &str) -> bool {
    Uuid::parse_str(value)
        .map(|parsed| parsed.to_string() == value)
        .unwrap_or(false)
}

fn is_hex_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_runtime_error_code(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

fn restore_destructive_marker_matches(marker: &str, candidate: &ExpiredClaim) -> bool {
    serde_json::from_str::<serde_json::Value>(marker)
        .ok()
        .is_some_and(|value| {
            value
                .get("schemaVersion")
                .and_then(serde_json::Value::as_i64)
                == Some(1)
                && value.get("phase").and_then(serde_json::Value::as_str)
                    == Some("destructive_pending")
                && value.get("attemptId").and_then(serde_json::Value::as_str)
                    == Some(candidate.claim.attempt_id.as_str())
                && value
                    .get("fencingToken")
                    .and_then(serde_json::Value::as_i64)
                    == Some(candidate.claim.fencing_token)
        })
}

struct ExpiredClaimSqlRow {
    work_item_id: String,
    kind: String,
    attempt_id: String,
    attempt_no: i64,
    fencing_token: i64,
    worker_id: String,
    worker_boot_id: String,
    work_state: String,
    attempt_state: String,
    lease_expires_at_ms: i64,
    process_instance_id: Option<String>,
    process_id: Option<i64>,
    process_start_time_ms: Option<i64>,
    process_boot_id: Option<String>,
    job_object_key: Option<String>,
    executable_path: Option<String>,
    executable_sha256: Option<String>,
    process_state: Option<String>,
    restore_destructive_step_started: bool,
}

struct ReconcileProcessRow {
    process_id: i64,
    process_start_time_ms: i64,
    service_boot_id: String,
    job_object_key: String,
    executable_path: String,
    executable_sha256: Option<String>,
    process_state: String,
}

struct ExpiredBackupCommitRow {
    work_state: String,
    fencing_token: i64,
    work_lease: i64,
    attempt_state: String,
    attempt_lease: i64,
    kind: String,
    backup_run_id: Option<String>,
    operation_run_id: Option<String>,
}

impl TryFrom<ExpiredClaimSqlRow> for ExpiredClaim {
    type Error = RepositoryError;

    fn try_from(row: ExpiredClaimSqlRow) -> Result<Self, Self::Error> {
        let process = match (
            row.process_instance_id,
            row.process_id,
            row.process_start_time_ms,
            row.process_boot_id,
            row.job_object_key,
            row.executable_path,
            row.executable_sha256,
            row.process_state,
        ) {
            (None, None, None, None, None, None, None, None) => None,
            (
                Some(process_instance_id),
                Some(process_id),
                Some(process_start_time_ms),
                Some(service_boot_id),
                Some(job_object_key),
                Some(executable_path),
                executable_sha256,
                Some(state),
            ) if process_id > 0
                && process_start_time_ms >= 0
                && !service_boot_id.is_empty()
                && !job_object_key.is_empty()
                && std::path::Path::new(&executable_path).is_absolute()
                && executable_sha256.as_deref().is_none_or(is_hex_digest) =>
            {
                Some(RecordedProcessIdentity {
                    process_instance_id,
                    process_id,
                    process_start_time_ms,
                    service_boot_id,
                    job_object_key,
                    executable_path,
                    executable_sha256,
                    state,
                })
            }
            _ => return Err(RepositoryError::RuntimeStateInvalid),
        };
        Ok(Self {
            claim: ClaimedWork {
                work_item_id: WorkItemId::from_str(&row.work_item_id)?,
                kind: WorkItemKind::from_str(&row.kind)?,
                attempt_id: WorkAttemptId::from_str(&row.attempt_id)?,
                attempt_no: row.attempt_no,
                fencing_token: row.fencing_token,
                worker_id: row.worker_id,
                worker_boot_id: row.worker_boot_id,
            },
            work_state: WorkItemState::from_str(&row.work_state)?,
            attempt_state: WorkAttemptState::from_str(&row.attempt_state)?,
            lease_expires_at_ms: row.lease_expires_at_ms,
            process,
            restore_destructive_step_started: row.restore_destructive_step_started,
        })
    }
}

struct CurrentClaimRow {
    work_state: WorkItemState,
    attempt_state: WorkAttemptState,
    lease_expires_at_ms: i64,
    kind: String,
    backup_run_id: Option<String>,
    restore_run_id: Option<String>,
    verification_run_id: Option<String>,
    catalog_rebuild_run_id: Option<String>,
    operation_run_id: Option<String>,
}

struct CurrentClaimSqlRow {
    work_state: String,
    work_lease: i64,
    attempt_state: String,
    attempt_lease: i64,
    kind: String,
    backup_run_id: Option<String>,
    restore_run_id: Option<String>,
    verification_run_id: Option<String>,
    catalog_rebuild_run_id: Option<String>,
    operation_run_id: Option<String>,
}

struct RetentionConfirmationBindingRow {
    token_state: String,
    token_action: String,
    token_resource_type: String,
    token_resource_id: String,
    token_resource_fingerprint: String,
    token_payload_digest: String,
    ledger_command: String,
    ledger_payload_digest: String,
    ledger_state: String,
    ledger_result_ref_type: Option<String>,
    ledger_result_ref_id: Option<String>,
}

fn load_current_claim(
    transaction: &Transaction<'_>,
    claim: &ClaimedWork,
    now_ms: i64,
) -> Result<CurrentClaimRow, RepositoryError> {
    let current: Option<CurrentClaimSqlRow> = transaction
        .query_row(
            "SELECT
                 work_items.state, work_items.lease_expires_at_ms,
                 work_attempts.state, work_attempts.lease_expires_at_ms,
                 work_items.kind, work_items.backup_run_id,
                 work_items.restore_run_id, work_items.verification_run_id,
                 work_items.catalog_rebuild_run_id, work_items.operation_run_id
             FROM work_items
             JOIN work_attempts
               ON work_attempts.id = ?2
              AND work_attempts.work_item_id = work_items.id
             WHERE work_items.id = ?1
               AND work_items.current_attempt_no = ?3
               AND work_items.fencing_token = ?4
               AND work_items.lease_owner = ?5
               AND work_items.lease_boot_id = ?6
               AND work_attempts.attempt_no = ?3
               AND work_attempts.fencing_token = ?4
               AND work_attempts.worker_id = ?5
               AND work_attempts.worker_boot_id = ?6",
            params![
                claim.work_item_id.as_str(),
                claim.attempt_id.as_str(),
                claim.attempt_no,
                claim.fencing_token,
                claim.worker_id.as_str(),
                claim.worker_boot_id.as_str()
            ],
            |row| {
                Ok(CurrentClaimSqlRow {
                    work_state: row.get(0)?,
                    work_lease: row.get(1)?,
                    attempt_state: row.get(2)?,
                    attempt_lease: row.get(3)?,
                    kind: row.get(4)?,
                    backup_run_id: row.get(5)?,
                    restore_run_id: row.get(6)?,
                    verification_run_id: row.get(7)?,
                    catalog_rebuild_run_id: row.get(8)?,
                    operation_run_id: row.get(9)?,
                })
            },
        )
        .optional()?;
    let Some(current) = current else {
        return Err(RepositoryError::StaleFence);
    };
    if now_ms < 0 || current.work_lease < now_ms || current.attempt_lease < now_ms {
        return Err(RepositoryError::StaleFence);
    }
    Ok(CurrentClaimRow {
        work_state: WorkItemState::from_str(&current.work_state)?,
        attempt_state: WorkAttemptState::from_str(&current.attempt_state)?,
        lease_expires_at_ms: current.work_lease.min(current.attempt_lease),
        kind: current.kind,
        backup_run_id: current.backup_run_id,
        restore_run_id: current.restore_run_id,
        verification_run_id: current.verification_run_id,
        catalog_rebuild_run_id: current.catalog_rebuild_run_id,
        operation_run_id: current.operation_run_id,
    })
}

fn current_claim_lease(
    transaction: &Transaction<'_>,
    claim: &ClaimedWork,
    now_ms: i64,
) -> Result<i64, RepositoryError> {
    let current = load_current_claim(transaction, claim, now_ms)?;
    ensure_claim_phase(&current, WorkItemState::Leased, WorkAttemptState::Claimed)?;
    Ok(current.lease_expires_at_ms)
}

fn ensure_claim_phase(
    current: &CurrentClaimRow,
    expected_work: WorkItemState,
    expected_attempt: WorkAttemptState,
) -> Result<(), RepositoryError> {
    if current.work_state != expected_work || current.attempt_state != expected_attempt {
        return Err(RepositoryError::StaleFence);
    }
    Ok(())
}

fn update_claim_phase(
    transaction: &Transaction<'_>,
    claim: &ClaimedWork,
    current: &CurrentClaimRow,
    next_work: WorkItemState,
    next_attempt: WorkAttemptState,
    observed_at_ms: i64,
) -> Result<(), RepositoryError> {
    let item_affected = transaction.execute(
        "UPDATE work_items
         SET state = ?7, updated_at_ms = ?8
         WHERE id = ?1
           AND current_attempt_no = ?2
           AND fencing_token = ?3
           AND lease_owner = ?4
           AND lease_boot_id = ?5
           AND state = ?6
           AND lease_expires_at_ms >= ?8",
        params![
            claim.work_item_id.as_str(),
            claim.attempt_no,
            claim.fencing_token,
            claim.worker_id.as_str(),
            claim.worker_boot_id.as_str(),
            current.work_state.as_str(),
            next_work.as_str(),
            observed_at_ms
        ],
    )?;
    let attempt_affected = transaction.execute(
        "UPDATE work_attempts
         SET state = ?7
         WHERE id = ?1
           AND work_item_id = ?2
           AND attempt_no = ?3
           AND fencing_token = ?4
           AND worker_id = ?5
           AND worker_boot_id = ?6
           AND state = ?8
           AND lease_expires_at_ms >= ?9",
        params![
            claim.attempt_id.as_str(),
            claim.work_item_id.as_str(),
            claim.attempt_no,
            claim.fencing_token,
            claim.worker_id.as_str(),
            claim.worker_boot_id.as_str(),
            next_attempt.as_str(),
            current.attempt_state.as_str(),
            observed_at_ms
        ],
    )?;
    if item_affected != 1 || attempt_affected != 1 {
        return Err(RepositoryError::StaleFence);
    }
    Ok(())
}

fn ensure_no_active_process(
    transaction: &Transaction<'_>,
    claim: &ClaimedWork,
) -> Result<(), RepositoryError> {
    let active: i64 = transaction.query_row(
        "SELECT COUNT(*)
         FROM process_instances
         WHERE work_attempt_id = ?1
           AND service_boot_id = ?2
           AND state IN ('starting', 'running', 'orphaned')",
        params![claim.attempt_id.as_str(), claim.worker_boot_id.as_str()],
        |row| row.get(0),
    )?;
    if active != 0 {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    Ok(())
}

fn advance_subject_start(
    transaction: &Transaction<'_>,
    current: &CurrentClaimRow,
    started_at_ms: i64,
) -> Result<(), RepositoryError> {
    match current.kind.as_str() {
        "operation" => {
            let operation_id = current
                .operation_run_id
                .as_deref()
                .ok_or(RepositoryError::RuntimeStateInvalid)?;
            let state: String = transaction
                .query_row(
                    "SELECT state FROM operation_runs WHERE id = ?1",
                    [operation_id],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or(RepositoryError::NotFound)?;
            match state.as_str() {
                "queued" => {
                    let affected = transaction.execute(
                        "UPDATE operation_runs
                         SET state = 'running',
                             started_at_ms = COALESCE(started_at_ms, ?2),
                             updated_at_ms = ?2
                         WHERE id = ?1 AND state = 'queued'",
                        params![operation_id, started_at_ms],
                    )?;
                    if affected != 1 {
                        return Err(RepositoryError::StaleFence);
                    }
                }
                "running" => {}
                _ => return Err(RepositoryError::RuntimeStateInvalid),
            }
        }
        "catalog_rebuild" => {
            let rebuild_id = current
                .catalog_rebuild_run_id
                .as_deref()
                .ok_or(RepositoryError::RuntimeStateInvalid)?;
            let status: String = transaction
                .query_row(
                    "SELECT status FROM catalog_rebuild_runs WHERE id = ?1",
                    [rebuild_id],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or(RepositoryError::NotFound)?;
            match status.as_str() {
                "queued" => {
                    let affected = transaction.execute(
                        "UPDATE catalog_rebuild_runs
                         SET status = 'running',
                             started_at_ms = COALESCE(started_at_ms, ?2)
                         WHERE id = ?1 AND status = 'queued'",
                        params![rebuild_id, started_at_ms],
                    )?;
                    if affected != 1 {
                        return Err(RepositoryError::StaleFence);
                    }
                }
                "running" => {}
                _ => return Err(RepositoryError::RuntimeStateInvalid),
            }
        }
        "restore" => {
            let restore_id = current
                .restore_run_id
                .as_deref()
                .ok_or(RepositoryError::RuntimeStateInvalid)?;
            let (state, purpose): (String, String) = transaction
                .query_row(
                    "SELECT state, purpose FROM restore_runs WHERE id = ?1",
                    [restore_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?
                .ok_or(RepositoryError::NotFound)?;
            match state.as_str() {
                "queued" => {
                    let affected = transaction.execute(
                        "UPDATE restore_runs
                         SET state = 'running',
                             started_at_ms = COALESCE(started_at_ms, ?2),
                             updated_at_ms = ?2
                         WHERE id = ?1 AND state = 'queued'",
                        params![restore_id, started_at_ms],
                    )?;
                    if affected != 1 {
                        return Err(RepositoryError::StaleFence);
                    }
                    if purpose == "restore_drill" {
                        let verification_affected = transaction.execute(
                            "UPDATE verification_runs
                             SET state = 'running',
                                 started_at_ms = COALESCE(started_at_ms, ?2),
                                 updated_at_ms = ?2
                             WHERE restore_run_id = ?1
                               AND trigger_kind = 'restore_drill'
                               AND requested_level = 3
                               AND state = 'queued'",
                            params![restore_id, started_at_ms],
                        )?;
                        if verification_affected != 1 {
                            return Err(RepositoryError::RuntimeStateInvalid);
                        }
                    }
                }
                "running" => {
                    if purpose == "restore_drill" {
                        let verification_state: Option<String> = transaction
                            .query_row(
                                "SELECT state FROM verification_runs
                                 WHERE restore_run_id = ?1
                                   AND trigger_kind = 'restore_drill'
                                   AND requested_level = 3",
                                [restore_id],
                                |row| row.get(0),
                            )
                            .optional()?;
                        if verification_state.as_deref() != Some("running") {
                            return Err(RepositoryError::RuntimeStateInvalid);
                        }
                    }
                }
                _ => return Err(RepositoryError::RuntimeStateInvalid),
            }
        }
        "verification" => {
            let verification_id = current
                .verification_run_id
                .as_deref()
                .ok_or(RepositoryError::RuntimeStateInvalid)?;
            let state: String = transaction
                .query_row(
                    "SELECT state FROM verification_runs WHERE id = ?1",
                    [verification_id],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or(RepositoryError::NotFound)?;
            match state.as_str() {
                "queued" => {
                    let affected = transaction.execute(
                        "UPDATE verification_runs
                         SET state = 'running',
                             started_at_ms = COALESCE(started_at_ms, ?2),
                             updated_at_ms = ?2
                         WHERE id = ?1 AND state = 'queued'",
                        params![verification_id, started_at_ms],
                    )?;
                    if affected != 1 {
                        return Err(RepositoryError::StaleFence);
                    }
                }
                "running" => {}
                _ => return Err(RepositoryError::RuntimeStateInvalid),
            }
        }
        "backup" => {
            let backup_id = current
                .backup_run_id
                .as_deref()
                .ok_or(RepositoryError::RuntimeStateInvalid)?;
            let (state, occurrence_id): (String, String) = transaction
                .query_row(
                    "SELECT state, plan_occurrence_id
                     FROM backup_runs
                     WHERE id = ?1",
                    [backup_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?
                .ok_or(RepositoryError::NotFound)?;
            let backup_state = BackupRunState::from_str(&state)?;
            match backup_state {
                BackupRunState::Queued => {
                    let next = transition_backup_run(backup_state, BackupRunCommand::Start)?;
                    let affected = transaction.execute(
                        "UPDATE backup_runs
                         SET state = ?2, started_at_ms = COALESCE(started_at_ms, ?3),
                             updated_at_ms = ?3
                         WHERE id = ?1 AND state = ?4",
                        params![backup_id, next.as_str(), started_at_ms, state],
                    )?;
                    if affected != 1 {
                        return Err(RepositoryError::StaleFence);
                    }
                }
                BackupRunState::Running => {}
                _ => return Err(RepositoryError::RuntimeStateInvalid),
            }
            let occurrence_state: String = transaction.query_row(
                "SELECT state FROM plan_occurrences WHERE id = ?1",
                [&occurrence_id],
                |row| row.get(0),
            )?;
            let occurrence_state = PlanOccurrenceState::from_str(&occurrence_state)?;
            match occurrence_state {
                PlanOccurrenceState::Enqueued => {
                    let next =
                        transition_plan_occurrence(occurrence_state, OccurrenceCommand::Start)?;
                    let affected = transaction.execute(
                        "UPDATE plan_occurrences
                         SET state = ?2, updated_at_ms = ?3
                         WHERE id = ?1 AND state = 'enqueued'",
                        params![occurrence_id, next.as_str(), started_at_ms],
                    )?;
                    if affected != 1 {
                        return Err(RepositoryError::StaleFence);
                    }
                }
                PlanOccurrenceState::Running => {}
                _ => return Err(RepositoryError::RuntimeStateInvalid),
            }
        }
        _ => return Err(RepositoryError::RuntimeStateInvalid),
    }
    Ok(())
}

fn advance_subject_commit(
    transaction: &Transaction<'_>,
    current: &CurrentClaimRow,
    observed_at_ms: i64,
) -> Result<(), RepositoryError> {
    match current.kind.as_str() {
        "operation" | "catalog_rebuild" | "verification" => Ok(()),
        "restore" => {
            let restore_id = current
                .restore_run_id
                .as_deref()
                .ok_or(RepositoryError::RuntimeStateInvalid)?;
            let state: Option<String> = transaction
                .query_row(
                    "SELECT state FROM restore_runs WHERE id = ?1",
                    [restore_id],
                    |row| row.get(0),
                )
                .optional()?;
            if state.as_deref() != Some("validating") {
                return Err(RepositoryError::RuntimeStateInvalid);
            }
            Ok(())
        }
        "backup" => {
            let backup_id = current
                .backup_run_id
                .as_deref()
                .ok_or(RepositoryError::RuntimeStateInvalid)?;
            let state: String = transaction.query_row(
                "SELECT state FROM backup_runs WHERE id = ?1",
                [backup_id],
                |row| row.get(0),
            )?;
            let current_state = BackupRunState::from_str(&state)?;
            let next = transition_backup_run(current_state, BackupRunCommand::BeginCommit)?;
            let affected = transaction.execute(
                "UPDATE backup_runs
                 SET state = ?2, updated_at_ms = ?3
                 WHERE id = ?1 AND state = ?4",
                params![backup_id, next.as_str(), observed_at_ms, state],
            )?;
            if affected != 1 {
                return Err(RepositoryError::StaleFence);
            }
            Ok(())
        }
        _ => Err(RepositoryError::RuntimeStateInvalid),
    }
}

fn catalog_rebuild_result_json(
    accepted_count: usize,
    rejected_count: usize,
    unknown_key_count: usize,
    source_locator: &str,
    mode: CatalogRebuildApplyModeV2,
) -> Result<String, RepositoryError> {
    serde_json::to_string(&serde_json::json!({
        "accepted_count": accepted_count,
        "mode": catalog_rebuild_apply_mode_str(mode),
        "rejected_count": rejected_count,
        "schema_version": 1,
        "source_locator": source_locator,
        "unknown_key_count": unknown_key_count,
    }))
    .map_err(|_| RepositoryError::RuntimeStateInvalid)
}

const fn catalog_rebuild_apply_mode_str(mode: CatalogRebuildApplyModeV2) -> &'static str {
    match mode {
        CatalogRebuildApplyModeV2::DryRun => "dry_run",
        CatalogRebuildApplyModeV2::Merge => "merge",
        CatalogRebuildApplyModeV2::Replace => "replace_projection",
    }
}

fn validate_catalog_rebuild_run(
    transaction: &Transaction<'_>,
    current: &CurrentClaimRow,
    mode: CatalogRebuildApplyModeV2,
) -> Result<(String, String), RepositoryError> {
    let rebuild_run_id = current
        .catalog_rebuild_run_id
        .as_deref()
        .ok_or(RepositoryError::RuntimeStateInvalid)?;
    let row: Option<(String, String, String)> = transaction
        .query_row(
            "SELECT source_kind, mode, status
             FROM catalog_rebuild_runs
             WHERE id = ?1",
            [rebuild_run_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((source_kind, persisted_mode, status)) = row else {
        return Err(RepositoryError::NotFound);
    };
    if persisted_mode != catalog_rebuild_apply_mode_str(mode) || status != "running" {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    Ok((rebuild_run_id.to_owned(), source_kind))
}

fn clear_rebuildable_catalog(transaction: &Transaction<'_>) -> Result<(), RepositoryError> {
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
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn record_catalog_rebuild_candidates(
    transaction: &Transaction<'_>,
    rebuild_run_id: &str,
    source_kind: &str,
    source_locator: &str,
    candidates: &[CatalogRebuildCandidateV2],
    mode: CatalogRebuildApplyModeV2,
    observed_at_ms: i64,
) -> Result<(), RepositoryError> {
    for candidate in candidates {
        let (mut result, mut diagnostic_code) = catalog_rebuild_candidate_result(candidate)?;
        let mut checksum = candidate.manifest_checksum_blake3();
        let duplicate = if let Some(checksum) = checksum {
            transaction.query_row(
                "SELECT EXISTS(
                     SELECT 1
                     FROM catalog_manifest_ingest
                     WHERE source_kind = ?1
                       AND source_locator = ?2
                       AND manifest_object_key = ?3
                       AND manifest_checksum_blake3 = ?4
                 )",
                params![source_kind, source_locator, candidate.key(), checksum],
                |row| row.get::<_, bool>(0),
            )?
        } else {
            false
        };
        if duplicate {
            result = "duplicate";
            diagnostic_code = Some("CATALOG_REBUILD_DUPLICATE_MANIFEST");
            checksum = None;
        }
        let recovery_point_id = if mode != CatalogRebuildApplyModeV2::DryRun
            && candidate.disposition() == CatalogRebuildDispositionV2::Accepted
        {
            candidate.recovery_point_id()
        } else {
            None
        };
        let identity = format!(
            "{source_kind}|{source_locator}|{}|{}",
            candidate.key(),
            candidate.manifest_checksum_blake3().unwrap_or("unread")
        );
        let ingest_id = stable_runtime_row_id("catalog-rebuild-ingest", rebuild_run_id, &identity);
        transaction.execute(
            "INSERT INTO catalog_manifest_ingest(
                 id, catalog_rebuild_run_id, source_kind, source_locator,
                 manifest_object_key, manifest_checksum_blake3, result,
                 recovery_point_id, diagnostic_code, observed_at_ms, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
            params![
                ingest_id,
                rebuild_run_id,
                source_kind,
                source_locator,
                candidate.key(),
                checksum,
                result,
                recovery_point_id,
                diagnostic_code,
                observed_at_ms
            ],
        )?;
    }
    Ok(())
}

fn catalog_rebuild_candidate_result(
    candidate: &CatalogRebuildCandidateV2,
) -> Result<(&'static str, Option<&'static str>), RepositoryError> {
    match candidate.disposition() {
        CatalogRebuildDispositionV2::Accepted => Ok(("accepted", None)),
        CatalogRebuildDispositionV2::UnknownKey => {
            Ok(("unknown_key", Some("CATALOG_REBUILD_UNKNOWN_SIGNING_KEY")))
        }
        CatalogRebuildDispositionV2::Rejected => match candidate
            .reject_reason()
            .ok_or(RepositoryError::RuntimeStateInvalid)?
        {
            CatalogRebuildRejectReasonV2::ReadFailed => {
                Ok(("read_failed", Some("CATALOG_REBUILD_MANIFEST_READ_FAILED")))
            }
            CatalogRebuildRejectReasonV2::SignatureInvalid => Ok((
                "invalid_signature",
                Some("CATALOG_REBUILD_SIGNATURE_INVALID"),
            )),
            CatalogRebuildRejectReasonV2::DuplicateRecoveryPoint => Ok((
                "duplicate",
                Some("CATALOG_REBUILD_DUPLICATE_RECOVERY_POINT"),
            )),
            CatalogRebuildRejectReasonV2::MalformedEnvelope
            | CatalogRebuildRejectReasonV2::NonCanonicalEnvelope
            | CatalogRebuildRejectReasonV2::ProjectionInvalid => Ok((
                "invalid_schema",
                Some("CATALOG_REBUILD_MANIFEST_SCHEMA_INVALID"),
            )),
            CatalogRebuildRejectReasonV2::DestinationNotDeclared
            | CatalogRebuildRejectReasonV2::ManifestKeyMismatch
            | CatalogRebuildRejectReasonV2::PayloadUnavailable
            | CatalogRebuildRejectReasonV2::PayloadMismatch => {
                Ok(("malformed", Some("CATALOG_REBUILD_EVIDENCE_MISMATCH")))
            }
        },
    }
}

fn catalog_rebuild_terminal_counts(
    result_json: Option<&str>,
) -> Result<(i64, i64), RepositoryError> {
    let Some(result_json) = result_json else {
        return Ok((0, 0));
    };
    let value = serde_json::from_str::<serde_json::Value>(result_json)
        .map_err(|_| RepositoryError::RuntimeStateInvalid)?;
    let accepted = catalog_rebuild_json_count(&value, "accepted_count")?;
    let rejected = catalog_rebuild_json_count(&value, "rejected_count")?;
    let unknown = catalog_rebuild_json_count(&value, "unknown_key_count")?;
    let rejected = rejected
        .checked_add(unknown)
        .ok_or(RepositoryError::RuntimeStateInvalid)?;
    Ok((accepted, rejected))
}

fn catalog_rebuild_json_count(
    value: &serde_json::Value,
    field: &str,
) -> Result<i64, RepositoryError> {
    let Some(value) = value.get(field) else {
        return Ok(0);
    };
    let value = value.as_u64().ok_or(RepositoryError::RuntimeStateInvalid)?;
    i64::try_from(value).map_err(|_| RepositoryError::RuntimeStateInvalid)
}

fn retention_delete_result_json(
    report: &RetentionDeleteReportV2,
) -> Result<String, RepositoryError> {
    let observations = report
        .observations
        .iter()
        .map(|observation| {
            serde_json::json!({
                "diagnostic": observation.diagnostic,
                "disposition": retention_disposition_str(observation.disposition),
                "object_key": observation.object_key,
                "object_version": observation.object_version,
                "role": retention_role_str(observation.role),
            })
        })
        .collect::<Vec<_>>();
    serde_json::to_string(&serde_json::json!({
        "blocked_count": count_retention_disposition(
            report,
            DeleteObjectDispositionV2::NotAttemptedMarkerBlocked
        ),
        "deleted_count": count_retention_disposition(
            report,
            DeleteObjectDispositionV2::Deleted
        ),
        "failed_terminal_count": count_retention_disposition(
            report,
            DeleteObjectDispositionV2::FailedTerminal
        ),
        "observations": observations,
        "retained_by_lock_count": count_retention_disposition(
            report,
            DeleteObjectDispositionV2::RetainedByLock
        ),
        "retry_required_count": count_retention_disposition(
            report,
            DeleteObjectDispositionV2::RetryRequired
        ),
        "schema_version": 1,
    }))
    .map_err(|_| RepositoryError::RuntimeStateInvalid)
}

fn count_retention_disposition(
    report: &RetentionDeleteReportV2,
    disposition: DeleteObjectDispositionV2,
) -> usize {
    report
        .observations
        .iter()
        .filter(|observation| observation.disposition == disposition)
        .count()
}

struct ReplicaRepairRequest {
    schema_version: u64,
    recovery_point_id: RecoveryPointId,
    source_destination_fingerprint: String,
    target_destination_fingerprint: String,
    source_reader_secret_ref_id: Option<SecretRefId>,
    target_writer_secret_ref_id: Option<SecretRefId>,
    target_reader_secret_ref_id: Option<SecretRefId>,
}

fn parse_replica_repair_request(
    request_json: &str,
    subject_id: &str,
) -> Result<ReplicaRepairRequest, RepositoryError> {
    let request: serde_json::Value =
        serde_json::from_str(request_json).map_err(|_| RepositoryError::RuntimeStateInvalid)?;
    let object = request
        .as_object()
        .ok_or(RepositoryError::RuntimeStateInvalid)?;
    let schema_version = request
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .ok_or(RepositoryError::RuntimeStateInvalid)?;
    let recovery_point_id = request
        .get("recovery_point_id")
        .and_then(serde_json::Value::as_str)
        .ok_or(RepositoryError::RuntimeStateInvalid)?;
    let source_destination_fingerprint = request
        .get("source_destination_fingerprint")
        .and_then(serde_json::Value::as_str)
        .ok_or(RepositoryError::RuntimeStateInvalid)?;
    let target_destination_fingerprint = request
        .get("target_destination_fingerprint")
        .and_then(serde_json::Value::as_str)
        .ok_or(RepositoryError::RuntimeStateInvalid)?;
    if recovery_point_id != subject_id
        || source_destination_fingerprint.trim().is_empty()
        || source_destination_fingerprint.len() > 512
        || target_destination_fingerprint.trim().is_empty()
        || target_destination_fingerprint.len() > 512
        || source_destination_fingerprint == target_destination_fingerprint
    {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    let (source_reader_secret_ref_id, target_writer_secret_ref_id, target_reader_secret_ref_id) =
        match schema_version {
            1 if object.len() == 4 => (None, None, None),
            2 if object.len() == 7 => (
                parse_optional_secret_ref(&request, "source_reader_secret_ref_id")?,
                parse_optional_secret_ref(&request, "target_writer_secret_ref_id")?,
                parse_optional_secret_ref(&request, "target_reader_secret_ref_id")?,
            ),
            _ => return Err(RepositoryError::RuntimeStateInvalid),
        };
    Ok(ReplicaRepairRequest {
        schema_version,
        recovery_point_id: RecoveryPointId::from_str(recovery_point_id)
            .map_err(RepositoryError::from)?,
        source_destination_fingerprint: source_destination_fingerprint.to_owned(),
        target_destination_fingerprint: target_destination_fingerprint.to_owned(),
        source_reader_secret_ref_id,
        target_writer_secret_ref_id,
        target_reader_secret_ref_id,
    })
}

fn parse_optional_secret_ref(
    request: &serde_json::Value,
    field: &str,
) -> Result<Option<SecretRefId>, RepositoryError> {
    let value = request
        .get(field)
        .ok_or(RepositoryError::RuntimeStateInvalid)?;
    if value.is_null() {
        return Ok(None);
    }
    let value = value.as_str().ok_or(RepositoryError::RuntimeStateInvalid)?;
    SecretRefId::from_str(value)
        .map(Some)
        .map_err(RepositoryError::from)
}

fn replica_repair_operation_run_id(request: &ReplicaRepairRequest) -> OperationRunId {
    let identity = if request.schema_version == 1 {
        format!(
            "replica-repair\0{}\0{}",
            request.source_destination_fingerprint, request.target_destination_fingerprint
        )
    } else {
        replica_repair_identity_v2(
            &request.source_destination_fingerprint,
            &request.target_destination_fingerprint,
            request.source_reader_secret_ref_id.as_ref(),
            request.target_writer_secret_ref_id.as_ref(),
            request.target_reader_secret_ref_id.as_ref(),
        )
    };
    OperationRunId::from_stable_name(request.recovery_point_id.as_uuid(), identity.as_bytes())
}

fn optional_secret_ref_identity(secret_ref_id: Option<&SecretRefId>) -> &str {
    secret_ref_id.map(SecretRefId::as_str).unwrap_or("-")
}

struct ReplicaRepairBinding {
    operation_run_id: String,
    recovery_point_id: RecoveryPointId,
    source_destination_fingerprint: String,
    target_destination_fingerprint: String,
}

type ReplicaRepairCatalogPartRow = (String, String, bool, Option<String>, Option<i64>);

fn validate_replica_repair_operation(
    transaction: &Transaction<'_>,
    current: &CurrentClaimRow,
) -> Result<ReplicaRepairBinding, RepositoryError> {
    let operation_run_id = current
        .operation_run_id
        .as_deref()
        .ok_or(RepositoryError::RuntimeStateInvalid)?;
    let operation: Option<(String, String, String, String, String)> = transaction
        .query_row(
            "SELECT operation_kind, subject_type, subject_id, request_json, state
             FROM operation_runs
             WHERE id = ?1",
            [operation_run_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    let Some((kind, subject_type, subject_id, request_json, state)) = operation else {
        return Err(RepositoryError::NotFound);
    };
    let request = parse_replica_repair_request(&request_json, &subject_id)?;
    if kind != "replica_repair"
        || subject_type != "recovery_point"
        || state != "running"
        || operation_run_id != replica_repair_operation_run_id(&request).as_str()
    {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    Ok(ReplicaRepairBinding {
        operation_run_id: operation_run_id.to_owned(),
        recovery_point_id: request.recovery_point_id,
        source_destination_fingerprint: request.source_destination_fingerprint,
        target_destination_fingerprint: request.target_destination_fingerprint,
    })
}

fn apply_replica_repair_success(
    transaction: &Transaction<'_>,
    binding: &ReplicaRepairBinding,
    signed: &SignedSetManifestV2,
    signed_json: &str,
    outcome: &ReplicaPublishOutcomeV2,
    completed_at_ms: i64,
) -> Result<(), RepositoryError> {
    if signed.manifest.recovery_point_id.to_string() != binding.recovery_point_id.as_str()
        || outcome.destination_fingerprint != binding.target_destination_fingerprint
        || outcome.reader_status != ReaderStatusV2::Eligible
        || outcome.result.is_err()
        || outcome.payload_receipts.len() != signed.manifest.parts.len()
    {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    let source_manifest = signed
        .manifest
        .replicas
        .iter()
        .find(|replica| replica.destination_fingerprint == binding.source_destination_fingerprint)
        .ok_or(RepositoryError::RuntimeStateInvalid)?;
    let target_manifest = signed
        .manifest
        .replicas
        .iter()
        .find(|replica| replica.destination_fingerprint == binding.target_destination_fingerprint)
        .ok_or(RepositoryError::RuntimeStateInvalid)?;
    if source_manifest.destination_fingerprint == target_manifest.destination_fingerprint
        || target_manifest.requirement != outcome.requirement
    {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    let manifest_receipt = outcome
        .manifest_receipt
        .as_ref()
        .ok_or(RepositoryError::RuntimeStateInvalid)?;
    if manifest_receipt.key.is_empty()
        || usize::try_from(manifest_receipt.bytes).ok() != Some(signed_json.len())
        || outcome
            .payload_receipts
            .iter()
            .zip(&signed.manifest.parts)
            .any(|(receipt, part)| receipt.key.is_empty() || receipt.bytes != part.ciphertext_bytes)
    {
        return Err(RepositoryError::RuntimeStateInvalid);
    }

    let manifest_checksum = blake3::hash(signed_json.as_bytes()).to_hex().to_string();
    let recovery: Option<(String, String, i64, String)> = transaction
        .query_row(
            "SELECT set_manifest_checksum_blake3, signing_key_id, committed_at_ms,
                    signature_status
             FROM catalog_recovery_points
             WHERE id = ?1",
            [binding.recovery_point_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((persisted_manifest_checksum, signing_key_id, committed_at_ms, signature_status)) =
        recovery
    else {
        return Err(RepositoryError::NotFound);
    };
    if persisted_manifest_checksum != manifest_checksum
        || signing_key_id != signed.manifest.signing_key_id
        || committed_at_ms != signed.manifest.committed_at_ms
        || signature_status != "valid"
    {
        return Err(RepositoryError::RuntimeStateInvalid);
    }

    let catalog_part_count: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM catalog_artifact_parts WHERE recovery_point_id = ?1",
        [binding.recovery_point_id.as_str()],
        |row| row.get(0),
    )?;
    if usize::try_from(catalog_part_count).ok() != Some(signed.manifest.parts.len()) {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    for part in &signed.manifest.parts {
        let catalog_part: Option<ReplicaRepairCatalogPartRow> = transaction
            .query_row(
                "SELECT part_role, logical_name, required,
                        plaintext_checksum_blake3, plaintext_bytes
                 FROM catalog_artifact_parts
                 WHERE id = ?1 AND recovery_point_id = ?2",
                params![part.part_id.to_string(), binding.recovery_point_id.as_str()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((role, logical_name, required, plaintext_checksum, plaintext_bytes)) =
            catalog_part
        else {
            return Err(RepositoryError::RuntimeStateInvalid);
        };
        if role != repair_part_role_str(part.role)
            || logical_name != part.logical_name
            || required != part.required
            || plaintext_checksum.as_deref() != Some(part.plaintext_blake3.as_str())
            || plaintext_bytes != i64::try_from(part.plaintext_bytes).ok()
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
    }

    let source_valid: bool = transaction.query_row(
        "SELECT EXISTS(
             SELECT 1
             FROM catalog_artifact_replicas AS source
             WHERE source.recovery_point_id = ?1
               AND source.destination_fingerprint = ?2
               AND source.state = 'committed'
               AND source.reader_status = 'eligible'
               AND EXISTS (
                   SELECT 1
                   FROM catalog_stored_objects AS manifest
                   WHERE manifest.artifact_replica_id = source.id
                     AND manifest.object_key = ?3
                     AND manifest.object_role = 'set_manifest'
                     AND manifest.state = 'present'
               )
         )",
        params![
            binding.recovery_point_id.as_str(),
            binding.source_destination_fingerprint,
            source_manifest.manifest_object_key
        ],
        |row| row.get(0),
    )?;
    let target: Option<(String, String, String)> = transaction
        .query_row(
            "SELECT id, requirement, state
             FROM catalog_artifact_replicas
             WHERE recovery_point_id = ?1
               AND destination_fingerprint = ?2",
            params![
                binding.recovery_point_id.as_str(),
                binding.target_destination_fingerprint
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((target_replica_id, target_requirement, target_state)) = target else {
        return Err(RepositoryError::NotFound);
    };
    if !source_valid
        || target_requirement != repair_requirement_str(target_manifest.requirement)
        || !matches!(
            target_state.as_str(),
            "degraded" | "missing" | "corrupt" | "quarantined"
        )
    {
        return Err(RepositoryError::RuntimeStateInvalid);
    }

    transaction.execute(
        "DELETE FROM catalog_stored_objects WHERE artifact_replica_id = ?1",
        [&target_replica_id],
    )?;
    for (part, receipt) in signed.manifest.parts.iter().zip(&outcome.payload_receipts) {
        let object_id = stable_runtime_row_id(
            "replica-repair-payload",
            binding.recovery_point_id.as_str(),
            &format!(
                "{}|{}",
                binding.target_destination_fingerprint, part.part_id
            ),
        );
        transaction.execute(
            "INSERT INTO catalog_stored_objects(
                 id, recovery_point_id, artifact_replica_id, artifact_part_id,
                 object_role, object_key, object_version, checksum_blake3,
                 size_bytes, state, observed_at_ms, metadata_json
             ) VALUES (
                 ?1, ?2, ?3, ?4,
                 'payload', ?5, ?6, ?7,
                 ?8, 'present', ?9, '{}'
             )",
            params![
                object_id,
                binding.recovery_point_id.as_str(),
                target_replica_id,
                part.part_id.to_string(),
                part.object_key,
                receipt.version,
                part.ciphertext_blake3,
                i64::try_from(part.ciphertext_bytes)
                    .map_err(|_| RepositoryError::RuntimeStateInvalid)?,
                completed_at_ms
            ],
        )?;
    }
    let manifest_object_id = stable_runtime_row_id(
        "replica-repair-manifest",
        binding.recovery_point_id.as_str(),
        &binding.target_destination_fingerprint,
    );
    transaction.execute(
        "INSERT INTO catalog_stored_objects(
             id, recovery_point_id, artifact_replica_id,
             object_role, object_key, object_version, checksum_blake3,
             size_bytes, state, observed_at_ms, metadata_json
         ) VALUES (
             ?1, ?2, ?3,
             'set_manifest', ?4, ?5, ?6,
             ?7, 'present', ?8, '{}'
         )",
        params![
            manifest_object_id,
            binding.recovery_point_id.as_str(),
            target_replica_id,
            target_manifest.manifest_object_key,
            manifest_receipt.version,
            manifest_checksum,
            i64::try_from(signed_json.len()).map_err(|_| RepositoryError::RuntimeStateInvalid)?,
            completed_at_ms
        ],
    )?;
    let target_updated = transaction.execute(
        "UPDATE catalog_artifact_replicas
         SET state = 'committed', reader_status = 'eligible',
             committed_at_ms = ?2, last_observed_at_ms = ?2,
             metadata_json = json_set(
                 metadata_json,
                 '$.last_repair_operation_id', ?3,
                 '$.last_repair_source_fingerprint', ?4
             )
         WHERE id = ?1
           AND state IN ('degraded', 'missing', 'corrupt', 'quarantined')",
        params![
            target_replica_id,
            completed_at_ms,
            binding.operation_run_id,
            binding.source_destination_fingerprint
        ],
    )?;
    if target_updated != 1 {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    recompute_snapshot_after_retention(transaction, &binding.recovery_point_id, completed_at_ms)
}

const fn repair_requirement_str(requirement: ReplicaRequirementV2) -> &'static str {
    match requirement {
        ReplicaRequirementV2::Required => "required",
        ReplicaRequirementV2::BestEffort => "best_effort",
    }
}

const fn repair_part_role_str(role: restoreplane_core::ArtifactPartRole) -> &'static str {
    match role {
        restoreplane_core::ArtifactPartRole::DatabaseDump => "database_dump",
        restoreplane_core::ArtifactPartRole::Globals => "globals",
        restoreplane_core::ArtifactPartRole::Metadata => "metadata",
        restoreplane_core::ArtifactPartRole::Wal => "wal",
        restoreplane_core::ArtifactPartRole::Auxiliary => "auxiliary",
    }
}

fn validate_retention_delete_operation(
    transaction: &Transaction<'_>,
    current: &CurrentClaimRow,
    spec: &RetentionDeleteSettlementSpec,
) -> Result<String, RepositoryError> {
    let operation_run_id = current
        .operation_run_id
        .as_deref()
        .ok_or(RepositoryError::RuntimeStateInvalid)?;
    let row: Option<(String, String, String, String, String)> = transaction
        .query_row(
            "SELECT operation_kind, subject_type, subject_id, request_json, state
             FROM operation_runs
             WHERE id = ?1",
            [operation_run_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    let Some((kind, subject_type, subject_id, request_json, state)) = row else {
        return Err(RepositoryError::NotFound);
    };
    let request: serde_json::Value =
        serde_json::from_str(&request_json).map_err(|_| RepositoryError::RuntimeStateInvalid)?;
    let request_destination = request
        .get("destination_fingerprint")
        .and_then(serde_json::Value::as_str);
    let request_recovery_point = request
        .get("recovery_point_id")
        .and_then(serde_json::Value::as_str);
    let dry_run_digest = request
        .get("dry_run_digest_blake3")
        .and_then(serde_json::Value::as_str);
    let confirmation_digest = request
        .get("confirmation_digest_blake3")
        .and_then(serde_json::Value::as_str);
    let request_payload_digest = request
        .get("request_payload_digest_blake3")
        .and_then(serde_json::Value::as_str);
    let authorized_at_ms = request
        .get("authorized_at_ms")
        .and_then(serde_json::Value::as_i64);
    let tombstoned_at_ms = request
        .get("tombstoned_at_ms")
        .and_then(serde_json::Value::as_i64);
    let schema_version = request
        .get("schema_version")
        .and_then(serde_json::Value::as_u64);
    let common_valid = kind == "retention_delete"
        && subject_type == "artifact_replica"
        && subject_id == spec.recovery_point_id.as_str()
        && state == "running"
        && request_destination == Some(spec.destination_fingerprint.as_str())
        && request_recovery_point == Some(spec.recovery_point_id.as_str())
        && dry_run_digest.is_some_and(is_hex_digest)
        && authorized_at_ms.is_some_and(|value| value >= 0 && value <= spec.completed_at_ms)
        && tombstoned_at_ms.is_some_and(|value| value >= 0 && value <= spec.completed_at_ms);
    let authority_valid = match schema_version {
        Some(2) => {
            confirmation_digest.is_some_and(is_hex_digest)
                && request_payload_digest.is_some_and(is_hex_digest)
        }
        Some(3) => {
            request
                .get("authority_kind")
                .and_then(serde_json::Value::as_str)
                == Some("published_retention_policy")
                && validate_automatic_policy_authorization_in_transaction(
                    transaction,
                    operation_run_id,
                    &subject_id,
                    &request_json,
                    None,
                )
                .is_ok()
        }
        _ => false,
    };
    if !common_valid || !authority_valid {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    Ok(operation_run_id.to_owned())
}

fn validate_automatic_retention_destructive_step(
    transaction: &Transaction<'_>,
    current: &CurrentClaimRow,
    observed_at_ms: i64,
) -> Result<(), RepositoryError> {
    if current.kind != "operation" {
        return Ok(());
    }
    let Some(operation_run_id) = current.operation_run_id.as_deref() else {
        return Ok(());
    };
    let row: Option<(String, String, String)> = transaction
        .query_row(
            "SELECT operation_kind, subject_id, request_json
             FROM operation_runs
             WHERE id = ?1",
            [operation_run_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((kind, subject_id, request_json)) = row else {
        return Err(RepositoryError::NotFound);
    };
    if kind != "retention_delete" {
        return Ok(());
    }
    let schema_version = serde_json::from_str::<serde_json::Value>(&request_json)
        .map_err(|_| RepositoryError::RuntimeStateInvalid)?
        .get("schema_version")
        .and_then(serde_json::Value::as_u64);
    if schema_version == Some(3) {
        validate_automatic_policy_authorization_in_transaction(
            transaction,
            operation_run_id,
            &subject_id,
            &request_json,
            Some(observed_at_ms),
        )?;
    }
    Ok(())
}

struct RetentionCatalogObjectRow {
    key: String,
    version: Option<String>,
    role: String,
    state: String,
}

fn apply_retention_delete_report(
    transaction: &Transaction<'_>,
    spec: &RetentionDeleteSettlementSpec,
) -> Result<String, RepositoryError> {
    let replica: Option<(String, String)> = transaction
        .query_row(
            "SELECT replicas.id, recovery.state
             FROM catalog_artifact_replicas AS replicas
             JOIN catalog_recovery_points AS recovery
               ON recovery.id = replicas.recovery_point_id
             WHERE replicas.recovery_point_id = ?1
               AND replicas.destination_fingerprint = ?2",
            params![
                spec.recovery_point_id.as_str(),
                spec.destination_fingerprint
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((replica_id, recovery_state)) = replica else {
        return Err(RepositoryError::NotFound);
    };
    if !matches!(recovery_state.as_str(), "complete" | "incomplete") {
        return Err(RepositoryError::RuntimeStateInvalid);
    }

    let mut statement = transaction.prepare(
        "SELECT object_key, object_version, object_role, state
         FROM catalog_stored_objects
         WHERE recovery_point_id = ?1
           AND artifact_replica_id = ?2
         ORDER BY object_key",
    )?;
    let rows = statement.query_map(
        params![spec.recovery_point_id.as_str(), replica_id],
        |row| {
            Ok(RetentionCatalogObjectRow {
                key: row.get(0)?,
                version: row.get(1)?,
                role: row.get(2)?,
                state: row.get(3)?,
            })
        },
    )?;
    let catalog_objects = rows.collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    if catalog_objects.len() != spec.report.observations.len() {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    let mut set_manifest_observations = spec
        .report
        .observations
        .iter()
        .filter(|observation| observation.role == RetentionObjectRoleV2::SetManifest);
    let set_manifest = set_manifest_observations
        .next()
        .ok_or(RepositoryError::RuntimeStateInvalid)?;
    if set_manifest_observations.next().is_some()
        || set_manifest.disposition == DeleteObjectDispositionV2::NotAttemptedMarkerBlocked
    {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    let marker_deleted = set_manifest.disposition == DeleteObjectDispositionV2::Deleted;
    if spec.report.observations.iter().any(|observation| {
        observation.role != RetentionObjectRoleV2::SetManifest
            && if marker_deleted {
                observation.disposition == DeleteObjectDispositionV2::NotAttemptedMarkerBlocked
            } else {
                observation.disposition != DeleteObjectDispositionV2::NotAttemptedMarkerBlocked
            }
    }) {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    let catalog_by_identity = catalog_objects
        .iter()
        .map(|object| ((object.key.as_str(), object.version.as_deref()), object))
        .collect::<std::collections::BTreeMap<_, _>>();
    for observation in &spec.report.observations {
        let identity = (
            observation.object_key.as_str(),
            observation.object_version.as_deref(),
        );
        let Some(catalog_object) = catalog_by_identity.get(&identity) else {
            return Err(RepositoryError::RuntimeStateInvalid);
        };
        if catalog_object.role != retention_role_str(observation.role)
            || (catalog_object.state == "deleted"
                && observation.disposition != DeleteObjectDispositionV2::Deleted)
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
    }

    let mut deleted_any = false;
    for observation in &spec.report.observations {
        if observation.disposition != DeleteObjectDispositionV2::Deleted {
            continue;
        }
        deleted_any = true;
        let affected = transaction.execute(
            "UPDATE catalog_stored_objects
             SET state = 'deleted', observed_at_ms = ?5
             WHERE recovery_point_id = ?1
               AND artifact_replica_id = ?2
               AND object_key = ?3
               AND object_version IS ?4",
            params![
                spec.recovery_point_id.as_str(),
                replica_id,
                observation.object_key,
                observation.object_version,
                spec.completed_at_ms
            ],
        )?;
        if affected != 1 {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
    }
    if deleted_any {
        let remaining: i64 = transaction.query_row(
            "SELECT COUNT(*)
             FROM catalog_stored_objects
             WHERE recovery_point_id = ?1
               AND artifact_replica_id = ?2
               AND state <> 'deleted'",
            params![spec.recovery_point_id.as_str(), replica_id],
            |row| row.get(0),
        )?;
        let replica_state = if remaining == 0 {
            "missing"
        } else {
            "degraded"
        };
        let affected = transaction.execute(
            "UPDATE catalog_artifact_replicas
             SET state = ?2, reader_status = 'unavailable',
                 last_observed_at_ms = ?3
             WHERE id = ?1",
            params![replica_id, replica_state, spec.completed_at_ms],
        )?;
        if affected != 1 {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
    }
    Ok(replica_id)
}

fn recompute_snapshot_after_retention(
    transaction: &Transaction<'_>,
    recovery_point_id: &RecoveryPointId,
    projected_at_ms: i64,
) -> Result<(), RepositoryError> {
    let required_part_count: i64 = transaction.query_row(
        "SELECT COUNT(*)
         FROM catalog_artifact_parts
         WHERE recovery_point_id = ?1 AND required = 1",
        [recovery_point_id.as_str()],
        |row| row.get(0),
    )?;
    if required_part_count <= 0 {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    let eligible_replica_count: i64 = transaction.query_row(
        "SELECT COUNT(*)
         FROM catalog_artifact_replicas AS replicas
         WHERE replicas.recovery_point_id = ?1
           AND replicas.state = 'committed'
           AND replicas.reader_status = 'eligible'
           AND NOT EXISTS (
               SELECT 1
               FROM catalog_artifact_parts AS parts
               WHERE parts.recovery_point_id = replicas.recovery_point_id
                 AND parts.required = 1
                 AND NOT EXISTS (
                     SELECT 1
                     FROM catalog_stored_objects AS objects
                     WHERE objects.recovery_point_id = parts.recovery_point_id
                       AND objects.artifact_replica_id = replicas.id
                       AND objects.artifact_part_id = parts.id
                       AND objects.object_role = 'payload'
                       AND objects.state = 'present'
                 )
           )",
        [recovery_point_id.as_str()],
        |row| row.get(0),
    )?;
    let (required_replica_count, healthy_required_replica_count): (i64, i64) = transaction
        .query_row(
            "SELECT
                 COUNT(*),
                 SUM(CASE
                     WHEN replicas.state = 'committed'
                      AND replicas.reader_status = 'eligible'
                      AND NOT EXISTS (
                          SELECT 1
                          FROM catalog_artifact_parts AS parts
                          WHERE parts.recovery_point_id = replicas.recovery_point_id
                            AND parts.required = 1
                            AND NOT EXISTS (
                                SELECT 1
                                FROM catalog_stored_objects AS objects
                                WHERE objects.recovery_point_id = parts.recovery_point_id
                                  AND objects.artifact_replica_id = replicas.id
                                  AND objects.artifact_part_id = parts.id
                                  AND objects.object_role = 'payload'
                                  AND objects.state = 'present'
                            )
                      )
                     THEN 1 ELSE 0
                 END)
             FROM catalog_artifact_replicas AS replicas
             WHERE replicas.recovery_point_id = ?1
               AND replicas.requirement = 'required'",
            [recovery_point_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
    if required_replica_count <= 0 {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    let all_required_healthy = healthy_required_replica_count == required_replica_count;
    let artifact_completeness = if eligible_replica_count > 0 {
        "complete"
    } else {
        "incomplete"
    };
    let replica_durability = if all_required_healthy {
        "healthy"
    } else if eligible_replica_count > 0 {
        "degraded"
    } else {
        "unavailable"
    };
    let protection_compliance = if all_required_healthy {
        "compliant"
    } else {
        "non_compliant"
    };
    let restore_eligibility = match replica_durability {
        "healthy" => "eligible",
        "degraded" => "conditional",
        _ => "blocked",
    };
    let present_part_count = if eligible_replica_count > 0 {
        required_part_count
    } else {
        0
    };
    let recovery_state = if present_part_count >= required_part_count {
        "complete"
    } else {
        "incomplete"
    };
    let recovery_updated = transaction.execute(
        "UPDATE catalog_recovery_points
         SET state = ?2, present_part_count = ?3
         WHERE id = ?1
           AND state IN ('complete', 'incomplete')",
        params![
            recovery_point_id.as_str(),
            recovery_state,
            present_part_count
        ],
    )?;
    let snapshot_updated = transaction.execute(
        "UPDATE catalog_snapshot_projection
         SET artifact_completeness = ?2,
             replica_durability = ?3,
             protection_compliance = ?4,
             restore_eligibility = ?5,
             projection_version = projection_version + 1,
             projected_at_ms = ?6
         WHERE recovery_point_id = ?1",
        params![
            recovery_point_id.as_str(),
            artifact_completeness,
            replica_durability,
            protection_compliance,
            restore_eligibility,
            projected_at_ms
        ],
    )?;
    if recovery_updated != 1 || snapshot_updated != 1 {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn record_retention_evidence(
    transaction: &Transaction<'_>,
    claim: &ClaimedWork,
    operation_run_id: &str,
    replica_id: &str,
    result_json: &str,
    incomplete: bool,
    retryable: bool,
    observed_at_ms: i64,
) -> Result<(), RepositoryError> {
    let event_id = stable_runtime_row_id(
        "retention-delete-event",
        claim.attempt_id.as_str(),
        operation_run_id,
    );
    let event_code = if incomplete {
        "RETENTION_DELETE_INCOMPLETE"
    } else {
        "RETENTION_DELETE_COMPLETED"
    };
    let severity = if incomplete { "error" } else { "info" };
    let retryability = if incomplete {
        Some(if retryable { "retryable" } else { "terminal" })
    } else {
        None
    };
    let message = if incomplete {
        "Retention delete left one or more destination objects intact"
    } else {
        "Retention delete removed every confirmed destination object"
    };
    transaction.execute(
        "INSERT INTO activity_events(
             id, occurred_at_ms, severity, event_code, phase,
             entity_type, entity_id, correlation_id, work_item_id, attempt_id,
             retryability, message, attributes_json, redaction_version, created_at_ms
         ) VALUES (
             ?1, ?2, ?3, ?4, 'delete',
             'artifact_replica', ?5, ?6, ?7, ?8,
             ?9, ?10, ?11, 1, ?2
         )",
        params![
            event_id,
            observed_at_ms,
            severity,
            event_code,
            replica_id,
            operation_run_id,
            claim.work_item_id.as_str(),
            claim.attempt_id.as_str(),
            retryability,
            message,
            result_json
        ],
    )?;
    let event_seq = transaction.last_insert_rowid();
    let fingerprint = format!("retention-delete:{operation_run_id}");
    if incomplete {
        let existing: Option<(String, i64)> = transaction
            .query_row(
                "SELECT id, occurrence_count
                 FROM attention_states
                 WHERE fingerprint = ?1
                   AND state IN ('open', 'acknowledged')",
                [fingerprint.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let summary =
            "Retention delete is incomplete; inspect preserved and failed destination objects";
        let category = if retryable {
            "destination_health"
        } else {
            "protection_compliance"
        };
        let attention_id = if let Some((id, occurrence_count)) = existing {
            let affected = transaction.execute(
                "UPDATE attention_states
                 SET category = ?2, severity = 'error', summary = ?3,
                     last_seen_at_ms = ?4,
                     occurrence_count = occurrence_count + 1,
                     updated_at_ms = ?4
                 WHERE id = ?1
                   AND state IN ('open', 'acknowledged')
                   AND occurrence_count = ?5",
                params![id, category, summary, observed_at_ms, occurrence_count],
            )?;
            if affected != 1 {
                return Err(RepositoryError::RuntimeStateInvalid);
            }
            id
        } else {
            let id =
                stable_runtime_row_id("retention-delete-attention", operation_run_id, replica_id);
            transaction.execute(
                "INSERT INTO attention_states(
                     id, fingerprint, category, severity, entity_type, entity_id,
                     state, summary, first_seen_at_ms, last_seen_at_ms,
                     occurrence_count, created_at_ms, updated_at_ms
                 ) VALUES (
                     ?1, ?2, ?3, 'error', 'artifact_replica', ?4,
                     'open', ?5, ?6, ?6,
                     1, ?6, ?6
                 )",
                params![
                    id,
                    fingerprint,
                    category,
                    replica_id,
                    summary,
                    observed_at_ms
                ],
            )?;
            id
        };
        transaction.execute(
            "INSERT INTO attention_event_links(
                 attention_state_id, activity_event_seq, created_at_ms
             ) VALUES (?1, ?2, ?3)",
            params![attention_id, event_seq, observed_at_ms],
        )?;
    } else {
        transaction.execute(
            "UPDATE attention_states
             SET state = 'resolved', resolved_at_ms = ?2,
                 resolution_code = 'RETENTION_DELETE_CONVERGED',
                 updated_at_ms = ?2
             WHERE fingerprint = ?1
               AND state IN ('open', 'acknowledged')",
            params![fingerprint, observed_at_ms],
        )?;
    }
    Ok(())
}

fn is_retention_object_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 2_048
        && !key.starts_with('/')
        && !key.contains('\\')
        && !key
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
}

const fn retention_role_str(role: RetentionObjectRoleV2) -> &'static str {
    match role {
        RetentionObjectRoleV2::Payload => "payload",
        RetentionObjectRoleV2::PartManifest => "part_manifest",
        RetentionObjectRoleV2::SetManifest => "set_manifest",
        RetentionObjectRoleV2::VerificationEvidence => "verification_evidence",
    }
}

const fn retention_disposition_str(disposition: DeleteObjectDispositionV2) -> &'static str {
    match disposition {
        DeleteObjectDispositionV2::Deleted => "deleted",
        DeleteObjectDispositionV2::RetainedByLock => "retained_by_lock",
        DeleteObjectDispositionV2::RetryRequired => "retry_required",
        DeleteObjectDispositionV2::FailedTerminal => "failed_terminal",
        DeleteObjectDispositionV2::NotAttemptedMarkerBlocked => "not_attempted_marker_blocked",
    }
}

#[allow(clippy::too_many_arguments)]
fn retry_claim_in_transaction(
    transaction: &Transaction<'_>,
    current: &CurrentClaimRow,
    claim: &ClaimedWork,
    completed_at_ms: i64,
    available_at_ms: i64,
    retry_error_code: &str,
    terminal_spec: &ClaimCompletionSpec,
    cancel_spec: &ClaimCompletionSpec,
) -> Result<RetryDisposition, RepositoryError> {
    if current.work_state == WorkItemState::CancelRequested {
        settle_claim_terminal(transaction, current, cancel_spec)?;
        return Ok(RetryDisposition::Cancelled);
    }
    let (attempt_limit, attempts_used): (i64, i64) = transaction.query_row(
        "SELECT
             attempt_limit,
             (
                 SELECT COUNT(*)
                 FROM work_attempts
                 WHERE work_attempts.work_item_id = work_items.id
                   AND work_attempts.state <> 'abandoned'
             )
         FROM work_items
         WHERE id = ?1",
        [claim.work_item_id.as_str()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if attempt_limit <= 0 || attempts_used <= 0 || attempts_used > attempt_limit {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    let exhausted = attempts_used == attempt_limit;
    let next_work = if exhausted {
        transition_work_item(current.work_state, WorkItemCommand::FailTerminal)?
    } else {
        transition_work_item(current.work_state, WorkItemCommand::Release)?
    };
    let next_attempt = transition_work_attempt(current.attempt_state, WorkAttemptCommand::Fail)?;
    if exhausted {
        advance_subject_terminal(transaction, current, terminal_spec)?;
    }

    let item_affected = transaction.execute(
        "UPDATE work_items
         SET state = ?7, available_at_ms = ?8, lease_owner = NULL,
             lease_boot_id = NULL, lease_expires_at_ms = NULL,
             heartbeat_at_ms = NULL,
             wait_reason = CASE WHEN ?9 THEN NULL ELSE 'retryable' END,
             terminal_at_ms = CASE WHEN ?9 THEN ?10 ELSE terminal_at_ms END,
             updated_at_ms = ?10
         WHERE id = ?1
           AND current_attempt_no = ?2
           AND fencing_token = ?3
           AND lease_owner = ?4
           AND lease_boot_id = ?5
           AND state = ?6",
        params![
            claim.work_item_id.as_str(),
            claim.attempt_no,
            claim.fencing_token,
            claim.worker_id.as_str(),
            claim.worker_boot_id.as_str(),
            current.work_state.as_str(),
            next_work.as_str(),
            available_at_ms,
            exhausted,
            completed_at_ms
        ],
    )?;
    if item_affected != 1 {
        return Err(RepositoryError::StaleFence);
    }
    let attempt_affected = transaction.execute(
        "UPDATE work_attempts
         SET state = ?7, completed_at_ms = ?8,
             retry_class = CASE WHEN ?9 THEN 'terminal' ELSE 'retryable' END,
             retry_after_ms = CASE WHEN ?9 THEN NULL ELSE ?10 END,
             error_code = CASE WHEN ?9 THEN 'RETRY_BUDGET_EXHAUSTED' ELSE ?11 END
         WHERE id = ?1
           AND work_item_id = ?2
           AND attempt_no = ?3
           AND fencing_token = ?4
           AND worker_id = ?5
           AND worker_boot_id = ?6
           AND state = ?12",
        params![
            claim.attempt_id.as_str(),
            claim.work_item_id.as_str(),
            claim.attempt_no,
            claim.fencing_token,
            claim.worker_id.as_str(),
            claim.worker_boot_id.as_str(),
            next_attempt.as_str(),
            completed_at_ms,
            exhausted,
            available_at_ms,
            retry_error_code,
            current.attempt_state.as_str()
        ],
    )?;
    if attempt_affected != 1 {
        return Err(RepositoryError::StaleFence);
    }
    release_claim_capacity(transaction, claim, completed_at_ms, "failed", false)?;
    Ok(if exhausted {
        RetryDisposition::Exhausted
    } else {
        RetryDisposition::Requeued
    })
}

fn settle_claim_terminal(
    transaction: &Transaction<'_>,
    current: &CurrentClaimRow,
    spec: &ClaimCompletionSpec,
) -> Result<(), RepositoryError> {
    let (work_command, attempt_command, retry_class, release_reason) = match spec.outcome {
        ClaimTerminalOutcome::Succeeded => (
            WorkItemCommand::Succeed,
            WorkAttemptCommand::Succeed,
            "terminal",
            "completed",
        ),
        ClaimTerminalOutcome::PartiallySucceeded => (
            WorkItemCommand::PartialSucceed,
            WorkAttemptCommand::Succeed,
            "terminal",
            "completed",
        ),
        ClaimTerminalOutcome::Failed => (
            WorkItemCommand::FailTerminal,
            WorkAttemptCommand::Fail,
            "terminal",
            "failed",
        ),
        ClaimTerminalOutcome::Cancelled => (
            WorkItemCommand::Cancel,
            WorkAttemptCommand::Cancel,
            "cancelled",
            "cancelled",
        ),
    };
    let next_work = transition_work_item(current.work_state, work_command)?;
    let next_attempt = transition_work_attempt(current.attempt_state, attempt_command)?;
    advance_subject_terminal(transaction, current, spec)?;

    let item_affected = transaction.execute(
        "UPDATE work_items
         SET state = ?7, lease_owner = NULL, lease_boot_id = NULL,
             lease_expires_at_ms = NULL, heartbeat_at_ms = NULL,
             wait_reason = NULL, terminal_at_ms = ?8, updated_at_ms = ?8
         WHERE id = ?1
           AND current_attempt_no = ?2
           AND fencing_token = ?3
           AND lease_owner = ?4
           AND lease_boot_id = ?5
           AND state = ?6",
        params![
            spec.claim.work_item_id.as_str(),
            spec.claim.attempt_no,
            spec.claim.fencing_token,
            spec.claim.worker_id.as_str(),
            spec.claim.worker_boot_id.as_str(),
            current.work_state.as_str(),
            next_work.as_str(),
            spec.completed_at_ms
        ],
    )?;
    let attempt_affected = transaction.execute(
        "UPDATE work_attempts
         SET state = ?7, completed_at_ms = ?8, retry_class = ?9,
             retry_after_ms = NULL, error_code = ?10
         WHERE id = ?1
           AND work_item_id = ?2
           AND attempt_no = ?3
           AND fencing_token = ?4
           AND worker_id = ?5
           AND worker_boot_id = ?6
           AND state = ?11",
        params![
            spec.claim.attempt_id.as_str(),
            spec.claim.work_item_id.as_str(),
            spec.claim.attempt_no,
            spec.claim.fencing_token,
            spec.claim.worker_id.as_str(),
            spec.claim.worker_boot_id.as_str(),
            next_attempt.as_str(),
            spec.completed_at_ms,
            retry_class,
            spec.error_code.as_deref(),
            current.attempt_state.as_str()
        ],
    )?;
    if item_affected != 1 || attempt_affected != 1 {
        return Err(RepositoryError::StaleFence);
    }
    release_claim_capacity(
        transaction,
        &spec.claim,
        spec.completed_at_ms,
        release_reason,
        false,
    )
}

fn advance_subject_terminal(
    transaction: &Transaction<'_>,
    current: &CurrentClaimRow,
    spec: &ClaimCompletionSpec,
) -> Result<(), RepositoryError> {
    match current.kind.as_str() {
        "operation" => {
            let operation_id = current
                .operation_run_id
                .as_deref()
                .ok_or(RepositoryError::RuntimeStateInvalid)?;
            let next = match spec.outcome {
                ClaimTerminalOutcome::Succeeded => "succeeded",
                ClaimTerminalOutcome::PartiallySucceeded => "partially_succeeded",
                ClaimTerminalOutcome::Failed => "failed",
                ClaimTerminalOutcome::Cancelled => "cancelled",
            };
            let affected = transaction.execute(
                "UPDATE operation_runs
                 SET state = ?2, result_json = ?3, error_code = ?4,
                     completed_at_ms = ?5, updated_at_ms = ?5
                 WHERE id = ?1 AND state IN ('queued', 'running')",
                params![
                    operation_id,
                    next,
                    spec.result_json.as_deref(),
                    spec.error_code.as_deref(),
                    spec.completed_at_ms
                ],
            )?;
            if affected != 1 {
                return Err(RepositoryError::StaleFence);
            }
            Ok(())
        }
        "catalog_rebuild" => {
            let rebuild_id = current
                .catalog_rebuild_run_id
                .as_deref()
                .ok_or(RepositoryError::RuntimeStateInvalid)?;
            let next = match spec.outcome {
                ClaimTerminalOutcome::Succeeded | ClaimTerminalOutcome::PartiallySucceeded => {
                    "succeeded"
                }
                ClaimTerminalOutcome::Failed => "failed",
                ClaimTerminalOutcome::Cancelled => "cancelled",
            };
            let (accepted_count, rejected_count) =
                catalog_rebuild_terminal_counts(spec.result_json.as_deref())?;
            let affected = transaction.execute(
                "UPDATE catalog_rebuild_runs
                 SET status = ?2, accepted_count = ?3, rejected_count = ?4,
                     completed_at_ms = ?5, error_code = ?6
                 WHERE id = ?1 AND status IN ('queued', 'running')",
                params![
                    rebuild_id,
                    next,
                    accepted_count,
                    rejected_count,
                    spec.completed_at_ms,
                    spec.error_code.as_deref()
                ],
            )?;
            if affected != 1 {
                return Err(RepositoryError::StaleFence);
            }
            Ok(())
        }
        "restore" => {
            let restore_id = current
                .restore_run_id
                .as_deref()
                .ok_or(RepositoryError::RuntimeStateInvalid)?;
            let evidence = spec
                .result_json
                .as_deref()
                .ok_or(RepositoryError::RuntimeStateInvalid)?;
            let terminal = restore_terminal_projection(evidence, spec.outcome)?;
            let (current_state, purpose): (String, String) = transaction
                .query_row(
                    "SELECT state, purpose FROM restore_runs WHERE id = ?1",
                    [restore_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?
                .ok_or(RepositoryError::NotFound)?;
            let state_valid = match spec.outcome {
                ClaimTerminalOutcome::Succeeded => current_state == "validating",
                ClaimTerminalOutcome::Failed
                | ClaimTerminalOutcome::Cancelled
                | ClaimTerminalOutcome::PartiallySucceeded => {
                    matches!(current_state.as_str(), "queued" | "running" | "validating")
                }
            };
            if !state_valid {
                return Err(RepositoryError::RuntimeStateInvalid);
            }
            let error_code = match spec.outcome {
                ClaimTerminalOutcome::Succeeded => None,
                ClaimTerminalOutcome::Cancelled => {
                    Some(spec.error_code.as_deref().unwrap_or("RESTORE_CANCELLED"))
                }
                ClaimTerminalOutcome::Failed | ClaimTerminalOutcome::PartiallySucceeded => {
                    Some(spec.error_code.as_deref().unwrap_or("RESTORE_FAILED"))
                }
            };
            let affected = transaction.execute(
                "UPDATE restore_runs
                 SET state = ?2,
                     validation_result_json = ?3,
                     measured_rto_ms = CASE
                         WHEN started_at_ms IS NULL THEN NULL
                         ELSE ?4 - started_at_ms
                     END,
                     error_code = ?5,
                     completed_at_ms = ?4,
                     updated_at_ms = ?4
                 WHERE id = ?1 AND state = ?6",
                params![
                    restore_id,
                    terminal.state,
                    evidence,
                    spec.completed_at_ms,
                    error_code,
                    current_state
                ],
            )?;
            if affected != 1 {
                return Err(RepositoryError::StaleFence);
            }
            if purpose == "restore_drill" {
                let (verification_state, achieved_level, verification_error_code) = match spec
                    .outcome
                {
                    ClaimTerminalOutcome::Succeeded => ("passed", Some(3_i64), None),
                    ClaimTerminalOutcome::Cancelled => (
                        "cancelled",
                        None,
                        Some(spec.error_code.as_deref().unwrap_or("RESTORE_CANCELLED")),
                    ),
                    ClaimTerminalOutcome::Failed | ClaimTerminalOutcome::PartiallySucceeded => (
                        "failed",
                        None,
                        Some(spec.error_code.as_deref().unwrap_or("RESTORE_FAILED")),
                    ),
                };
                let verification_affected = transaction.execute(
                    "UPDATE verification_runs
                     SET state = ?2,
                         achieved_level = ?3,
                         evidence_json = ?4,
                         completed_at_ms = ?5,
                         error_code = ?6,
                         updated_at_ms = ?5
                     WHERE restore_run_id = ?1
                       AND trigger_kind = 'restore_drill'
                       AND requested_level = 3
                       AND state IN ('queued', 'running')",
                    params![
                        restore_id,
                        verification_state,
                        achieved_level,
                        evidence,
                        spec.completed_at_ms,
                        verification_error_code
                    ],
                )?;
                if verification_affected != 1 {
                    return Err(RepositoryError::RuntimeStateInvalid);
                }
                let verification_id: String = transaction.query_row(
                    "SELECT id FROM verification_runs
                     WHERE restore_run_id = ?1
                       AND trigger_kind = 'restore_drill'
                       AND requested_level = 3",
                    [restore_id],
                    |row| row.get(0),
                )?;
                project_terminal_verification_run(
                    transaction,
                    &verification_id,
                    spec.completed_at_ms,
                )?;
            }
            Ok(())
        }
        "verification" => {
            let verification_id = current
                .verification_run_id
                .as_deref()
                .ok_or(RepositoryError::RuntimeStateInvalid)?;
            let (next, achieved_level, evidence_json, default_error_code) = match spec.outcome {
                ClaimTerminalOutcome::Succeeded => {
                    let evidence = spec.result_json.as_deref().unwrap_or("{}");
                    let parsed = serde_json::from_str::<serde_json::Value>(evidence)
                        .map_err(|_| RepositoryError::VerificationInputInvalid)?;
                    if !parsed.is_object() {
                        return Err(RepositoryError::VerificationInputInvalid);
                    }
                    ("passed", true, Some(evidence), None)
                }
                ClaimTerminalOutcome::PartiallySucceeded => (
                    "failed",
                    false,
                    spec.result_json.as_deref(),
                    Some("VERIFY_PARTIAL_RESULT"),
                ),
                ClaimTerminalOutcome::Failed => (
                    "failed",
                    false,
                    spec.result_json.as_deref(),
                    Some("VERIFY_FAILED"),
                ),
                ClaimTerminalOutcome::Cancelled => (
                    "cancelled",
                    false,
                    spec.result_json.as_deref(),
                    Some("VERIFY_CANCELLED"),
                ),
            };
            if evidence_json.is_some_and(|evidence| {
                serde_json::from_str::<serde_json::Value>(evidence)
                    .map(|value| !value.is_object())
                    .unwrap_or(true)
            }) {
                return Err(RepositoryError::VerificationInputInvalid);
            }
            let affected = transaction.execute(
                "UPDATE verification_runs
                 SET state = ?2,
                     achieved_level = CASE WHEN ?3 THEN requested_level ELSE NULL END,
                     evidence_json = ?4,
                     completed_at_ms = ?5,
                     error_code = ?6,
                     updated_at_ms = ?5
                 WHERE id = ?1
                   AND state IN ('queued', 'running')",
                params![
                    verification_id,
                    next,
                    achieved_level,
                    evidence_json,
                    spec.completed_at_ms,
                    spec.error_code.as_deref().or(default_error_code)
                ],
            )?;
            if affected != 1 {
                return Err(RepositoryError::StaleFence);
            }
            project_terminal_verification_run(transaction, verification_id, spec.completed_at_ms)?;
            Ok(())
        }
        "backup" => {
            let backup_id = current
                .backup_run_id
                .as_deref()
                .ok_or(RepositoryError::RuntimeStateInvalid)?;
            let (state, occurrence_id): (String, String) = transaction.query_row(
                "SELECT state, plan_occurrence_id FROM backup_runs WHERE id = ?1",
                [backup_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let current_state = BackupRunState::from_str(&state)?;
            let command = match spec.outcome {
                ClaimTerminalOutcome::Succeeded => BackupRunCommand::Succeed,
                ClaimTerminalOutcome::PartiallySucceeded => BackupRunCommand::PartialSucceed,
                ClaimTerminalOutcome::Failed => BackupRunCommand::Fail,
                ClaimTerminalOutcome::Cancelled => BackupRunCommand::Cancel,
            };
            let next = transition_backup_run(current_state, command)?;
            let affected = transaction.execute(
                "UPDATE backup_runs
                 SET state = ?2, error_code = ?3, completed_at_ms = ?4,
                     updated_at_ms = ?4
                 WHERE id = ?1 AND state = ?5",
                params![
                    backup_id,
                    next.as_str(),
                    spec.error_code.as_deref(),
                    spec.completed_at_ms,
                    state
                ],
            )?;
            if affected != 1 {
                return Err(RepositoryError::StaleFence);
            }
            settle_occurrence_if_terminal(transaction, &occurrence_id, spec.completed_at_ms)
        }
        _ => Err(RepositoryError::RuntimeStateInvalid),
    }
}

fn settle_occurrence_if_terminal(
    transaction: &Transaction<'_>,
    occurrence_id: &str,
    completed_at_ms: i64,
) -> Result<(), RepositoryError> {
    let counts: (i64, i64, i64, i64, i64, i64) = transaction.query_row(
        "SELECT
             COUNT(*),
             SUM(CASE WHEN state = 'succeeded' THEN 1 ELSE 0 END),
             SUM(CASE WHEN state = 'partially_succeeded' THEN 1 ELSE 0 END),
             SUM(CASE WHEN state = 'failed' THEN 1 ELSE 0 END),
             SUM(CASE WHEN state = 'cancelled' THEN 1 ELSE 0 END),
             SUM(CASE WHEN state NOT IN (
                 'succeeded', 'partially_succeeded', 'failed', 'cancelled'
             ) THEN 1 ELSE 0 END)
         FROM backup_runs
         WHERE plan_occurrence_id = ?1",
        [occurrence_id],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        },
    )?;
    let (total, succeeded, partial, failed, cancelled, active) = counts;
    if total == 0 || active != 0 {
        return Ok(());
    }
    let desired = if succeeded == total {
        PlanOccurrenceState::Succeeded
    } else if cancelled == total {
        PlanOccurrenceState::Cancelled
    } else if failed == total {
        PlanOccurrenceState::Failed
    } else if succeeded + partial > 0 {
        PlanOccurrenceState::PartiallySucceeded
    } else {
        PlanOccurrenceState::Failed
    };
    let state: String = transaction.query_row(
        "SELECT state FROM plan_occurrences WHERE id = ?1",
        [occurrence_id],
        |row| row.get(0),
    )?;
    let mut current = PlanOccurrenceState::from_str(&state)?;
    if current == desired {
        return Ok(());
    }
    if current == PlanOccurrenceState::Enqueued && desired != PlanOccurrenceState::Cancelled {
        current = transition_plan_occurrence(current, OccurrenceCommand::Start)?;
    }
    let command = match desired {
        PlanOccurrenceState::Succeeded => OccurrenceCommand::Succeed,
        PlanOccurrenceState::PartiallySucceeded => OccurrenceCommand::PartialSucceed,
        PlanOccurrenceState::Failed => OccurrenceCommand::Fail,
        PlanOccurrenceState::Cancelled => OccurrenceCommand::Cancel,
        _ => return Err(RepositoryError::RuntimeStateInvalid),
    };
    let next = transition_plan_occurrence(current, command)?;
    let affected = transaction.execute(
        "UPDATE plan_occurrences
         SET state = ?2, updated_at_ms = ?3
         WHERE id = ?1 AND state = ?4",
        params![occurrence_id, next.as_str(), completed_at_ms, state],
    )?;
    if affected != 1 {
        return Err(RepositoryError::StaleFence);
    }
    Ok(())
}

fn load_restore_claim_row(
    transaction: &Transaction<'_>,
    restore_run_id: &str,
) -> Result<RestoreClaimRow, RepositoryError> {
    transaction
        .query_row(
            "SELECT recovery_point_id, source_replica_id,
                    target_connection_profile_id, target_database_name,
                    target_fingerprint, purpose, conflict_policy,
                    eligibility_snapshot_json, confirmation_token_id, state
             FROM restore_runs
             WHERE id = ?1",
            [restore_run_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                ))
            },
        )
        .optional()?
        .ok_or(RepositoryError::NotFound)
}

fn restore_context_is_authoritatively_eligible(
    transaction: &Transaction<'_>,
    restore_run_id: &str,
    row: &RestoreClaimRow,
    authorization_at_ms: i64,
    current_at_ms: i64,
    live: RestoreTargetObservation<'_>,
) -> Result<bool, RepositoryError> {
    let (
        recovery_point_id,
        source_replica_id,
        target_connection_id,
        target_database_name,
        stored_target_fingerprint,
        purpose,
        conflict_policy,
        eligibility_snapshot_json,
        confirmation_token_id,
        _state,
    ) = row;
    let snapshot: serde_json::Value = match serde_json::from_str(eligibility_snapshot_json) {
        Ok(serde_json::Value::Object(values)) => serde_json::Value::Object(values),
        _ => return Ok(false),
    };
    let expected_mode = match conflict_policy.as_str() {
        "create_new" | "fail_if_exists" => "create_new",
        "overwrite" => "overwrite",
        _ => return Ok(false),
    };
    let snapshot_replica_id = snapshot
        .pointer("/source/replicaId")
        .and_then(serde_json::Value::as_str);
    let binding_valid = snapshot
        .get("eligible")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
        && snapshot.get("mode").and_then(serde_json::Value::as_str) == Some(expected_mode)
        && snapshot
            .get("targetConnectionId")
            .and_then(serde_json::Value::as_str)
            == Some(target_connection_id.as_str())
        && snapshot
            .get("targetName")
            .and_then(serde_json::Value::as_str)
            == Some(target_database_name.as_str())
        && snapshot
            .get("targetFingerprint")
            .and_then(serde_json::Value::as_str)
            == Some(stored_target_fingerprint.as_str())
        && snapshot
            .pointer("/source/snapshotId")
            .and_then(serde_json::Value::as_str)
            == Some(recovery_point_id.as_str())
        && snapshot_replica_id == source_replica_id.as_deref()
        && snapshot
            .get("purpose")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("incident_recovery")
            == purpose
        && (purpose != "restore_drill"
            || (snapshot
                .get("restoreTestTarget")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
                && snapshot
                    .get("targetRole")
                    .and_then(serde_json::Value::as_str)
                    == Some("restore_test")))
        && snapshot
            .get("evaluatedAtMs")
            .and_then(serde_json::Value::as_i64)
            .is_some_and(|value| value <= authorization_at_ms)
        && snapshot
            .get("expiresAtMs")
            .and_then(serde_json::Value::as_i64)
            .is_some_and(|value| value > authorization_at_ms);
    if !binding_valid {
        return Ok(false);
    }
    let Some(source_replica_id) = source_replica_id.as_deref() else {
        return Ok(false);
    };

    type SourceEligibilityRow = (String, String, String, String, i64, Option<i64>, String);
    let source: Option<SourceEligibilityRow> = transaction
        .query_row(
            "SELECT recovery.state, recovery.signature_status,
                    projection.restore_eligibility, recovery.engine,
                    recovery.last_ingested_at_ms, recovery.plaintext_bytes,
                    projection.artifact_completeness
             FROM catalog_recovery_points AS recovery
             JOIN catalog_snapshot_projection AS projection
               ON projection.recovery_point_id = recovery.id
             WHERE recovery.id = ?1",
            [recovery_point_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .optional()?;
    let Some((
        source_state,
        signature_status,
        restore_eligibility,
        source_engine,
        last_ingested_at_ms,
        plaintext_bytes,
        artifact_completeness,
    )) = source
    else {
        return Ok(false);
    };
    let source_valid = source_state == "complete"
        && signature_status == "valid"
        && artifact_completeness == "complete"
        && matches!(restore_eligibility.as_str(), "eligible" | "conditional");
    let replica_valid: bool = transaction.query_row(
        "SELECT EXISTS(
             SELECT 1
             FROM catalog_artifact_replicas
             WHERE id = ?1
               AND recovery_point_id = ?2
               AND requirement = 'required'
               AND state = 'committed'
               AND reader_status = 'eligible'
         )",
        params![source_replica_id, recovery_point_id],
        |row| row.get(0),
    )?;

    type RestoreTargetRow = (
        String,
        String,
        bool,
        Option<String>,
        Option<String>,
        Option<i64>,
        String,
        Option<String>,
    );
    let target: Option<RestoreTargetRow> = transaction
        .query_row(
            "SELECT profile.engine, profile.state,
                    EXISTS(
                        SELECT 1
                        FROM connection_secret_bindings AS binding
                        JOIN secret_refs AS secret
                          ON secret.id = binding.secret_ref_id
                        WHERE binding.connection_profile_id = profile.id
                          AND binding.secret_role = 'database_auth'
                          AND secret.state = 'active'
                    ),
                    probe.status, probe.server_version, probe.completed_at_ms,
                    profile.adapter_config_json, probe.observed_capabilities_json
             FROM connection_profiles AS profile
             LEFT JOIN connection_probe_runs AS probe
               ON probe.id = (
                    SELECT candidate.id
                    FROM connection_probe_runs AS candidate
                    WHERE candidate.connection_profile_id = profile.id
                    ORDER BY candidate.started_at_ms DESC, candidate.id DESC
                    LIMIT 1
               )
             WHERE profile.id = ?1
               AND profile.deleted_at_ms IS NULL",
            [target_connection_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )
        .optional()?;
    let Some((
        target_engine,
        target_state,
        credential_bound,
        probe_status,
        server_version,
        probed_at_ms,
        adapter_config_json,
        observed_capabilities_json,
    )) = target
    else {
        return Ok(false);
    };
    let latest_discovery_id: Option<String> = transaction
        .query_row(
            "SELECT id
             FROM discovery_runs
             WHERE connection_profile_id = ?1
               AND status = 'succeeded'
             ORDER BY completed_at_ms DESC, id DESC
             LIMIT 1",
            [target_connection_id],
            |row| row.get(0),
        )
        .optional()?;
    let catalog_target_exists = latest_discovery_id
        .map(|discovery_id| {
            transaction.query_row(
                "SELECT EXISTS(
                     SELECT 1
                     FROM discovered_databases
                     WHERE discovery_run_id = ?1
                       AND normalized_name = LOWER(?2)
                 )",
                params![discovery_id, target_database_name],
                |row| row.get::<_, bool>(0),
            )
        })
        .transpose()?;
    let (available_spool_bytes, spool_observed_at_ms): (Option<i64>, Option<i64>) = transaction
        .query_row(
            "SELECT
                 MAX(
                     CASE
                         WHEN last_free_bytes > reserve_floor_bytes
                         THEN last_free_bytes - reserve_floor_bytes
                         ELSE 0
                     END
                 ),
                 MAX(last_probed_at_ms)
             FROM spool_volumes
             WHERE state IN ('ready', 'throttled')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
    let certified_capability_count: i64 = transaction.query_row(
        "SELECT COUNT(DISTINCT capability.capability)
         FROM engine_certifications AS certification
         JOIN engine_certification_capabilities AS capability
           ON capability.certification_id = certification.id
         WHERE certification.engine = ?1
           AND certification.status = 'certified'
           AND certification.certified_at_ms <= ?2
           AND (
               certification.expires_at_ms IS NULL
               OR certification.expires_at_ms > ?2
           )
           AND capability.capability IN ('restore_create_new', 'validation')
           AND capability.support_status IN ('passed', 'limited')",
        params![source_engine, current_at_ms],
        |row| row.get(0),
    )?;
    let expected_tool = restore_tool_for_engine_runtime(&source_engine);
    let trusted_tool = if source_engine == "sqlite" {
        true
    } else {
        match expected_tool {
            Some(expected_tool) => transaction.query_row(
                "SELECT EXISTS(
                 SELECT 1
                 FROM native_tool_inventory
                 WHERE tool_name = ?1
                   AND trust_status = 'trusted'
                   AND signature_status = 'valid'
                   AND deleted_at_ms IS NULL
             )",
                [expected_tool],
                |row| row.get::<_, bool>(0),
            )?,
            None => false,
        }
    };
    let required_spool_bytes = plaintext_bytes
        .filter(|value| *value > 0)
        .and_then(|value| value.checked_mul(3));
    let space_valid = matches!(
        (required_spool_bytes, available_spool_bytes),
        (Some(required), Some(available)) if available >= required
    );
    let destructive_live_observation = live.target_exists.is_some();
    let effective_server_version = live.server_version.or(server_version.as_deref());
    let catalog_server_identity = observed_capabilities_json
        .as_deref()
        .and_then(|json| server_identity_from_capabilities(&target_engine, json));
    let effective_server_identity = match live.server_identity {
        Some(identity) => canonical_server_identity(&target_engine, identity),
        None => catalog_server_identity,
    };
    let snapshot_server_identity = snapshot
        .get("serverIdentity")
        .and_then(serde_json::Value::as_str)
        .and_then(|identity| canonical_server_identity(&target_engine, identity));
    let target_exists = live.target_exists.or(catalog_target_exists);
    let probe_valid = destructive_live_observation
        || matches!(
            (probe_status.as_deref(), probed_at_ms),
            (Some("passed"), Some(observed))
                if observed <= current_at_ms && current_at_ms - observed <= 5 * 60 * 1_000
        );
    let server_valid = effective_server_version.is_some_and(|version| !version.trim().is_empty());
    let server_identity_valid = source_engine == "sqlite"
        || (effective_server_identity.is_some()
            && effective_server_identity == snapshot_server_identity);
    let existence_valid = matches!(
        (expected_mode, target_exists),
        ("create_new", Some(false)) | ("overwrite", Some(true))
    );
    let engine_policy_valid = target_engine == source_engine
        && !is_protected_database_name_runtime(&source_engine, target_database_name);
    let restore_test_target = serde_json::from_str::<serde_json::Value>(&adapter_config_json)
        .ok()
        .and_then(|value| {
            value
                .get("restoreTestTarget")
                .and_then(serde_json::Value::as_bool)
        })
        .unwrap_or(false);
    let restore_target_role_valid = purpose != "restore_drill" || restore_test_target;
    let confirmation_valid = match (expected_mode, confirmation_token_id.as_deref()) {
        ("create_new", None) => true,
        ("overwrite", Some(token_id)) => transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1
                 FROM confirmation_tokens AS token
                 JOIN ipc_request_ledger AS request
                   ON request.request_id = token.consumed_by_request_id
                 WHERE token.id = ?1
                   AND token.state = 'consumed'
                   AND token.action = 'restore_overwrite'
                   AND token.resource_type = 'restore_target'
                   AND token.resource_id = ?2
                   AND token.resource_fingerprint = ?3
                   AND request.state = 'succeeded'
                   AND request.result_ref_id IS NOT NULL
             )",
            params![token_id, target_database_name, stored_target_fingerprint],
            |row| row.get::<_, bool>(0),
        )?,
        _ => false,
    };
    let currently_eligible = source_valid
        && replica_valid
        && target_state == "ready"
        && (source_engine == "sqlite" || credential_bound)
        && probe_valid
        && server_valid
        && server_identity_valid
        && existence_valid
        && engine_policy_valid
        && restore_target_role_valid
        && (destructive_live_observation || space_valid)
        && certified_capability_count == 2
        && trusted_tool
        && confirmation_valid;
    let target_fingerprint = restore_target_fingerprint_runtime(
        target_connection_id,
        &target_engine,
        target_database_name,
        effective_server_version,
        effective_server_identity.as_deref(),
    );
    let preflight_version = blake3::hash(
        format!(
            "restore-preflight-v1\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}",
            recovery_point_id,
            last_ingested_at_ms,
            source_replica_id,
            target_fingerprint,
            expected_mode,
            probed_at_ms.unwrap_or(-1),
            spool_observed_at_ms.unwrap_or(-1),
            catalog_target_exists.map(i64::from).unwrap_or(-1),
            currently_eligible
        )
        .as_bytes(),
    )
    .to_hex()
    .to_string();
    let snapshot_preflight_version = snapshot
        .get("preflightVersion")
        .and_then(serde_json::Value::as_str);
    let snapshot_target_exists = snapshot
        .get("targetExists")
        .and_then(serde_json::Value::as_bool);
    let run_matches = transaction.query_row(
        "SELECT EXISTS(
             SELECT 1
             FROM work_items
             WHERE restore_run_id = ?1
               AND kind = 'restore'
               AND state IN ('leased', 'starting', 'running')
         )",
        [restore_run_id],
        |row| row.get::<_, bool>(0),
    )?;
    Ok(currently_eligible
        && run_matches
        && target_fingerprint == *stored_target_fingerprint
        && (destructive_live_observation
            || snapshot_preflight_version == Some(preflight_version.as_str()))
        && snapshot_target_exists == target_exists)
}

fn restore_target_fingerprint_runtime(
    connection_id: &str,
    engine: &str,
    target_name: &str,
    server_version: Option<&str>,
    server_identity: Option<&str>,
) -> String {
    let identity = if engine == "sqlite" {
        "embedded-sqlite"
    } else {
        server_identity.unwrap_or("unavailable")
    };
    blake3::hash(
        format!(
            "restore-target-v2\0{connection_id}\0{engine}\0{}\0{}\0{identity}",
            target_name.to_ascii_lowercase(),
            server_version.unwrap_or("unknown")
        )
        .as_bytes(),
    )
    .to_hex()
    .to_string()
}

struct RestoreTerminalProjection {
    state: &'static str,
}

fn restore_terminal_projection(
    evidence_json: &str,
    outcome: ClaimTerminalOutcome,
) -> Result<RestoreTerminalProjection, RepositoryError> {
    let evidence: serde_json::Value =
        serde_json::from_str(evidence_json).map_err(|_| RepositoryError::RuntimeStateInvalid)?;
    if evidence
        .get("schemaVersion")
        .and_then(serde_json::Value::as_i64)
        != Some(1)
        || !matches!(
            evidence.get("phase").and_then(serde_json::Value::as_str),
            Some("preflight" | "restore" | "validation" | "cancelled" | "completed" | "reconcile")
        )
    {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    let target_created = evidence
        .pointer("/restore/targetCreated")
        .and_then(serde_json::Value::as_bool)
        .ok_or(RepositoryError::RuntimeStateInvalid)?;
    let native_exit_code = match evidence.pointer("/restore/nativeExitCode") {
        Some(serde_json::Value::Null) => None,
        Some(value) => value.as_i64(),
        None => return Err(RepositoryError::RuntimeStateInvalid),
    };
    let validation_status = evidence
        .pointer("/validation/status")
        .and_then(serde_json::Value::as_str)
        .ok_or(RepositoryError::RuntimeStateInvalid)?;
    if !matches!(validation_status, "passed" | "failed" | "not_run") {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    let cleanup_required = evidence
        .pointer("/cleanup/required")
        .and_then(serde_json::Value::as_bool)
        .ok_or(RepositoryError::RuntimeStateInvalid)?;
    let target_preserved = evidence
        .pointer("/cleanup/targetPreserved")
        .and_then(serde_json::Value::as_bool)
        .ok_or(RepositoryError::RuntimeStateInvalid)?;
    match outcome {
        ClaimTerminalOutcome::Succeeded
            if target_created
                && native_exit_code == Some(0)
                && validation_status == "passed"
                && !cleanup_required
                && target_preserved =>
        {
            Ok(RestoreTerminalProjection { state: "succeeded" })
        }
        ClaimTerminalOutcome::Succeeded => Err(RepositoryError::RuntimeStateInvalid),
        ClaimTerminalOutcome::Failed
        | ClaimTerminalOutcome::PartiallySucceeded
        | ClaimTerminalOutcome::Cancelled => {
            if target_created {
                if !cleanup_required || !target_preserved {
                    return Err(RepositoryError::RuntimeStateInvalid);
                }
                Ok(RestoreTerminalProjection {
                    state: "cleanup_required",
                })
            } else {
                if cleanup_required || target_preserved || validation_status == "passed" {
                    return Err(RepositoryError::RuntimeStateInvalid);
                }
                Ok(RestoreTerminalProjection {
                    state: if outcome == ClaimTerminalOutcome::Cancelled {
                        "cancelled"
                    } else {
                        "failed"
                    },
                })
            }
        }
    }
}

fn restore_tool_for_engine_runtime(engine: &str) -> Option<&'static str> {
    match engine {
        "postgresql" => Some("pg_restore"),
        "mysql" => Some("mysql"),
        "mariadb" => Some("mariadb"),
        "sqlserver" | "sqlite" => None,
        _ => None,
    }
}

fn is_protected_database_name_runtime(engine: &str, name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    match engine {
        "postgresql" => matches!(name.as_str(), "postgres" | "template0" | "template1"),
        "mysql" | "mariadb" => matches!(
            name.as_str(),
            "mysql" | "information_schema" | "performance_schema" | "sys"
        ),
        "sqlserver" => matches!(name.as_str(), "master" | "model" | "msdb" | "tempdb"),
        _ => false,
    }
}

fn record_admission_wait(
    transaction: &Transaction<'_>,
    claim: &ClaimedWork,
    now_ms: i64,
    reason: &str,
) -> Result<(), RepositoryError> {
    let affected = transaction.execute(
        "UPDATE work_items
         SET wait_reason = ?7, updated_at_ms = ?8
         WHERE id = ?1
           AND current_attempt_no = ?2
           AND fencing_token = ?3
           AND lease_owner = ?4
           AND lease_boot_id = ?5
           AND state = 'leased'
           AND lease_expires_at_ms >= ?6",
        params![
            claim.work_item_id.as_str(),
            claim.attempt_no,
            claim.fencing_token,
            claim.worker_id,
            claim.worker_boot_id,
            now_ms,
            reason,
            now_ms
        ],
    )?;
    if affected != 1 {
        return Err(RepositoryError::StaleFence);
    }
    Ok(())
}

fn release_claim_capacity(
    transaction: &Transaction<'_>,
    claim: &ClaimedWork,
    released_at_ms: i64,
    release_reason: &str,
    release_quarantine: bool,
) -> Result<(), RepositoryError> {
    let mismatched_allocations: i64 = transaction.query_row(
        "SELECT COUNT(*)
         FROM resource_allocations
         WHERE work_attempt_id = ?1
           AND released_at_ms IS NULL
           AND fencing_token <> ?2",
        params![claim.attempt_id.as_str(), claim.fencing_token],
        |row| row.get(0),
    )?;
    let mismatched_spool: i64 = transaction.query_row(
        "SELECT COUNT(*)
         FROM spool_reservations
         WHERE work_attempt_id = ?1
           AND released_at_ms IS NULL
           AND fencing_token <> ?2",
        params![claim.attempt_id.as_str(), claim.fencing_token],
        |row| row.get(0),
    )?;
    if mismatched_allocations != 0 || mismatched_spool != 0 {
        return Err(RepositoryError::StaleFence);
    }

    transaction.execute(
        "UPDATE resource_allocations
         SET released_at_ms = ?3, release_reason = ?4
         WHERE work_attempt_id = ?1
           AND fencing_token = ?2
           AND released_at_ms IS NULL",
        params![
            claim.attempt_id.as_str(),
            claim.fencing_token,
            released_at_ms,
            release_reason
        ],
    )?;

    transaction.execute(
        "UPDATE spool_reservations
         SET state = 'released', released_at_ms = ?3
         WHERE work_attempt_id = ?1
           AND fencing_token = ?2
           AND released_at_ms IS NULL
           AND (
               state IN ('reserved', 'materializing', 'ready')
               OR (state = 'quarantined' AND ?4 = 1)
           )",
        params![
            claim.attempt_id.as_str(),
            claim.fencing_token,
            released_at_ms,
            i64::from(release_quarantine)
        ],
    )?;
    Ok(())
}

fn renew_claim_allocations(
    transaction: &Transaction<'_>,
    claim: &ClaimedWork,
    lease_expires_at_ms: i64,
) -> Result<(), RepositoryError> {
    let mismatched_allocations: i64 = transaction.query_row(
        "SELECT COUNT(*)
         FROM resource_allocations
         WHERE work_attempt_id = ?1
           AND released_at_ms IS NULL
           AND fencing_token <> ?2",
        params![claim.attempt_id.as_str(), claim.fencing_token],
        |row| row.get(0),
    )?;
    if mismatched_allocations != 0 {
        return Err(RepositoryError::StaleFence);
    }
    transaction.execute(
        "UPDATE resource_allocations
         SET expires_at_ms = ?3
         WHERE work_attempt_id = ?1
           AND fencing_token = ?2
           AND released_at_ms IS NULL",
        params![
            claim.attempt_id.as_str(),
            claim.fencing_token,
            lease_expires_at_ms
        ],
    )?;
    Ok(())
}

fn waiting_admission(reason: String) -> AdmissionOutcome {
    AdmissionOutcome {
        admitted: false,
        wait_reason: Some(reason),
        allocation_count: 0,
        spool_reserved: false,
    }
}

fn insert_or_validate_allocation(
    transaction: &Transaction<'_>,
    claim: &ClaimedWork,
    pool: &ResourcePoolRow,
    acquired_at_ms: i64,
    expires_at_ms: i64,
) -> Result<(), RepositoryError> {
    let existing: Option<(i64, i64)> = transaction
        .query_row(
            "SELECT units, fencing_token
             FROM resource_allocations
             WHERE resource_pool_id = ?1
               AND work_attempt_id = ?2
               AND released_at_ms IS NULL",
            params![pool.id, claim.attempt_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((units, fence)) = existing {
        if units != pool.units || fence != claim.fencing_token {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        return Ok(());
    }
    let allocation_id =
        stable_runtime_row_id("resource-allocation", claim.attempt_id.as_str(), &pool.id);
    transaction.execute(
        "INSERT INTO resource_allocations(
             id, resource_pool_id, work_attempt_id, units, fencing_token,
             acquired_at_ms, expires_at_ms, released_at_ms, release_reason,
             created_at_ms
         ) VALUES (
             ?1, ?2, ?3, ?4, ?5,
             ?6, ?7, NULL, NULL,
             ?6
         )",
        params![
            allocation_id,
            pool.id,
            claim.attempt_id.as_str(),
            pool.units,
            claim.fencing_token,
            acquired_at_ms,
            expires_at_ms
        ],
    )?;
    Ok(())
}

fn insert_or_validate_spool_reservation(
    transaction: &Transaction<'_>,
    claim: &ClaimedWork,
    spool: &SpoolRequirement,
    volume: &SpoolVolumeRow,
    acquired_at_ms: i64,
) -> Result<(), RepositoryError> {
    let existing: Option<(String, i64, String, i64)> = transaction
        .query_row(
            "SELECT spool_volume_id, reserved_bytes, spool_path, fencing_token
             FROM spool_reservations
             WHERE work_attempt_id = ?1
               AND released_at_ms IS NULL",
            [claim.attempt_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    if let Some((volume_id, bytes, path, fence)) = existing {
        if volume_id != volume.id
            || bytes != spool.reserved_bytes
            || path != spool.spool_path
            || fence != claim.fencing_token
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        return Ok(());
    }
    let reservation_id =
        stable_runtime_row_id("spool-reservation", claim.attempt_id.as_str(), &volume.id);
    transaction.execute(
        "INSERT INTO spool_reservations(
             id, spool_volume_id, work_attempt_id, reserved_bytes, actual_bytes,
             fencing_token, spool_path, state, acquired_at_ms, released_at_ms,
             cleanup_error, created_at_ms
         ) VALUES (
             ?1, ?2, ?3, ?4, NULL,
             ?5, ?6, 'reserved', ?7, NULL,
             NULL, ?7
         )",
        params![
            reservation_id,
            volume.id,
            claim.attempt_id.as_str(),
            spool.reserved_bytes,
            claim.fencing_token,
            spool.spool_path,
            acquired_at_ms
        ],
    )?;
    Ok(())
}

fn stable_runtime_row_id(kind: &str, attempt_id: &str, subject_id: &str) -> String {
    const RUNTIME_NAMESPACE: Uuid = Uuid::from_u128(0x7bce_80ef_ebea_5632_a062_251a_7f12_fcc5);
    Uuid::new_v5(
        &RUNTIME_NAMESPACE,
        format!("{kind}|{attempt_id}|{subject_id}").as_bytes(),
    )
    .to_string()
}
