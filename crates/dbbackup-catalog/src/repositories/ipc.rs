use std::str::FromStr;

use dbbackup_domain::{
    ArtifactReplicaId, BackupPlanId, BackupRunId, BackupScopeId, ConfirmationTokenId,
    ConnectionProfileId, DatabaseTargetId, DestinationId, IpcRequestId, OccurrenceIdentity,
    OperationRunId, PlanOccurrenceId, PlanRevisionId, RecoveryPointId, RestoreRunId,
    VerificationRunId, WorkItemId, WorkspaceId,
};
use dbbackup_pipeline::{RetentionDeleteAuthorizationV2, RetentionDispositionV2};
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};

use crate::{OperationalStore, DEFAULT_WORKSPACE_ID};

use super::catalog_projection::project_terminal_verification_run;
use super::configuration::resolve_replica_repair_secret_refs_in_transaction;
use super::cutover::{
    ensure_cutover_not_committed, expected_response_json, insert_cutover_state, load_authority,
    ConfigCutoverResult, ConfigCutoverSpec, PersistedAuthoritySnapshot,
};
use super::recovery::{
    ensure_restore_source_target_in_transaction, insert_safe_restore_in_transaction,
    SafeRestoreSpec,
};
use super::retention_planner::{
    apply_safety_control, prepare_delete_plan_in_transaction, RetentionSafetyControlSpec,
};
use super::runtime::{insert_operation_work_in_transaction, ReplicaRepairWorkSpec};
use super::shadow_import::verify_shadow_import_for_cutover;
use super::RepositoryError;

const MAX_STORED_RESPONSE_BYTES: usize = 1024 * 1024;
type ExistingLedgerRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);
type ExistingExpirableLedgerRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
);

#[derive(Clone, Debug)]
pub struct IpcRequestSpec {
    request_id: IpcRequestId,
    command_name: String,
    client_sid_hash: String,
    idempotency_key: String,
    payload_digest_blake3: String,
    accepted_at_ms: i64,
    expires_at_ms: i64,
}

impl IpcRequestSpec {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        request_id: IpcRequestId,
        command_name: impl Into<String>,
        client_sid_hash: impl Into<String>,
        idempotency_key: impl Into<String>,
        payload_digest_blake3: impl Into<String>,
        accepted_at_ms: i64,
        expires_at_ms: i64,
    ) -> Self {
        Self {
            request_id,
            command_name: command_name.into(),
            client_sid_hash: client_sid_hash.into(),
            idempotency_key: idempotency_key.into(),
            payload_digest_blake3: payload_digest_blake3.into(),
            accepted_at_ms,
            expires_at_ms,
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if self.command_name.is_empty()
            || self.command_name.len() > 128
            || self.idempotency_key.is_empty()
            || self.idempotency_key.len() > 256
            || !is_digest(&self.client_sid_hash)
            || !is_digest(&self.payload_digest_blake3)
            || self.accepted_at_ms < 0
            || self.expires_at_ms <= self.accepted_at_ms
        {
            return Err(RepositoryError::SecurityInputInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ConfirmationSpec {
    id: ConfirmationTokenId,
    token_digest_blake3: String,
    action: String,
    client_sid_hash: String,
    resource_type: String,
    resource_id: String,
    resource_fingerprint: String,
    payload_digest_blake3: String,
    issued_at_ms: i64,
    expires_at_ms: i64,
}

impl ConfirmationSpec {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: ConfirmationTokenId,
        token_digest_blake3: impl Into<String>,
        action: impl Into<String>,
        client_sid_hash: impl Into<String>,
        resource_type: impl Into<String>,
        resource_id: impl Into<String>,
        resource_fingerprint: impl Into<String>,
        payload_digest_blake3: impl Into<String>,
        issued_at_ms: i64,
        expires_at_ms: i64,
    ) -> Self {
        Self {
            id,
            token_digest_blake3: token_digest_blake3.into(),
            action: action.into(),
            client_sid_hash: client_sid_hash.into(),
            resource_type: resource_type.into(),
            resource_id: resource_id.into(),
            resource_fingerprint: resource_fingerprint.into(),
            payload_digest_blake3: payload_digest_blake3.into(),
            issued_at_ms,
            expires_at_ms,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn restore_overwrite(
        id: ConfirmationTokenId,
        token_digest_blake3: impl Into<String>,
        client_sid_hash: impl Into<String>,
        resource_type: impl Into<String>,
        resource_id: impl Into<String>,
        resource_fingerprint: impl Into<String>,
        payload_digest_blake3: impl Into<String>,
        issued_at_ms: i64,
        expires_at_ms: i64,
    ) -> Self {
        Self::new(
            id,
            token_digest_blake3,
            "restore_overwrite",
            client_sid_hash,
            resource_type,
            resource_id,
            resource_fingerprint,
            payload_digest_blake3,
            issued_at_ms,
            expires_at_ms,
        )
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if !is_digest(&self.token_digest_blake3)
            || !is_confirmation_action(&self.action)
            || !is_digest(&self.client_sid_hash)
            || !is_digest(&self.payload_digest_blake3)
            || self.resource_type.is_empty()
            || self.resource_type.len() > 128
            || self.resource_id.is_empty()
            || self.resource_id.len() > 512
            || self.resource_fingerprint.is_empty()
            || self.resource_fingerprint.len() > 512
            || self.issued_at_ms < 0
            || self.expires_at_ms <= self.issued_at_ms
        {
            return Err(RepositoryError::SecurityInputInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfirmationRecord {
    id: ConfirmationTokenId,
    token_digest_blake3: String,
    action: String,
    client_sid_hash: String,
    resource_type: String,
    resource_id: String,
    resource_fingerprint: String,
    payload_digest_blake3: String,
    state: String,
    issued_at_ms: i64,
    expires_at_ms: i64,
}

#[derive(Clone, Debug)]
pub struct RetentionConfirmationRequestSpec {
    confirmation_token_id: ConfirmationTokenId,
    token_digest_blake3: String,
    recovery_point_id: RecoveryPointId,
    destination_fingerprint: String,
    grace_period_ms: i64,
    expires_at_ms: i64,
}

impl RetentionConfirmationRequestSpec {
    pub fn new(
        confirmation_token_id: ConfirmationTokenId,
        token_digest_blake3: impl Into<String>,
        recovery_point_id: RecoveryPointId,
        destination_fingerprint: impl Into<String>,
        grace_period_ms: i64,
        expires_at_ms: i64,
    ) -> Self {
        Self {
            confirmation_token_id,
            token_digest_blake3: token_digest_blake3.into(),
            recovery_point_id,
            destination_fingerprint: destination_fingerprint.into(),
            grace_period_ms,
            expires_at_ms,
        }
    }

    fn validate(&self, issued_at_ms: i64) -> Result<(), RepositoryError> {
        if !is_digest(&self.token_digest_blake3)
            || self.destination_fingerprint.is_empty()
            || self.destination_fingerprint.len() > 512
            || self.destination_fingerprint.split_once(':').is_none()
            || self.grace_period_ms <= 0
            || self.expires_at_ms <= issued_at_ms
        {
            return Err(RepositoryError::SecurityInputInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct RetentionConfirmationIssueResult {
    created: bool,
    confirmation_token_id: ConfirmationTokenId,
    authorization: RetentionDeleteAuthorizationV2,
    issued_at_ms: i64,
    expires_at_ms: i64,
}

#[derive(Clone, Debug)]
pub struct RestoreOverwriteConfirmationRequestSpec {
    confirmation_token_id: ConfirmationTokenId,
    token_digest_blake3: String,
    recovery_point_id: RecoveryPointId,
    source_replica_id: String,
    target_connection_profile_id: ConnectionProfileId,
    target_database_name: String,
    target_fingerprint: String,
    preflight_version: String,
    required_verification_level: i64,
    payload_digest_blake3: String,
    expires_at_ms: i64,
}

impl RestoreOverwriteConfirmationRequestSpec {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        confirmation_token_id: ConfirmationTokenId,
        token_digest_blake3: impl Into<String>,
        recovery_point_id: RecoveryPointId,
        source_replica_id: impl Into<String>,
        target_connection_profile_id: ConnectionProfileId,
        target_database_name: impl Into<String>,
        target_fingerprint: impl Into<String>,
        preflight_version: impl Into<String>,
        required_verification_level: i64,
        payload_digest_blake3: impl Into<String>,
        expires_at_ms: i64,
    ) -> Self {
        Self {
            confirmation_token_id,
            token_digest_blake3: token_digest_blake3.into(),
            recovery_point_id,
            source_replica_id: source_replica_id.into(),
            target_connection_profile_id,
            target_database_name: target_database_name.into(),
            target_fingerprint: target_fingerprint.into(),
            preflight_version: preflight_version.into(),
            required_verification_level,
            payload_digest_blake3: payload_digest_blake3.into(),
            expires_at_ms,
        }
    }

    fn validate(&self, issued_at_ms: i64) -> Result<(), RepositoryError> {
        if !is_digest(&self.token_digest_blake3)
            || self.source_replica_id.is_empty()
            || self.source_replica_id.len() > 256
            || invalid_text(&self.source_replica_id)
            || self.target_database_name.is_empty()
            || self.target_database_name.len() > 128
            || invalid_text(&self.target_database_name)
            || !is_digest(&self.target_fingerprint)
            || !is_digest(&self.preflight_version)
            || !(1..=3).contains(&self.required_verification_level)
            || !is_digest(&self.payload_digest_blake3)
            || self.expires_at_ms <= issued_at_ms
        {
            return Err(RepositoryError::SecurityInputInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct RestoreOverwriteConfirmationIssueResult {
    created: bool,
    confirmation_token_id: ConfirmationTokenId,
    recovery_point_id: RecoveryPointId,
    source_replica_id: String,
    target_connection_profile_id: ConnectionProfileId,
    target_database_name: String,
    target_fingerprint: String,
    preflight_version: String,
    required_verification_level: i64,
    payload_digest_blake3: String,
    issued_at_ms: i64,
    expires_at_ms: i64,
}

impl RestoreOverwriteConfirmationIssueResult {
    pub const fn created(&self) -> bool {
        self.created
    }

    pub const fn confirmation_token_id(&self) -> &ConfirmationTokenId {
        &self.confirmation_token_id
    }

    pub const fn recovery_point_id(&self) -> &RecoveryPointId {
        &self.recovery_point_id
    }

    pub fn source_replica_id(&self) -> &str {
        &self.source_replica_id
    }

    pub const fn target_connection_profile_id(&self) -> &ConnectionProfileId {
        &self.target_connection_profile_id
    }

    pub fn target_database_name(&self) -> &str {
        &self.target_database_name
    }

    pub fn target_fingerprint(&self) -> &str {
        &self.target_fingerprint
    }

    pub fn preflight_version(&self) -> &str {
        &self.preflight_version
    }

    pub const fn required_verification_level(&self) -> i64 {
        self.required_verification_level
    }

    pub fn payload_digest_blake3(&self) -> &str {
        &self.payload_digest_blake3
    }

    pub const fn issued_at_ms(&self) -> i64 {
        self.issued_at_ms
    }

    pub const fn expires_at_ms(&self) -> i64 {
        self.expires_at_ms
    }
}

#[derive(Clone, Debug)]
pub struct ReplicaRepairRequestSpec {
    recovery_point_id: RecoveryPointId,
    source_replica_id: ArtifactReplicaId,
    target_destination_id: DestinationId,
    available_at_ms: i64,
}

impl ReplicaRepairRequestSpec {
    pub fn new(
        recovery_point_id: RecoveryPointId,
        source_replica_id: ArtifactReplicaId,
        target_destination_id: DestinationId,
        available_at_ms: i64,
    ) -> Self {
        Self {
            recovery_point_id,
            source_replica_id,
            target_destination_id,
            available_at_ms,
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if self.available_at_ms < 0 {
            return Err(RepositoryError::SecurityInputInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ReplicaRepairEnqueueResult {
    created: bool,
    operation_run_id: OperationRunId,
    work_item_id: WorkItemId,
    activity_after_seq: i64,
}

impl ReplicaRepairEnqueueResult {
    pub const fn created(&self) -> bool {
        self.created
    }

    pub const fn operation_run_id(&self) -> &OperationRunId {
        &self.operation_run_id
    }

    pub const fn work_item_id(&self) -> &WorkItemId {
        &self.work_item_id
    }

    pub const fn activity_after_seq(&self) -> i64 {
        self.activity_after_seq
    }
}

#[derive(Clone, Debug)]
pub struct ManualBackupRequestSpec {
    workspace_id: WorkspaceId,
    plan_id: BackupPlanId,
    requested_at_ms: i64,
}

impl ManualBackupRequestSpec {
    pub const fn new(
        workspace_id: WorkspaceId,
        plan_id: BackupPlanId,
        requested_at_ms: i64,
    ) -> Self {
        Self {
            workspace_id,
            plan_id,
            requested_at_ms,
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if self.requested_at_ms < 0 {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ManualBackupWorkItem {
    backup_run_id: BackupRunId,
    work_item_id: WorkItemId,
    target_id: DatabaseTargetId,
    target_name: String,
    scope_id: BackupScopeId,
    scope_name: String,
    scope_mode: String,
    engine: String,
}

impl ManualBackupWorkItem {
    pub const fn backup_run_id(&self) -> &BackupRunId {
        &self.backup_run_id
    }

    pub const fn work_item_id(&self) -> &WorkItemId {
        &self.work_item_id
    }

    pub const fn target_id(&self) -> &DatabaseTargetId {
        &self.target_id
    }

    pub fn target_name(&self) -> &str {
        &self.target_name
    }

    pub const fn scope_id(&self) -> &BackupScopeId {
        &self.scope_id
    }

    pub fn scope_name(&self) -> &str {
        &self.scope_name
    }

    pub fn scope_mode(&self) -> &str {
        &self.scope_mode
    }

    pub fn engine(&self) -> &str {
        &self.engine
    }
}

#[derive(Clone, Debug)]
pub struct ManualBackupEnqueueResult {
    created: bool,
    plan_id: BackupPlanId,
    plan_revision_id: PlanRevisionId,
    plan_occurrence_id: PlanOccurrenceId,
    plan_name: String,
    work_items: Vec<ManualBackupWorkItem>,
    activity_after_seq: i64,
}

impl ManualBackupEnqueueResult {
    pub const fn created(&self) -> bool {
        self.created
    }

    pub const fn plan_id(&self) -> &BackupPlanId {
        &self.plan_id
    }

    pub const fn plan_revision_id(&self) -> &PlanRevisionId {
        &self.plan_revision_id
    }

    pub const fn plan_occurrence_id(&self) -> &PlanOccurrenceId {
        &self.plan_occurrence_id
    }

    pub fn plan_name(&self) -> &str {
        &self.plan_name
    }

    pub fn work_items(&self) -> &[ManualBackupWorkItem] {
        &self.work_items
    }

    pub const fn activity_after_seq(&self) -> i64 {
        self.activity_after_seq
    }
}

#[derive(Clone, Debug)]
pub struct ManualVerificationRequestSpec {
    verification_run_id: VerificationRunId,
    work_item_id: WorkItemId,
    recovery_point_id: RecoveryPointId,
    source_replica_id: Option<ArtifactReplicaId>,
    requested_level: i64,
    available_at_ms: i64,
}

impl ManualVerificationRequestSpec {
    pub fn new(
        verification_run_id: VerificationRunId,
        work_item_id: WorkItemId,
        recovery_point_id: RecoveryPointId,
        source_replica_id: Option<ArtifactReplicaId>,
        requested_level: i64,
        available_at_ms: i64,
    ) -> Self {
        Self {
            verification_run_id,
            work_item_id,
            recovery_point_id,
            source_replica_id,
            requested_level,
            available_at_ms,
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if !(1..=3).contains(&self.requested_level) || self.available_at_ms < 0 {
            return Err(RepositoryError::VerificationInputInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ManualVerificationEnqueueResult {
    created: bool,
    verification_run_id: VerificationRunId,
    work_item_id: Option<WorkItemId>,
    state: String,
    requested_level: i64,
    source_replica_id: ArtifactReplicaId,
    support_status: Option<String>,
    limitation: Option<String>,
    error_code: Option<String>,
    activity_after_seq: i64,
}

impl ManualVerificationEnqueueResult {
    pub const fn created(&self) -> bool {
        self.created
    }

    pub const fn verification_run_id(&self) -> &VerificationRunId {
        &self.verification_run_id
    }

    pub const fn work_item_id(&self) -> Option<&WorkItemId> {
        self.work_item_id.as_ref()
    }

    pub fn state(&self) -> &str {
        &self.state
    }

    pub const fn requested_level(&self) -> i64 {
        self.requested_level
    }

    pub const fn source_replica_id(&self) -> &ArtifactReplicaId {
        &self.source_replica_id
    }

    pub fn support_status(&self) -> Option<&str> {
        self.support_status.as_deref()
    }

    pub fn limitation(&self) -> Option<&str> {
        self.limitation.as_deref()
    }

    pub fn error_code(&self) -> Option<&str> {
        self.error_code.as_deref()
    }

    pub const fn activity_after_seq(&self) -> i64 {
        self.activity_after_seq
    }
}

#[derive(Clone, Debug)]
pub struct ConfigCutoverConfirmationRequestSpec {
    confirmation_token_id: ConfirmationTokenId,
    token_digest_blake3: String,
    cutover_run_id: uuid::Uuid,
    shadow_import_id: uuid::Uuid,
    cutover_marker: String,
    payload_digest_blake3: String,
    expires_at_ms: i64,
}

impl ConfigCutoverConfirmationRequestSpec {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        confirmation_token_id: ConfirmationTokenId,
        token_digest_blake3: impl Into<String>,
        cutover_run_id: uuid::Uuid,
        shadow_import_id: uuid::Uuid,
        cutover_marker: impl Into<String>,
        payload_digest_blake3: impl Into<String>,
        expires_at_ms: i64,
    ) -> Self {
        Self {
            confirmation_token_id,
            token_digest_blake3: token_digest_blake3.into(),
            cutover_run_id,
            shadow_import_id,
            cutover_marker: cutover_marker.into(),
            payload_digest_blake3: payload_digest_blake3.into(),
            expires_at_ms,
        }
    }

    fn validate(&self, issued_at_ms: i64) -> Result<(), RepositoryError> {
        if !is_digest(&self.token_digest_blake3)
            || self.cutover_run_id.is_nil()
            || self.shadow_import_id.is_nil()
            || !is_cutover_marker(&self.cutover_marker)
            || !is_digest(&self.payload_digest_blake3)
            || self.expires_at_ms <= issued_at_ms
        {
            return Err(RepositoryError::CutoverInputInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ConfigCutoverConfirmationIssueResult {
    created: bool,
    confirmation_token_id: ConfirmationTokenId,
    cutover_run_id: uuid::Uuid,
    shadow_import_id: uuid::Uuid,
    cutover_marker: String,
    payload_digest_blake3: String,
    issued_at_ms: i64,
    expires_at_ms: i64,
}

impl ConfigCutoverConfirmationIssueResult {
    pub const fn created(&self) -> bool {
        self.created
    }

    pub const fn confirmation_token_id(&self) -> &ConfirmationTokenId {
        &self.confirmation_token_id
    }

    pub const fn cutover_run_id(&self) -> uuid::Uuid {
        self.cutover_run_id
    }

    pub const fn shadow_import_id(&self) -> uuid::Uuid {
        self.shadow_import_id
    }

    pub fn cutover_marker(&self) -> &str {
        &self.cutover_marker
    }

    pub fn payload_digest_blake3(&self) -> &str {
        &self.payload_digest_blake3
    }

    pub const fn issued_at_ms(&self) -> i64 {
        self.issued_at_ms
    }

    pub const fn expires_at_ms(&self) -> i64 {
        self.expires_at_ms
    }
}

impl RetentionConfirmationIssueResult {
    pub const fn created(&self) -> bool {
        self.created
    }

    pub const fn confirmation_token_id(&self) -> &ConfirmationTokenId {
        &self.confirmation_token_id
    }

    pub const fn authorization(&self) -> &RetentionDeleteAuthorizationV2 {
        &self.authorization
    }

    pub const fn issued_at_ms(&self) -> i64 {
        self.issued_at_ms
    }

    pub const fn expires_at_ms(&self) -> i64 {
        self.expires_at_ms
    }
}

impl ConfirmationRecord {
    pub const fn id(&self) -> &ConfirmationTokenId {
        &self.id
    }

    pub fn token_digest_blake3(&self) -> &str {
        &self.token_digest_blake3
    }

    pub fn action(&self) -> &str {
        &self.action
    }

    pub fn client_sid_hash(&self) -> &str {
        &self.client_sid_hash
    }

    pub fn resource_type(&self) -> &str {
        &self.resource_type
    }

    pub fn resource_id(&self) -> &str {
        &self.resource_id
    }

    pub fn resource_fingerprint(&self) -> &str {
        &self.resource_fingerprint
    }

    pub fn payload_digest_blake3(&self) -> &str {
        &self.payload_digest_blake3
    }

    pub fn state(&self) -> &str {
        &self.state
    }

    pub const fn issued_at_ms(&self) -> i64 {
        self.issued_at_ms
    }

    pub const fn expires_at_ms(&self) -> i64 {
        self.expires_at_ms
    }
}

#[derive(Clone, Debug)]
pub struct OverwriteRestoreSpec {
    restore_run_id: RestoreRunId,
    work_item_id: WorkItemId,
    recovery_point_id: RecoveryPointId,
    source_replica_id: String,
    target_connection_profile_id: ConnectionProfileId,
    target_database_name: String,
    target_fingerprint: String,
    preflight_version: String,
    required_verification_level: i64,
    eligibility_snapshot_json: String,
    created_at_ms: i64,
}

impl OverwriteRestoreSpec {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        restore_run_id: RestoreRunId,
        work_item_id: WorkItemId,
        recovery_point_id: RecoveryPointId,
        source_replica_id: impl Into<String>,
        target_connection_profile_id: ConnectionProfileId,
        target_database_name: impl Into<String>,
        target_fingerprint: impl Into<String>,
        preflight_version: impl Into<String>,
        required_verification_level: i64,
        eligibility_snapshot_json: impl Into<String>,
        created_at_ms: i64,
    ) -> Self {
        Self {
            restore_run_id,
            work_item_id,
            recovery_point_id,
            source_replica_id: source_replica_id.into(),
            target_connection_profile_id,
            target_database_name: target_database_name.into(),
            target_fingerprint: target_fingerprint.into(),
            preflight_version: preflight_version.into(),
            required_verification_level,
            eligibility_snapshot_json: eligibility_snapshot_json.into(),
            created_at_ms,
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if self.source_replica_id.is_empty()
            || self.source_replica_id.len() > 256
            || invalid_text(&self.source_replica_id)
            || self.target_database_name.is_empty()
            || self.target_database_name.len() > 128
            || invalid_text(&self.target_database_name)
            || !is_digest(&self.target_fingerprint)
            || !is_digest(&self.preflight_version)
            || !(1..=3).contains(&self.required_verification_level)
            || self.eligibility_snapshot_json.len() > 64 * 1024
            || self.created_at_ms < 0
        {
            return Err(RepositoryError::SecurityInputInvalid);
        }
        let snapshot: serde_json::Value = serde_json::from_str(&self.eligibility_snapshot_json)
            .map_err(|_| RepositoryError::SecurityInputInvalid)?;
        let valid_binding = snapshot
            .get("eligible")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
            && snapshot.get("mode").and_then(serde_json::Value::as_str) == Some("overwrite")
            && snapshot
                .get("preflightVersion")
                .and_then(serde_json::Value::as_str)
                == Some(self.preflight_version.as_str())
            && snapshot
                .get("targetFingerprint")
                .and_then(serde_json::Value::as_str)
                == Some(self.target_fingerprint.as_str())
            && snapshot
                .get("targetConnectionId")
                .and_then(serde_json::Value::as_str)
                == Some(self.target_connection_profile_id.as_str())
            && snapshot
                .get("targetName")
                .and_then(serde_json::Value::as_str)
                == Some(self.target_database_name.as_str())
            && snapshot
                .get("targetExists")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
            && snapshot
                .get("requiredVerificationLevel")
                .and_then(serde_json::Value::as_i64)
                == Some(self.required_verification_level)
            && snapshot
                .pointer("/source/snapshotId")
                .and_then(serde_json::Value::as_str)
                == Some(self.recovery_point_id.as_str())
            && snapshot
                .pointer("/source/replicaId")
                .and_then(serde_json::Value::as_str)
                == Some(self.source_replica_id.as_str())
            && snapshot
                .get("evaluatedAtMs")
                .and_then(serde_json::Value::as_i64)
                .is_some_and(|value| value <= self.created_at_ms)
            && snapshot
                .get("expiresAtMs")
                .and_then(serde_json::Value::as_i64)
                .is_some_and(|value| value > self.created_at_ms);
        if valid_binding {
            Ok(())
        } else {
            Err(RepositoryError::SecurityInputInvalid)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreEnqueueResult {
    work_item_id: WorkItemId,
    created: bool,
}

#[derive(Clone, Debug)]
pub struct RetentionDeleteSpec {
    operation_run_id: OperationRunId,
    work_item_id: WorkItemId,
    authorization: RetentionDeleteAuthorizationV2,
}

#[derive(Clone, Debug)]
pub struct RetentionTombstoneSpec {
    operation_run_id: OperationRunId,
    recovery_point_id: RecoveryPointId,
    destination_fingerprint: String,
    grace_period_ms: i64,
    tombstoned_at_ms: i64,
}

impl RetentionTombstoneSpec {
    pub fn new(
        operation_run_id: OperationRunId,
        recovery_point_id: RecoveryPointId,
        destination_fingerprint: impl Into<String>,
        grace_period_ms: i64,
        tombstoned_at_ms: i64,
    ) -> Self {
        Self {
            operation_run_id,
            recovery_point_id,
            destination_fingerprint: destination_fingerprint.into(),
            grace_period_ms,
            tombstoned_at_ms,
        }
    }

    pub(super) fn validate(&self) -> Result<(), RepositoryError> {
        if self.destination_fingerprint.trim().is_empty()
            || self.destination_fingerprint.len() > 512
            || self.destination_fingerprint.split_once(':').is_none()
            || self.grace_period_ms <= 0
            || self.tombstoned_at_ms < 0
            || self
                .tombstoned_at_ms
                .checked_add(self.grace_period_ms)
                .is_none()
        {
            return Err(RepositoryError::RetentionTombstoneInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetentionTombstoneResult {
    operation_run_id: OperationRunId,
    created: bool,
}

impl RetentionTombstoneResult {
    pub fn operation_run_id(&self) -> &OperationRunId {
        &self.operation_run_id
    }

    pub const fn created(&self) -> bool {
        self.created
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RetentionTombstoneObjectV1 {
    key: String,
    role: String,
    version: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RetentionTombstoneDocumentV1 {
    destination_fingerprint: String,
    grace_period_ms: i64,
    objects: Vec<RetentionTombstoneObjectV1>,
    phase: String,
    recovery_point_id: String,
    schema_version: u32,
    tombstoned_at_ms: i64,
}

impl RetentionDeleteSpec {
    pub fn new(
        operation_run_id: OperationRunId,
        work_item_id: WorkItemId,
        authorization: RetentionDeleteAuthorizationV2,
    ) -> Self {
        Self {
            operation_run_id,
            work_item_id,
            authorization,
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if self.authorization.to_json().is_err() || self.authorization.tombstoned_at_ms().is_none()
        {
            return Err(RepositoryError::SecurityInputInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetentionDeleteEnqueueResult {
    work_item_id: WorkItemId,
    created: bool,
}

impl RetentionDeleteEnqueueResult {
    pub fn work_item_id(&self) -> &WorkItemId {
        &self.work_item_id
    }

    pub const fn created(&self) -> bool {
        self.created
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IpcLedgerReservation {
    New,
    Completed(IpcStoredOutcome),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IpcStoredOutcome {
    succeeded: bool,
    response_json: String,
    result_ref_type: Option<String>,
    result_ref_id: Option<String>,
}

impl IpcStoredOutcome {
    pub const fn succeeded(&self) -> bool {
        self.succeeded
    }

    pub fn response_json(&self) -> &str {
        &self.response_json
    }

    pub fn result_ref_type(&self) -> Option<&str> {
        self.result_ref_type.as_deref()
    }

    pub fn result_ref_id(&self) -> Option<&str> {
        self.result_ref_id.as_deref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IpcAtomicMutationResult {
    created: bool,
    outcome: IpcStoredOutcome,
}

impl IpcAtomicMutationResult {
    pub const fn created(&self) -> bool {
        self.created
    }

    pub const fn outcome(&self) -> &IpcStoredOutcome {
        &self.outcome
    }
}

#[derive(Clone, Debug)]
pub struct IpcCompletionSpec {
    succeeded: bool,
    response_json: String,
    result_ref_type: Option<String>,
    result_ref_id: Option<String>,
    completed_at_ms: i64,
}

impl IpcCompletionSpec {
    pub fn succeeded(
        response_json: impl Into<String>,
        result_ref: Option<(String, String)>,
        completed_at_ms: i64,
    ) -> Self {
        let (result_ref_type, result_ref_id) = result_ref
            .map(|(kind, id)| (Some(kind), Some(id)))
            .unwrap_or((None, None));
        Self {
            succeeded: true,
            response_json: response_json.into(),
            result_ref_type,
            result_ref_id,
            completed_at_ms,
        }
    }

    pub fn failed_terminal(response_json: impl Into<String>, completed_at_ms: i64) -> Self {
        Self {
            succeeded: false,
            response_json: response_json.into(),
            result_ref_type: None,
            result_ref_id: None,
            completed_at_ms,
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if self.response_json.len() > MAX_STORED_RESPONSE_BYTES
            || !matches!(
                serde_json::from_str::<serde_json::Value>(&self.response_json),
                Ok(serde_json::Value::Object(_))
            )
            || self.completed_at_ms < 0
            || self.result_ref_type.as_ref().is_some_and(|value| {
                value.is_empty() || value.len() > 128 || value.bytes().any(|byte| byte < b' ')
            })
            || self.result_ref_id.as_ref().is_some_and(|value| {
                value.is_empty() || value.len() > 512 || value.bytes().any(|byte| byte < b' ')
            })
            || self.result_ref_type.is_some() != self.result_ref_id.is_some()
        {
            return Err(RepositoryError::SecurityInputInvalid);
        }
        Ok(())
    }

    fn stored(&self) -> IpcStoredOutcome {
        IpcStoredOutcome {
            succeeded: self.succeeded,
            response_json: self.response_json.clone(),
            result_ref_type: self.result_ref_type.clone(),
            result_ref_id: self.result_ref_id.clone(),
        }
    }
}

impl RestoreEnqueueResult {
    pub fn work_item_id(&self) -> &WorkItemId {
        &self.work_item_id
    }

    pub const fn created(&self) -> bool {
        self.created
    }
}

#[derive(Clone)]
pub struct IpcRepository {
    store: OperationalStore,
}

impl OperationalStore {
    pub fn ipc(&self) -> IpcRepository {
        IpcRepository {
            store: self.clone(),
        }
    }
}

impl IpcRepository {
    pub fn require_workspace_plan(
        &self,
        workspace_id: &WorkspaceId,
        plan_id: &BackupPlanId,
    ) -> Result<(), RepositoryError> {
        self.store
            .workspace()
            .require_plan(workspace_id, plan_id.as_str())
    }

    pub fn require_workspace_recovery_point(
        &self,
        workspace_id: &WorkspaceId,
        recovery_point_id: &RecoveryPointId,
    ) -> Result<(), RepositoryError> {
        self.store
            .workspace()
            .require_recovery_point(workspace_id, recovery_point_id.as_str())
    }

    pub fn require_workspace_connection(
        &self,
        workspace_id: &WorkspaceId,
        connection_id: &ConnectionProfileId,
    ) -> Result<(), RepositoryError> {
        self.store
            .workspace()
            .require_connection(workspace_id, connection_id.as_str())
    }

    pub fn require_workspace_destination(
        &self,
        workspace_id: &WorkspaceId,
        destination_id: &DestinationId,
    ) -> Result<(), RepositoryError> {
        self.store
            .workspace()
            .require_destination(workspace_id, destination_id.as_str())
    }

    pub fn persisted_authority(&self) -> Result<PersistedAuthoritySnapshot, RepositoryError> {
        let reader = self.store.open_reader()?;
        load_authority(reader.connection())
    }

    pub fn reserve_request(
        &self,
        request: IpcRequestSpec,
    ) -> Result<IpcLedgerReservation, RepositoryError> {
        request.validate()?;
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let existing: Option<ExistingLedgerRow> = transaction
                .query_row(
                    "SELECT payload_digest_blake3, state, response_json,
                            result_ref_type, result_ref_id
                     FROM ipc_request_ledger
                     WHERE client_sid_hash = ?1
                       AND command_name = ?2
                       AND idempotency_key = ?3",
                    params![
                        request.client_sid_hash,
                        request.command_name,
                        request.idempotency_key
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
            if let Some((payload_digest, state, response_json, result_ref_type, result_ref_id)) =
                existing
            {
                if payload_digest != request.payload_digest_blake3 {
                    return Err(RepositoryError::IpcIdempotencyConflict);
                }
                let succeeded = match state.as_str() {
                    "succeeded" => true,
                    "failed_terminal" => false,
                    _ => return Err(RepositoryError::IpcRequestIncomplete),
                };
                let response_json = response_json.ok_or(RepositoryError::IpcRequestIncomplete)?;
                transaction.commit()?;
                return Ok(IpcLedgerReservation::Completed(IpcStoredOutcome {
                    succeeded,
                    response_json,
                    result_ref_type,
                    result_ref_id,
                }));
            }

            transaction.execute(
                "INSERT INTO ipc_request_ledger(
                     request_id, command_name, client_sid_hash, idempotency_key,
                     payload_digest_blake3, state, accepted_at_ms, expires_at_ms, created_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'running', ?6, ?7, ?6)",
                params![
                    request.request_id.as_str(),
                    request.command_name,
                    request.client_sid_hash,
                    request.idempotency_key,
                    request.payload_digest_blake3,
                    request.accepted_at_ms,
                    request.expires_at_ms
                ],
            )?;
            transaction.commit()?;
            Ok(IpcLedgerReservation::New)
        })
    }

    pub fn reserve_request_with_expired_completion(
        &self,
        request: IpcRequestSpec,
        expired_completion: IpcCompletionSpec,
    ) -> Result<IpcLedgerReservation, RepositoryError> {
        request.validate()?;
        expired_completion.validate()?;
        if expired_completion.succeeded
            || expired_completion.result_ref_type.is_some()
            || expired_completion.result_ref_id.is_some()
            || expired_completion.completed_at_ms != request.accepted_at_ms
        {
            return Err(RepositoryError::SecurityInputInvalid);
        }
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let existing: Option<ExistingExpirableLedgerRow> = transaction
                .query_row(
                    "SELECT payload_digest_blake3, state, response_json,
                            result_ref_type, result_ref_id, expires_at_ms
                     FROM ipc_request_ledger
                     WHERE client_sid_hash = ?1
                       AND command_name = ?2
                       AND idempotency_key = ?3",
                    params![
                        request.client_sid_hash,
                        request.command_name,
                        request.idempotency_key
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
            if let Some((
                payload_digest,
                state,
                response_json,
                result_ref_type,
                result_ref_id,
                expires_at_ms,
            )) = existing
            {
                if payload_digest != request.payload_digest_blake3 {
                    return Err(RepositoryError::IpcIdempotencyConflict);
                }
                if matches!(state.as_str(), "succeeded" | "failed_terminal") {
                    let succeeded = state == "succeeded";
                    let response_json =
                        response_json.ok_or(RepositoryError::IpcRequestIncomplete)?;
                    transaction.commit()?;
                    return Ok(IpcLedgerReservation::Completed(IpcStoredOutcome {
                        succeeded,
                        response_json,
                        result_ref_type,
                        result_ref_id,
                    }));
                }
                if !matches!(state.as_str(), "accepted" | "running")
                    || request.accepted_at_ms < expires_at_ms
                {
                    return Err(RepositoryError::IpcRequestIncomplete);
                }

                let updated = transaction.execute(
                    "UPDATE ipc_request_ledger
                     SET state = 'failed_terminal', result_ref_type = NULL,
                         result_ref_id = NULL, response_json = ?4, completed_at_ms = ?5
                     WHERE client_sid_hash = ?1
                       AND command_name = ?2
                       AND idempotency_key = ?3
                       AND payload_digest_blake3 = ?6
                       AND state IN ('accepted', 'running')
                       AND expires_at_ms <= ?5",
                    params![
                        request.client_sid_hash,
                        request.command_name,
                        request.idempotency_key,
                        expired_completion.response_json,
                        expired_completion.completed_at_ms,
                        request.payload_digest_blake3
                    ],
                )?;
                if updated != 1 {
                    return Err(RepositoryError::IpcRequestIncomplete);
                }
                let stored = expired_completion.stored();
                transaction.commit()?;
                return Ok(IpcLedgerReservation::Completed(stored));
            }

            transaction.execute(
                "INSERT INTO ipc_request_ledger(
                     request_id, command_name, client_sid_hash, idempotency_key,
                     payload_digest_blake3, state, accepted_at_ms, expires_at_ms, created_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'running', ?6, ?7, ?6)",
                params![
                    request.request_id.as_str(),
                    request.command_name,
                    request.client_sid_hash,
                    request.idempotency_key,
                    request.payload_digest_blake3,
                    request.accepted_at_ms,
                    request.expires_at_ms
                ],
            )?;
            transaction.commit()?;
            Ok(IpcLedgerReservation::New)
        })
    }

    pub fn complete_request(
        &self,
        request_id: &IpcRequestId,
        payload_digest_blake3: &str,
        completion: IpcCompletionSpec,
    ) -> Result<IpcStoredOutcome, RepositoryError> {
        if !is_digest(payload_digest_blake3) {
            return Err(RepositoryError::SecurityInputInvalid);
        }
        completion.validate()?;
        let request_id = request_id.clone();
        let payload_digest_blake3 = payload_digest_blake3.to_owned();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let state = if completion.succeeded {
                "succeeded"
            } else {
                "failed_terminal"
            };
            let updated = transaction.execute(
                "UPDATE ipc_request_ledger
                 SET state = ?3, result_ref_type = ?4, result_ref_id = ?5,
                     response_json = ?6, completed_at_ms = ?7
                 WHERE request_id = ?1
                   AND payload_digest_blake3 = ?2
                   AND state IN ('accepted', 'running')
                   AND accepted_at_ms <= ?7",
                params![
                    request_id.as_str(),
                    payload_digest_blake3,
                    state,
                    completion.result_ref_type,
                    completion.result_ref_id,
                    completion.response_json,
                    completion.completed_at_ms
                ],
            )?;
            if updated != 1 {
                return Err(RepositoryError::IpcRequestIncomplete);
            }
            transaction.commit()?;
            Ok(completion.stored())
        })
    }

    pub fn completed_request(
        &self,
        request: &IpcRequestSpec,
    ) -> Result<Option<IpcStoredOutcome>, RepositoryError> {
        request.validate()?;
        let reader = self.store.open_reader()?;
        let existing: Option<ExistingLedgerRow> = reader
            .connection()
            .query_row(
                "SELECT payload_digest_blake3, state, response_json,
                        result_ref_type, result_ref_id
                 FROM ipc_request_ledger
                 WHERE client_sid_hash = ?1
                   AND command_name = ?2
                   AND idempotency_key = ?3",
                params![
                    request.client_sid_hash,
                    request.command_name,
                    request.idempotency_key
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
        let Some((payload_digest, state, response_json, result_ref_type, result_ref_id)) = existing
        else {
            return Ok(None);
        };
        if payload_digest != request.payload_digest_blake3 {
            return Err(RepositoryError::IpcIdempotencyConflict);
        }
        let succeeded = match state.as_str() {
            "succeeded" => true,
            "failed_terminal" => false,
            _ => return Err(RepositoryError::IpcRequestIncomplete),
        };
        Ok(Some(IpcStoredOutcome {
            succeeded,
            response_json: response_json.ok_or(RepositoryError::IpcRequestIncomplete)?,
            result_ref_type,
            result_ref_id,
        }))
    }

    pub fn enqueue_safe_restore_request(
        &self,
        request: IpcRequestSpec,
        restore: SafeRestoreSpec,
        completion: IpcCompletionSpec,
    ) -> Result<IpcAtomicMutationResult, RepositoryError> {
        request.validate()?;
        restore.validate()?;
        completion.validate()?;
        if request.command_name != "restore.enqueue_safe"
            || restore.created_at_ms() != request.accepted_at_ms
            || !completion.succeeded
            || completion.result_ref_type.as_deref() != Some("restore_run")
            || completion.result_ref_id.as_deref() != Some(restore.restore_run_id().as_str())
            || completion.completed_at_ms != request.accepted_at_ms
        {
            return Err(RepositoryError::SecurityInputInvalid);
        }
        let stored = completion.stored();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let existing: Option<ExistingLedgerRow> = transaction
                .query_row(
                    "SELECT payload_digest_blake3, state, response_json,
                            result_ref_type, result_ref_id
                     FROM ipc_request_ledger
                     WHERE client_sid_hash = ?1
                       AND command_name = ?2
                       AND idempotency_key = ?3",
                    params![
                        request.client_sid_hash,
                        request.command_name,
                        request.idempotency_key
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
            if let Some((payload_digest, state, response_json, result_ref_type, result_ref_id)) =
                existing
            {
                if payload_digest != request.payload_digest_blake3 {
                    return Err(RepositoryError::IpcIdempotencyConflict);
                }
                if state != "succeeded"
                    || result_ref_type.as_deref() != Some("restore_run")
                    || result_ref_id
                        .as_deref()
                        .and_then(|value| RestoreRunId::from_str(value).ok())
                        .is_none()
                {
                    return Err(RepositoryError::IpcRequestIncomplete);
                }
                let response_json = response_json.ok_or(RepositoryError::IpcRequestIncomplete)?;
                transaction.commit()?;
                return Ok(IpcAtomicMutationResult {
                    created: false,
                    outcome: IpcStoredOutcome {
                        succeeded: true,
                        response_json,
                        result_ref_type,
                        result_ref_id,
                    },
                });
            }
            transaction.execute(
                "INSERT INTO ipc_request_ledger(
                     request_id, command_name, client_sid_hash, idempotency_key,
                     payload_digest_blake3, state, accepted_at_ms, expires_at_ms, created_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'accepted', ?6, ?7, ?6)",
                params![
                    request.request_id.as_str(),
                    request.command_name,
                    request.client_sid_hash,
                    request.idempotency_key,
                    request.payload_digest_blake3,
                    request.accepted_at_ms,
                    request.expires_at_ms
                ],
            )?;
            insert_safe_restore_in_transaction(&transaction, &restore)?;
            let completed = transaction.execute(
                "UPDATE ipc_request_ledger
                 SET state = 'succeeded', result_ref_type = ?2,
                     result_ref_id = ?3, response_json = ?4, completed_at_ms = ?5
                 WHERE request_id = ?1
                   AND state = 'accepted'
                   AND payload_digest_blake3 = ?6",
                params![
                    request.request_id.as_str(),
                    completion.result_ref_type,
                    completion.result_ref_id,
                    completion.response_json,
                    completion.completed_at_ms,
                    request.payload_digest_blake3
                ],
            )?;
            if completed != 1 {
                return Err(RepositoryError::IpcRequestIncomplete);
            }
            transaction.commit()?;
            Ok(IpcAtomicMutationResult {
                created: true,
                outcome: stored,
            })
        })
    }

    pub fn enqueue_replica_repair_request(
        &self,
        request: IpcRequestSpec,
        repair: ReplicaRepairRequestSpec,
    ) -> Result<ReplicaRepairEnqueueResult, RepositoryError> {
        request.validate()?;
        repair.validate()?;
        if request.command_name != "repair.enqueue"
            || repair.available_at_ms != request.accepted_at_ms
        {
            return Err(RepositoryError::SecurityInputInvalid);
        }
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let existing: Option<ExistingLedgerRow> = transaction
                .query_row(
                    "SELECT payload_digest_blake3, state, response_json,
                            result_ref_type, result_ref_id
                     FROM ipc_request_ledger
                     WHERE client_sid_hash = ?1
                       AND command_name = ?2
                       AND idempotency_key = ?3",
                    params![
                        request.client_sid_hash,
                        request.command_name,
                        request.idempotency_key
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
            if let Some((payload_digest, state, response_json, result_ref_type, result_ref_id)) =
                existing
            {
                if payload_digest != request.payload_digest_blake3 {
                    return Err(RepositoryError::IpcIdempotencyConflict);
                }
                if state != "succeeded" || result_ref_type.as_deref() != Some("operation_run") {
                    return Err(RepositoryError::IpcRequestIncomplete);
                }
                let response_json = response_json.ok_or(RepositoryError::IpcRequestIncomplete)?;
                let stored = serde_json::from_str::<ReplicaRepairStoredResponseV1>(&response_json)
                    .map_err(|_| RepositoryError::IpcRequestIncomplete)?;
                let operation_run_id = OperationRunId::from_str(&stored.operation_run_id)
                    .map_err(|_| RepositoryError::IpcRequestIncomplete)?;
                let work_item_id = WorkItemId::from_str(&stored.work_item_id)
                    .map_err(|_| RepositoryError::IpcRequestIncomplete)?;
                if stored.schema_version != 1
                    || stored.state != "queued"
                    || stored.activity_after_seq <= 0
                    || result_ref_id.as_deref() != Some(operation_run_id.as_str())
                {
                    return Err(RepositoryError::IpcRequestIncomplete);
                }
                transaction.commit()?;
                return Ok(ReplicaRepairEnqueueResult {
                    created: false,
                    operation_run_id,
                    work_item_id,
                    activity_after_seq: stored.activity_after_seq,
                });
            }

            let source_fingerprint: Option<String> = transaction
                .query_row(
                    "SELECT replica.destination_fingerprint
                     FROM catalog_artifact_replicas AS replica
                     JOIN catalog_recovery_points AS recovery
                       ON recovery.id = replica.recovery_point_id
                     JOIN catalog_snapshot_projection AS projection
                       ON projection.recovery_point_id = recovery.id
                     WHERE replica.id = ?1
                       AND replica.recovery_point_id = ?2
                       AND replica.state = 'committed'
                       AND replica.reader_status = 'eligible'
                       AND recovery.state = 'complete'
                       AND recovery.signature_status = 'valid'
                       AND projection.artifact_completeness = 'complete'
                       AND projection.restore_eligibility IN ('eligible', 'conditional')",
                    params![
                        repair.source_replica_id.as_str(),
                        repair.recovery_point_id.as_str()
                    ],
                    |row| row.get(0),
                )
                .optional()?;
            let source_fingerprint =
                source_fingerprint.ok_or(RepositoryError::SecurityInputInvalid)?;
            let target: Option<(String, String)> = transaction
                .query_row(
                    "SELECT kind, state
                     FROM destinations
                     WHERE id = ?1
                       AND deleted_at_ms IS NULL",
                    [repair.target_destination_id.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let (target_kind, target_state) =
                target.ok_or(RepositoryError::SecurityInputInvalid)?;
            if !matches!(target_state.as_str(), "ready" | "attention") {
                return Err(RepositoryError::SecurityInputInvalid);
            }
            let target_fingerprint =
                format!("{target_kind}:{}", repair.target_destination_id.as_str());
            let target_already_healthy: bool = transaction.query_row(
                "SELECT EXISTS(
                     SELECT 1
                     FROM catalog_artifact_replicas
                     WHERE recovery_point_id = ?1
                       AND destination_fingerprint = ?2
                       AND state = 'committed'
                       AND reader_status = 'eligible'
                 )",
                params![repair.recovery_point_id.as_str(), target_fingerprint],
                |row| row.get(0),
            )?;
            if source_fingerprint == target_fingerprint || target_already_healthy {
                return Err(RepositoryError::SecurityInputInvalid);
            }
            let secret_refs = resolve_replica_repair_secret_refs_in_transaction(
                &transaction,
                &source_fingerprint,
                &target_fingerprint,
            )?;
            let operation = ReplicaRepairWorkSpec::new(
                repair.recovery_point_id,
                source_fingerprint,
                target_fingerprint,
                secret_refs,
                repair.available_at_ms,
            )
            .operation_work()?;

            transaction.execute(
                "INSERT INTO ipc_request_ledger(
                     request_id, command_name, client_sid_hash, idempotency_key,
                     payload_digest_blake3, state, accepted_at_ms, expires_at_ms, created_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'accepted', ?6, ?7, ?6)",
                params![
                    request.request_id.as_str(),
                    request.command_name,
                    request.client_sid_hash,
                    request.idempotency_key,
                    request.payload_digest_blake3,
                    request.accepted_at_ms,
                    request.expires_at_ms
                ],
            )?;
            let upsert = insert_operation_work_in_transaction(&transaction, &operation)?;
            let activity_id = uuid::Uuid::new_v5(
                &uuid::Uuid::NAMESPACE_URL,
                format!("repair-queued:{}", request.request_id.as_str()).as_bytes(),
            )
            .to_string();
            transaction.execute(
                "INSERT INTO activity_events(
                     id, occurred_at_ms, severity, event_code,
                     entity_type, entity_id, work_item_id, message,
                     attributes_json, redaction_version, created_at_ms
                 ) VALUES (
                     ?1, ?2, 'info', 'REPLICA_REPAIR_QUEUED',
                     'operation_run', ?3, ?4, ?5,
                     '{}', 1, ?2
                 )",
                params![
                    activity_id,
                    request.accepted_at_ms,
                    upsert.operation_run_id().as_str(),
                    upsert.work_item_id().as_str(),
                    "The service accepted an authoritative Replica repair request."
                ],
            )?;
            let activity_after_seq = transaction.last_insert_rowid();
            let stored = ReplicaRepairStoredResponseV1 {
                schema_version: 1,
                operation_run_id: upsert.operation_run_id().as_str().to_owned(),
                work_item_id: upsert.work_item_id().as_str().to_owned(),
                state: "queued".to_owned(),
                activity_after_seq,
            };
            let response_json = serde_json::to_string(&stored)
                .map_err(|_| RepositoryError::SecurityInputInvalid)?;
            let completed = transaction.execute(
                "UPDATE ipc_request_ledger
                 SET state = 'succeeded', result_ref_type = 'operation_run',
                     result_ref_id = ?2, response_json = ?3, completed_at_ms = ?4
                 WHERE request_id = ?1
                   AND state = 'accepted'
                   AND payload_digest_blake3 = ?5",
                params![
                    request.request_id.as_str(),
                    upsert.operation_run_id().as_str(),
                    response_json,
                    request.accepted_at_ms,
                    request.payload_digest_blake3
                ],
            )?;
            if completed != 1 {
                return Err(RepositoryError::IpcRequestIncomplete);
            }
            transaction.commit()?;
            Ok(ReplicaRepairEnqueueResult {
                created: true,
                operation_run_id: upsert.operation_run_id().clone(),
                work_item_id: upsert.work_item_id().clone(),
                activity_after_seq,
            })
        })
    }

    pub fn enqueue_manual_verification_request(
        &self,
        request: IpcRequestSpec,
        verification: ManualVerificationRequestSpec,
    ) -> Result<ManualVerificationEnqueueResult, RepositoryError> {
        request.validate()?;
        verification.validate()?;
        if request.command_name != "verification.enqueue"
            || verification.available_at_ms != request.accepted_at_ms
        {
            return Err(RepositoryError::VerificationInputInvalid);
        }
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let existing: Option<ExistingLedgerRow> = transaction
                .query_row(
                    "SELECT payload_digest_blake3, state, response_json,
                            result_ref_type, result_ref_id
                     FROM ipc_request_ledger
                     WHERE client_sid_hash = ?1
                       AND command_name = ?2
                       AND idempotency_key = ?3",
                    params![
                        request.client_sid_hash,
                        request.command_name,
                        request.idempotency_key
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
            if let Some((payload_digest, state, response_json, result_ref_type, result_ref_id)) =
                existing
            {
                if payload_digest != request.payload_digest_blake3 {
                    return Err(RepositoryError::IpcIdempotencyConflict);
                }
                if state != "succeeded" || result_ref_type.as_deref() != Some("verification_run") {
                    return Err(RepositoryError::IpcRequestIncomplete);
                }
                let stored = serde_json::from_str::<ManualVerificationStoredResponseV1>(
                    response_json
                        .as_deref()
                        .ok_or(RepositoryError::IpcRequestIncomplete)?,
                )
                .map_err(|_| RepositoryError::IpcRequestIncomplete)?;
                let verification_run_id = VerificationRunId::from_str(&stored.verification_run_id)
                    .map_err(|_| RepositoryError::IpcRequestIncomplete)?;
                let work_item_id = stored
                    .work_item_id
                    .as_deref()
                    .map(WorkItemId::from_str)
                    .transpose()
                    .map_err(|_| RepositoryError::IpcRequestIncomplete)?;
                let source_replica_id = ArtifactReplicaId::from_str(&stored.source_replica_id)
                    .map_err(|_| RepositoryError::IpcRequestIncomplete)?;
                if stored.schema_version != 1
                    || !matches!(stored.state.as_str(), "queued" | "unsupported")
                    || !(1..=3).contains(&stored.requested_level)
                    || stored.activity_after_seq <= 0
                    || (stored.state == "queued") != work_item_id.is_some()
                    || result_ref_id.as_deref() != Some(verification_run_id.as_str())
                {
                    return Err(RepositoryError::IpcRequestIncomplete);
                }
                transaction.commit()?;
                return Ok(ManualVerificationEnqueueResult {
                    created: false,
                    verification_run_id,
                    work_item_id,
                    state: stored.state,
                    requested_level: stored.requested_level,
                    source_replica_id,
                    support_status: stored.support_status,
                    limitation: stored.limitation,
                    error_code: stored.error_code,
                    activity_after_seq: stored.activity_after_seq,
                });
            }

            let engine: Option<String> = transaction
                .query_row(
                    "SELECT recovery.engine
                     FROM catalog_recovery_points AS recovery
                     JOIN catalog_snapshot_projection AS projection
                       ON projection.recovery_point_id = recovery.id
                     WHERE recovery.id = ?1
                       AND recovery.state = 'complete'
                       AND recovery.signature_status = 'valid'
                       AND projection.artifact_completeness = 'complete'",
                    [verification.recovery_point_id.as_str()],
                    |row| row.get(0),
                )
                .optional()?;
            let engine = engine.ok_or(RepositoryError::RecoveryPointIncomplete)?;
            let source_replica_id: Option<String> = transaction
                .query_row(
                    "SELECT id
                     FROM catalog_artifact_replicas
                     WHERE recovery_point_id = ?1
                       AND state = 'committed'
                       AND reader_status = 'eligible'
                       AND (?2 IS NULL OR id = ?2)
                     ORDER BY id
                     LIMIT 1",
                    params![
                        verification.recovery_point_id.as_str(),
                        verification
                            .source_replica_id
                            .as_ref()
                            .map(ArtifactReplicaId::as_str)
                    ],
                    |row| row.get(0),
                )
                .optional()?;
            let source_replica_id = source_replica_id
                .as_deref()
                .map(ArtifactReplicaId::from_str)
                .transpose()
                .map_err(|_| RepositoryError::VerificationInputInvalid)?
                .ok_or(RepositoryError::VerificationInputInvalid)?;
            let capability_name = match verification.requested_level {
                1 => "verify_l1",
                2 => "verify_l2",
                3 => "restore_l3",
                _ => return Err(RepositoryError::VerificationInputInvalid),
            };
            let capability: Option<(String, Option<String>)> = transaction
                .query_row(
                    "SELECT capability.support_status, capability.limitation_text
                     FROM engine_certifications AS certification
                     JOIN engine_certification_capabilities AS capability
                       ON capability.certification_id = certification.id
                     WHERE certification.engine = ?1
                       AND capability.capability = ?2
                       AND capability.support_status IN ('passed', 'limited')
                       AND certification.status = 'certified'
                       AND certification.certified_at_ms <= ?3
                       AND (
                           certification.expires_at_ms IS NULL
                           OR certification.expires_at_ms > ?3
                       )
                     ORDER BY certification.certified_at_ms DESC, certification.id
                     LIMIT 1",
                    params![engine, capability_name, request.accepted_at_ms],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let unsupported_reason = if verification.requested_level == 3 {
                Some("VERIFICATION_RESTORE_RUN_REQUIRED")
            } else if capability.is_none() {
                Some("VERIFY_LEVEL_UNSUPPORTED")
            } else {
                None
            };
            let state = if unsupported_reason.is_some() {
                "unsupported"
            } else {
                "queued"
            };

            transaction.execute(
                "INSERT INTO ipc_request_ledger(
                     request_id, command_name, client_sid_hash, idempotency_key,
                     payload_digest_blake3, state, accepted_at_ms, expires_at_ms, created_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'accepted', ?6, ?7, ?6)",
                params![
                    request.request_id.as_str(),
                    request.command_name,
                    request.client_sid_hash,
                    request.idempotency_key,
                    request.payload_digest_blake3,
                    request.accepted_at_ms,
                    request.expires_at_ms
                ],
            )?;
            transaction.execute(
                "INSERT INTO verification_runs(
                     id, recovery_point_id, source_replica_id, requested_level,
                     trigger_kind, state, completed_at_ms, error_code,
                     created_at_ms, updated_at_ms
                 ) VALUES (
                     ?1, ?2, ?3, ?4,
                     'manual', ?5,
                     CASE WHEN ?5 = 'unsupported' THEN ?6 ELSE NULL END,
                     ?7, ?6, ?6
                 )",
                params![
                    verification.verification_run_id.as_str(),
                    verification.recovery_point_id.as_str(),
                    source_replica_id.as_str(),
                    verification.requested_level,
                    state,
                    request.accepted_at_ms,
                    unsupported_reason
                ],
            )?;
            if state == "unsupported" {
                project_terminal_verification_run(
                    &transaction,
                    verification.verification_run_id.as_str(),
                    request.accepted_at_ms,
                )?;
            }
            let work_item_id = if state == "queued" {
                transaction.execute(
                    "INSERT INTO work_items(
                         id, kind, verification_run_id, priority, state,
                         available_at_ms, attempt_limit, created_at_ms, updated_at_ms
                     ) VALUES (?1, 'verification', ?2, 200, 'queued', ?3, 3, ?3, ?3)",
                    params![
                        verification.work_item_id.as_str(),
                        verification.verification_run_id.as_str(),
                        request.accepted_at_ms
                    ],
                )?;
                Some(verification.work_item_id.clone())
            } else {
                None
            };
            let activity_id = uuid::Uuid::new_v5(
                &uuid::Uuid::NAMESPACE_URL,
                format!(
                    "manual-verification:{}:{}",
                    request.request_id.as_str(),
                    verification.requested_level
                )
                .as_bytes(),
            )
            .to_string();
            let event_code = if state == "queued" {
                "VERIFICATION_QUEUED"
            } else {
                "VERIFICATION_UNSUPPORTED"
            };
            let attributes_json = serde_json::json!({
                "errorCode": unsupported_reason,
                "recoveryPointId": verification.recovery_point_id.as_str(),
                "requestedLevel": verification.requested_level,
                "sourceReplicaId": source_replica_id.as_str(),
                "verificationRunId": verification.verification_run_id.as_str(),
            })
            .to_string();
            transaction.execute(
                "INSERT INTO activity_events(
                     id, occurred_at_ms, severity, event_code,
                     entity_type, entity_id, work_item_id, message,
                     attributes_json, redaction_version, created_at_ms
                 ) VALUES (
                     ?1, ?2, 'info', ?3,
                     'verification_run', ?4, ?5, ?6,
                     ?7, 1, ?2
                 )",
                params![
                    activity_id,
                    request.accepted_at_ms,
                    event_code,
                    verification.verification_run_id.as_str(),
                    work_item_id.as_ref().map(WorkItemId::as_str),
                    if state == "queued" {
                        "The service accepted an exact-level manual verification request."
                    } else {
                        "The requested manual verification level is explicitly unsupported."
                    },
                    attributes_json
                ],
            )?;
            let activity_after_seq = transaction.last_insert_rowid();
            let stored = ManualVerificationStoredResponseV1 {
                schema_version: 1,
                verification_run_id: verification.verification_run_id.as_str().to_owned(),
                work_item_id: work_item_id
                    .as_ref()
                    .map(|work_item_id| work_item_id.as_str().to_owned()),
                state: state.to_owned(),
                requested_level: verification.requested_level,
                source_replica_id: source_replica_id.as_str().to_owned(),
                support_status: capability.as_ref().map(|value| value.0.clone()),
                limitation: capability.as_ref().and_then(|value| value.1.clone()),
                error_code: unsupported_reason.map(str::to_owned),
                activity_after_seq,
            };
            let response_json = serde_json::to_string(&stored)
                .map_err(|_| RepositoryError::VerificationInputInvalid)?;
            let completed = transaction.execute(
                "UPDATE ipc_request_ledger
                 SET state = 'succeeded', result_ref_type = 'verification_run',
                     result_ref_id = ?2, response_json = ?3, completed_at_ms = ?4
                 WHERE request_id = ?1
                   AND state = 'accepted'
                   AND payload_digest_blake3 = ?5",
                params![
                    request.request_id.as_str(),
                    verification.verification_run_id.as_str(),
                    response_json,
                    request.accepted_at_ms,
                    request.payload_digest_blake3
                ],
            )?;
            if completed != 1 {
                return Err(RepositoryError::IpcRequestIncomplete);
            }
            transaction.commit()?;
            Ok(ManualVerificationEnqueueResult {
                created: true,
                verification_run_id: verification.verification_run_id,
                work_item_id,
                state: state.to_owned(),
                requested_level: verification.requested_level,
                source_replica_id,
                support_status: capability.as_ref().map(|value| value.0.clone()),
                limitation: capability.and_then(|value| value.1),
                error_code: unsupported_reason.map(str::to_owned),
                activity_after_seq,
            })
        })
    }

    pub fn enqueue_manual_backup_request(
        &self,
        request: IpcRequestSpec,
        backup: ManualBackupRequestSpec,
    ) -> Result<ManualBackupEnqueueResult, RepositoryError> {
        request.validate()?;
        backup.validate()?;
        if request.command_name != "backup.enqueue"
            || backup.requested_at_ms != request.accepted_at_ms
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let existing: Option<ExistingLedgerRow> = transaction
                .query_row(
                    "SELECT payload_digest_blake3, state, response_json,
                            result_ref_type, result_ref_id
                     FROM ipc_request_ledger
                     WHERE client_sid_hash = ?1
                       AND command_name = ?2
                       AND idempotency_key = ?3",
                    params![
                        request.client_sid_hash,
                        request.command_name,
                        request.idempotency_key
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
            if let Some((payload_digest, state, response_json, result_ref_type, result_ref_id)) =
                existing
            {
                if payload_digest != request.payload_digest_blake3 {
                    return Err(RepositoryError::IpcIdempotencyConflict);
                }
                if state != "succeeded" || result_ref_type.as_deref() != Some("plan_occurrence") {
                    return Err(RepositoryError::IpcRequestIncomplete);
                }
                let stored = serde_json::from_str::<ManualBackupStoredResponseV1>(
                    response_json
                        .as_deref()
                        .ok_or(RepositoryError::IpcRequestIncomplete)?,
                )
                .map_err(|_| RepositoryError::IpcRequestIncomplete)?;
                let plan_id = BackupPlanId::from_str(&stored.plan_id)
                    .map_err(|_| RepositoryError::IpcRequestIncomplete)?;
                let plan_revision_id = PlanRevisionId::from_str(&stored.plan_revision_id)
                    .map_err(|_| RepositoryError::IpcRequestIncomplete)?;
                let plan_occurrence_id = PlanOccurrenceId::from_str(&stored.plan_occurrence_id)
                    .map_err(|_| RepositoryError::IpcRequestIncomplete)?;
                let work_items = stored
                    .work_items
                    .into_iter()
                    .map(|item| {
                        let backup_run_id = BackupRunId::from_str(&item.backup_run_id)
                            .map_err(|_| RepositoryError::IpcRequestIncomplete)?;
                        let work_item_id = WorkItemId::from_str(&item.work_item_id)
                            .map_err(|_| RepositoryError::IpcRequestIncomplete)?;
                        let target_id = DatabaseTargetId::from_str(&item.target_id)
                            .map_err(|_| RepositoryError::IpcRequestIncomplete)?;
                        let scope_id = BackupScopeId::from_str(
                            item.scope_id.as_deref().unwrap_or(&item.target_id),
                        )
                        .map_err(|_| RepositoryError::IpcRequestIncomplete)?;
                        let scope_name =
                            item.scope_name.unwrap_or_else(|| item.target_name.clone());
                        let scope_mode = item
                            .scope_mode
                            .unwrap_or_else(|| "whole_database".to_owned());
                        if item.state != "queued"
                            || item.target_name.is_empty()
                            || scope_name.is_empty()
                            || !matches!(scope_mode.as_str(), "whole_database" | "explicit_tables")
                            || !supported_backup_engine(&item.engine)
                        {
                            return Err(RepositoryError::IpcRequestIncomplete);
                        }
                        Ok(ManualBackupWorkItem {
                            backup_run_id,
                            work_item_id,
                            target_id,
                            target_name: item.target_name,
                            scope_id,
                            scope_name,
                            scope_mode,
                            engine: item.engine,
                        })
                    })
                    .collect::<Result<Vec<_>, RepositoryError>>()?;
                if stored.schema_version != 1
                    || stored.state != "queued"
                    || stored.plan_name.is_empty()
                    || plan_id != backup.plan_id
                    || work_items.is_empty()
                    || work_items.len() > 1_000
                    || stored.activity_after_seq <= 0
                    || result_ref_id.as_deref() != Some(plan_occurrence_id.as_str())
                {
                    return Err(RepositoryError::IpcRequestIncomplete);
                }
                transaction.commit()?;
                return Ok(ManualBackupEnqueueResult {
                    created: false,
                    plan_id,
                    plan_revision_id,
                    plan_occurrence_id,
                    plan_name: stored.plan_name,
                    work_items,
                    activity_after_seq: stored.activity_after_seq,
                });
            }

            let plan: Option<(String, String)> = transaction
                .query_row(
                    "SELECT revision.id, plan.display_name
                     FROM backup_plans AS plan
                     JOIN workspace_backup_plans AS ownership
                       ON ownership.backup_plan_id = plan.id
                      AND ownership.workspace_id = ?2
                     JOIN backup_plan_revisions AS revision
                       ON revision.id = plan.current_revision_id
                      AND revision.backup_plan_id = plan.id
                     WHERE plan.id = ?1
                       AND plan.state = 'enabled'
                       AND plan.deleted_at_ms IS NULL
                       AND revision.published_at_ms IS NOT NULL",
                    params![backup.plan_id.as_str(), backup.workspace_id.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let (plan_revision_id, plan_name) = plan.ok_or(RepositoryError::NotFound)?;
            let plan_revision_id = PlanRevisionId::from_str(&plan_revision_id)
                .map_err(|_| RepositoryError::RuntimeStateInvalid)?;
            let scopes = {
                let mut statement = transaction.prepare(
                    "SELECT target.id, target.display_name, connection.engine,
                            revision_scope.source_fingerprint,
                            revision_scope.backup_scope_id, scope.display_name,
                            revision_scope.scope_mode
                     FROM plan_revision_targets AS membership
                     JOIN plan_revision_scopes AS revision_scope
                       ON revision_scope.plan_revision_id = membership.plan_revision_id
                      AND revision_scope.database_target_id = membership.database_target_id
                     JOIN backup_scopes AS scope
                       ON scope.id = revision_scope.backup_scope_id
                     JOIN database_targets AS target
                       ON target.id = membership.database_target_id
                     JOIN connection_profiles AS connection
                       ON connection.id = target.connection_profile_id
                     WHERE membership.plan_revision_id = ?1
                     ORDER BY target.id",
                )?;
                let rows = statement
                    .query_map([plan_revision_id.as_str()], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, String>(6)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                rows
            };
            if scopes.is_empty()
                || scopes.len() > 1_000
                || scopes.iter().any(
                    |(_, name, engine, fingerprint, _, scope_name, scope_mode)| {
                        name.is_empty()
                            || scope_name.is_empty()
                            || !matches!(scope_mode.as_str(), "whole_database" | "explicit_tables")
                            || !supported_backup_engine(engine)
                            || fingerprint.is_empty()
                            || fingerprint.len() > 1_024
                    },
                )
            {
                return Err(RepositoryError::RuntimeStateInvalid);
            }
            let identity = OccurrenceIdentity::manual(request.idempotency_key.clone())?;
            let plan_occurrence_id = identity.deterministic_id(uuid::Uuid::NAMESPACE_URL);

            transaction.execute(
                "INSERT INTO ipc_request_ledger(
                     request_id, command_name, client_sid_hash, idempotency_key,
                     payload_digest_blake3, state, accepted_at_ms, expires_at_ms, created_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'accepted', ?6, ?7, ?6)",
                params![
                    request.request_id.as_str(),
                    request.command_name,
                    request.client_sid_hash,
                    request.idempotency_key,
                    request.payload_digest_blake3,
                    request.accepted_at_ms,
                    request.expires_at_ms
                ],
            )?;
            transaction.execute(
                "INSERT INTO plan_occurrences(
                     id, plan_revision_id, origin, scheduled_for_ms, requested_at_ms,
                     request_idempotency_key, state, missed_reason, created_at_ms, updated_at_ms
                 ) VALUES (?1, ?2, 'manual', NULL, ?3, ?4, 'pending', NULL, ?3, ?3)",
                params![
                    plan_occurrence_id.as_str(),
                    plan_revision_id.as_str(),
                    request.accepted_at_ms,
                    request.idempotency_key
                ],
            )?;

            let mut work_items = Vec::with_capacity(scopes.len());
            let mut activity_after_seq = 0_i64;
            for (
                target_id,
                target_name,
                engine,
                source_fingerprint,
                scope_id,
                scope_name,
                scope_mode,
            ) in scopes
            {
                let target_id = DatabaseTargetId::from_str(&target_id)
                    .map_err(|_| RepositoryError::RuntimeStateInvalid)?;
                let scope_id = BackupScopeId::from_str(&scope_id)
                    .map_err(|_| RepositoryError::RuntimeStateInvalid)?;
                let backup_run_id = BackupRunId::from_stable_name(
                    uuid::Uuid::NAMESPACE_URL,
                    format!("backup-run:{plan_occurrence_id}:{scope_id}").as_bytes(),
                );
                let work_item_id = WorkItemId::from_stable_name(
                    uuid::Uuid::NAMESPACE_URL,
                    format!("backup-work:{plan_occurrence_id}:{scope_id}").as_bytes(),
                );
                transaction.execute(
                    "INSERT INTO backup_runs(
                         id, plan_occurrence_id, plan_revision_id, database_target_id,
                         state, engine, source_fingerprint, created_at_ms, updated_at_ms
                     ) VALUES (?1, ?2, ?3, ?4, 'queued', ?5, ?6, ?7, ?7)",
                    params![
                        backup_run_id.as_str(),
                        plan_occurrence_id.as_str(),
                        plan_revision_id.as_str(),
                        target_id.as_str(),
                        engine,
                        source_fingerprint,
                        request.accepted_at_ms
                    ],
                )?;
                transaction.execute(
                    "INSERT INTO work_items(
                         id, kind, backup_run_id, priority, state,
                         available_at_ms, attempt_limit, created_at_ms, updated_at_ms
                     ) VALUES (?1, 'backup', ?2, 100, 'queued', ?3, 3, ?3, ?3)",
                    params![
                        work_item_id.as_str(),
                        backup_run_id.as_str(),
                        request.accepted_at_ms
                    ],
                )?;
                let attributes_json = serde_json::json!({
                    "backupRunId": backup_run_id.as_str(),
                    "engine": engine,
                    "planId": backup.plan_id.as_str(),
                    "planOccurrenceId": plan_occurrence_id.as_str(),
                    "planRevisionId": plan_revision_id.as_str(),
                    "targetId": target_id.as_str(),
                    "scopeId": scope_id.as_str(),
                    "scopeMode": scope_mode,
                })
                .to_string();
                let activity_id = uuid::Uuid::new_v5(
                    &uuid::Uuid::NAMESPACE_URL,
                    format!("manual-backup:{}:{scope_id}", request.request_id.as_str()).as_bytes(),
                )
                .to_string();
                transaction.execute(
                    "INSERT INTO activity_events(
                         id, occurred_at_ms, severity, event_code,
                         entity_type, entity_id, work_item_id, message,
                         attributes_json, redaction_version, created_at_ms
                     ) VALUES (
                         ?1, ?2, 'info', 'BACKUP_QUEUED',
                         'backup_run', ?3, ?4,
                         'The service accepted a manual backup target.',
                         ?5, 1, ?2
                     )",
                    params![
                        activity_id,
                        request.accepted_at_ms,
                        backup_run_id.as_str(),
                        work_item_id.as_str(),
                        attributes_json
                    ],
                )?;
                activity_after_seq = transaction.last_insert_rowid();
                work_items.push(ManualBackupWorkItem {
                    backup_run_id,
                    work_item_id,
                    target_id,
                    target_name,
                    scope_id,
                    scope_name,
                    scope_mode,
                    engine,
                });
            }
            transaction.execute(
                "UPDATE plan_occurrences
                 SET state = 'enqueued', updated_at_ms = ?2
                 WHERE id = ?1 AND state = 'pending'",
                params![plan_occurrence_id.as_str(), request.accepted_at_ms],
            )?;
            let stored = ManualBackupStoredResponseV1 {
                schema_version: 1,
                plan_id: backup.plan_id.as_str().to_owned(),
                plan_revision_id: plan_revision_id.as_str().to_owned(),
                plan_occurrence_id: plan_occurrence_id.as_str().to_owned(),
                plan_name: plan_name.clone(),
                state: "queued".to_owned(),
                work_items: work_items
                    .iter()
                    .map(|item| ManualBackupStoredWorkItemV1 {
                        backup_run_id: item.backup_run_id.as_str().to_owned(),
                        work_item_id: item.work_item_id.as_str().to_owned(),
                        target_id: item.target_id.as_str().to_owned(),
                        target_name: item.target_name.clone(),
                        scope_id: Some(item.scope_id.as_str().to_owned()),
                        scope_name: Some(item.scope_name.clone()),
                        scope_mode: Some(item.scope_mode.clone()),
                        engine: item.engine.clone(),
                        state: "queued".to_owned(),
                    })
                    .collect(),
                activity_after_seq,
            };
            let response_json =
                serde_json::to_string(&stored).map_err(|_| RepositoryError::RuntimeStateInvalid)?;
            let completed = transaction.execute(
                "UPDATE ipc_request_ledger
                 SET state = 'succeeded', result_ref_type = 'plan_occurrence',
                     result_ref_id = ?2, response_json = ?3, completed_at_ms = ?4
                 WHERE request_id = ?1
                   AND state = 'accepted'
                   AND payload_digest_blake3 = ?5",
                params![
                    request.request_id.as_str(),
                    plan_occurrence_id.as_str(),
                    response_json,
                    request.accepted_at_ms,
                    request.payload_digest_blake3
                ],
            )?;
            if completed != 1 {
                return Err(RepositoryError::IpcRequestIncomplete);
            }
            transaction.commit()?;
            Ok(ManualBackupEnqueueResult {
                created: true,
                plan_id: backup.plan_id,
                plan_revision_id,
                plan_occurrence_id,
                plan_name,
                work_items,
                activity_after_seq,
            })
        })
    }

    pub fn set_retention_safety_control(
        &self,
        request: IpcRequestSpec,
        control: RetentionSafetyControlSpec,
        completion: IpcCompletionSpec,
    ) -> Result<IpcAtomicMutationResult, RepositoryError> {
        let workspace_id = WorkspaceId::from_str(DEFAULT_WORKSPACE_ID)
            .expect("the built-in workspace ID is valid");
        self.set_retention_safety_control_in_workspace(&workspace_id, request, control, completion)
    }

    pub fn set_retention_safety_control_in_workspace(
        &self,
        workspace_id: &WorkspaceId,
        request: IpcRequestSpec,
        control: RetentionSafetyControlSpec,
        completion: IpcCompletionSpec,
    ) -> Result<IpcAtomicMutationResult, RepositoryError> {
        request.validate()?;
        control.validate()?;
        completion.validate()?;
        if request.command_name != "retention.control_set"
            || control.updated_at_ms() != request.accepted_at_ms
            || !completion.succeeded
            || completion.result_ref_type.is_some()
            || completion.completed_at_ms != request.accepted_at_ms
        {
            return Err(RepositoryError::SecurityInputInvalid);
        }
        let stored = completion.stored();
        let workspace_id = workspace_id.clone();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let existing: Option<ExistingLedgerRow> = transaction
                .query_row(
                    "SELECT payload_digest_blake3, state, response_json,
                            result_ref_type, result_ref_id
                     FROM ipc_request_ledger
                     WHERE client_sid_hash = ?1
                       AND command_name = ?2
                       AND idempotency_key = ?3",
                    params![
                        request.client_sid_hash,
                        request.command_name,
                        request.idempotency_key
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
            if let Some((payload_digest, state, response_json, result_ref_type, result_ref_id)) =
                existing
            {
                if payload_digest != request.payload_digest_blake3 {
                    return Err(RepositoryError::IpcIdempotencyConflict);
                }
                if state != "succeeded" || result_ref_type.is_some() || result_ref_id.is_some() {
                    return Err(RepositoryError::IpcRequestIncomplete);
                }
                let response_json = response_json.ok_or(RepositoryError::IpcRequestIncomplete)?;
                transaction.commit()?;
                return Ok(IpcAtomicMutationResult {
                    created: false,
                    outcome: IpcStoredOutcome {
                        succeeded: true,
                        response_json,
                        result_ref_type: None,
                        result_ref_id: None,
                    },
                });
            }

            ensure_workspace_recovery_point_in_transaction(
                &transaction,
                &workspace_id,
                control.recovery_point_id(),
            )?;
            transaction.execute(
                "INSERT INTO ipc_request_ledger(
                     request_id, command_name, client_sid_hash, idempotency_key,
                     payload_digest_blake3, state, accepted_at_ms, expires_at_ms, created_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'accepted', ?6, ?7, ?6)",
                params![
                    request.request_id.as_str(),
                    request.command_name,
                    request.client_sid_hash,
                    request.idempotency_key,
                    request.payload_digest_blake3,
                    request.accepted_at_ms,
                    request.expires_at_ms
                ],
            )?;
            apply_safety_control(&transaction, &control)?;
            let completed = transaction.execute(
                "UPDATE ipc_request_ledger
                 SET state = 'succeeded', response_json = ?2, completed_at_ms = ?3
                 WHERE request_id = ?1
                   AND state = 'accepted'
                   AND payload_digest_blake3 = ?4",
                params![
                    request.request_id.as_str(),
                    completion.response_json,
                    completion.completed_at_ms,
                    request.payload_digest_blake3
                ],
            )?;
            if completed != 1 {
                return Err(RepositoryError::IpcRequestIncomplete);
            }
            transaction.commit()?;
            Ok(IpcAtomicMutationResult {
                created: true,
                outcome: stored,
            })
        })
    }

    pub fn stage_retention_tombstone_request(
        &self,
        request: IpcRequestSpec,
        tombstone: RetentionTombstoneSpec,
        completion: IpcCompletionSpec,
    ) -> Result<IpcAtomicMutationResult, RepositoryError> {
        let workspace_id = WorkspaceId::from_str(DEFAULT_WORKSPACE_ID)
            .expect("the built-in workspace ID is valid");
        self.stage_retention_tombstone_request_in_workspace(
            &workspace_id,
            request,
            tombstone,
            completion,
        )
    }

    pub fn stage_retention_tombstone_request_in_workspace(
        &self,
        workspace_id: &WorkspaceId,
        request: IpcRequestSpec,
        tombstone: RetentionTombstoneSpec,
        completion: IpcCompletionSpec,
    ) -> Result<IpcAtomicMutationResult, RepositoryError> {
        request.validate()?;
        tombstone.validate()?;
        completion.validate()?;
        if request.command_name != "retention.tombstone"
            || tombstone.tombstoned_at_ms != request.accepted_at_ms
            || !completion.succeeded
            || completion.result_ref_type.as_deref() != Some("operation_run")
            || completion.result_ref_id.as_deref() != Some(tombstone.operation_run_id.as_str())
            || completion.completed_at_ms != request.accepted_at_ms
        {
            return Err(RepositoryError::SecurityInputInvalid);
        }
        let stored = completion.stored();
        let workspace_id = workspace_id.clone();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let existing: Option<ExistingLedgerRow> = transaction
                .query_row(
                    "SELECT payload_digest_blake3, state, response_json,
                            result_ref_type, result_ref_id
                     FROM ipc_request_ledger
                     WHERE client_sid_hash = ?1
                       AND command_name = ?2
                       AND idempotency_key = ?3",
                    params![
                        request.client_sid_hash,
                        request.command_name,
                        request.idempotency_key
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
            if let Some((payload_digest, state, response_json, result_ref_type, result_ref_id)) =
                existing
            {
                if payload_digest != request.payload_digest_blake3 {
                    return Err(RepositoryError::IpcIdempotencyConflict);
                }
                if state != "succeeded"
                    || result_ref_type.as_deref() != Some("operation_run")
                    || result_ref_id
                        .as_deref()
                        .and_then(|value| OperationRunId::from_str(value).ok())
                        .is_none()
                {
                    return Err(RepositoryError::IpcRequestIncomplete);
                }
                let response_json = response_json.ok_or(RepositoryError::IpcRequestIncomplete)?;
                transaction.commit()?;
                return Ok(IpcAtomicMutationResult {
                    created: false,
                    outcome: IpcStoredOutcome {
                        succeeded: true,
                        response_json,
                        result_ref_type,
                        result_ref_id,
                    },
                });
            }

            ensure_workspace_recovery_point_in_transaction(
                &transaction,
                &workspace_id,
                &tombstone.recovery_point_id,
            )?;
            transaction.execute(
                "INSERT INTO ipc_request_ledger(
                     request_id, command_name, client_sid_hash, idempotency_key,
                     payload_digest_blake3, state, accepted_at_ms, expires_at_ms, created_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'accepted', ?6, ?7, ?6)",
                params![
                    request.request_id.as_str(),
                    request.command_name,
                    request.client_sid_hash,
                    request.idempotency_key,
                    request.payload_digest_blake3,
                    request.accepted_at_ms,
                    request.expires_at_ms
                ],
            )?;
            let plan = prepare_delete_plan_in_transaction(
                &transaction,
                &tombstone.recovery_point_id,
                &tombstone.destination_fingerprint,
                tombstone.tombstoned_at_ms,
                tombstone.grace_period_ms,
            )
            .map_err(|error| match error {
                RepositoryError::RetentionAuthorityInvalid => {
                    RepositoryError::RetentionTombstoneInvalid
                }
                error => error,
            })?;
            if plan.decision().disposition != RetentionDispositionV2::TombstoneRequired {
                return Err(RepositoryError::RetentionTombstoneInvalid);
            }
            stage_retention_tombstone_in_transaction(&transaction, &tombstone)?;
            let completed = transaction.execute(
                "UPDATE ipc_request_ledger
                 SET state = 'succeeded', result_ref_type = ?2,
                     result_ref_id = ?3, response_json = ?4, completed_at_ms = ?5
                 WHERE request_id = ?1
                   AND state = 'accepted'
                   AND payload_digest_blake3 = ?6",
                params![
                    request.request_id.as_str(),
                    completion.result_ref_type,
                    completion.result_ref_id,
                    completion.response_json,
                    completion.completed_at_ms,
                    request.payload_digest_blake3
                ],
            )?;
            if completed != 1 {
                return Err(RepositoryError::IpcRequestIncomplete);
            }
            transaction.commit()?;
            Ok(IpcAtomicMutationResult {
                created: true,
                outcome: stored,
            })
        })
    }

    pub fn issue_retention_confirmation_request(
        &self,
        request: IpcRequestSpec,
        confirmation: RetentionConfirmationRequestSpec,
    ) -> Result<RetentionConfirmationIssueResult, RepositoryError> {
        let workspace_id = WorkspaceId::from_str(DEFAULT_WORKSPACE_ID)
            .expect("the built-in workspace ID is valid");
        self.issue_retention_confirmation_request_in_workspace(&workspace_id, request, confirmation)
    }

    pub fn issue_retention_confirmation_request_in_workspace(
        &self,
        workspace_id: &WorkspaceId,
        request: IpcRequestSpec,
        confirmation: RetentionConfirmationRequestSpec,
    ) -> Result<RetentionConfirmationIssueResult, RepositoryError> {
        request.validate()?;
        confirmation.validate(request.accepted_at_ms)?;
        if request.command_name != "confirmation.prepare" {
            return Err(RepositoryError::SecurityInputInvalid);
        }
        let workspace_id = workspace_id.clone();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let existing: Option<ExistingLedgerRow> = transaction
                .query_row(
                    "SELECT payload_digest_blake3, state, response_json,
                            result_ref_type, result_ref_id
                     FROM ipc_request_ledger
                     WHERE client_sid_hash = ?1
                       AND command_name = ?2
                       AND idempotency_key = ?3",
                    params![
                        request.client_sid_hash,
                        request.command_name,
                        request.idempotency_key
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
            if let Some((payload_digest, state, response_json, result_ref_type, result_ref_id)) =
                existing
            {
                if payload_digest != request.payload_digest_blake3 {
                    return Err(RepositoryError::IpcIdempotencyConflict);
                }
                if state != "succeeded" || result_ref_type.as_deref() != Some("confirmation_token")
                {
                    return Err(RepositoryError::IpcRequestIncomplete);
                }
                let response_json = response_json.ok_or(RepositoryError::IpcRequestIncomplete)?;
                let result_ref_id = result_ref_id.ok_or(RepositoryError::IpcRequestIncomplete)?;
                let confirmation_token_id = ConfirmationTokenId::from_str(&result_ref_id)
                    .map_err(|_| RepositoryError::IpcRequestIncomplete)?;
                let stored =
                    serde_json::from_str::<RetentionConfirmationStoredResponseV1>(&response_json)
                        .map_err(|_| RepositoryError::IpcRequestIncomplete)?;
                let authorization_json = serde_json::to_string(&stored.authorization)
                    .map_err(|_| RepositoryError::IpcRequestIncomplete)?;
                let authorization = RetentionDeleteAuthorizationV2::from_json(&authorization_json)
                    .map_err(|_| RepositoryError::IpcRequestIncomplete)?;
                if stored.schema_version != 1
                    || stored.confirmation_token_id != confirmation_token_id.as_str()
                    || stored.action != "retention_delete"
                    || stored.token_available
                    || stored.payload_digest_blake3 != authorization.digest_blake3()
                    || authorization.recovery_point_id() != confirmation.recovery_point_id.as_uuid()
                    || authorization.destination_fingerprint()
                        != confirmation.destination_fingerprint
                    || authorization.evaluated_at_ms() != stored.issued_at_ms
                    || authorization.grace_period_ms() != confirmation.grace_period_ms
                    || stored.expires_at_ms <= stored.issued_at_ms
                {
                    return Err(RepositoryError::IpcRequestIncomplete);
                }
                let persisted_token_digest: Option<String> = transaction
                    .query_row(
                        "SELECT token_digest_blake3
                         FROM confirmation_tokens
                         WHERE id = ?1
                           AND action = 'retention_delete'
                           AND client_sid_hash = ?2
                           AND resource_type = 'artifact_replica'
                           AND resource_id = ?3
                           AND resource_fingerprint = ?4
                           AND payload_digest_blake3 = ?5
                           AND issued_at_ms = ?6
                           AND expires_at_ms = ?7",
                        params![
                            confirmation_token_id.as_str(),
                            request.client_sid_hash,
                            confirmation.recovery_point_id.as_str(),
                            confirmation.destination_fingerprint,
                            stored.payload_digest_blake3,
                            stored.issued_at_ms,
                            stored.expires_at_ms
                        ],
                        |row| row.get(0),
                    )
                    .optional()?;
                if !persisted_token_digest.as_deref().is_some_and(is_digest) {
                    return Err(RepositoryError::IpcRequestIncomplete);
                }
                transaction.commit()?;
                return Ok(RetentionConfirmationIssueResult {
                    created: false,
                    confirmation_token_id,
                    authorization,
                    issued_at_ms: stored.issued_at_ms,
                    expires_at_ms: stored.expires_at_ms,
                });
            }

            ensure_workspace_recovery_point_in_transaction(
                &transaction,
                &workspace_id,
                &confirmation.recovery_point_id,
            )?;
            transaction.execute(
                "INSERT INTO ipc_request_ledger(
                     request_id, command_name, client_sid_hash, idempotency_key,
                     payload_digest_blake3, state, accepted_at_ms, expires_at_ms, created_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'accepted', ?6, ?7, ?6)",
                params![
                    request.request_id.as_str(),
                    request.command_name,
                    request.client_sid_hash,
                    request.idempotency_key,
                    request.payload_digest_blake3,
                    request.accepted_at_ms,
                    request.expires_at_ms
                ],
            )?;
            let plan = prepare_delete_plan_in_transaction(
                &transaction,
                &confirmation.recovery_point_id,
                &confirmation.destination_fingerprint,
                request.accepted_at_ms,
                confirmation.grace_period_ms,
            )
            .map_err(|error| match error {
                RepositoryError::RetentionAuthorityInvalid => {
                    RepositoryError::RetentionTombstoneInvalid
                }
                error => error,
            })?;
            if plan.decision().disposition != RetentionDispositionV2::DeleteEligible {
                return Err(RepositoryError::RetentionTombstoneInvalid);
            }
            let authorization = plan.authorization().clone();
            let authorization_json = authorization
                .to_json()
                .map_err(|_| RepositoryError::RetentionAuthorityInvalid)?;
            let authorization_value = serde_json::from_str(&authorization_json)
                .map_err(|_| RepositoryError::RetentionAuthorityInvalid)?;
            let stored = RetentionConfirmationStoredResponseV1 {
                schema_version: 1,
                confirmation_token_id: confirmation.confirmation_token_id.as_str().to_owned(),
                action: "retention_delete".to_owned(),
                issued_at_ms: request.accepted_at_ms,
                expires_at_ms: confirmation.expires_at_ms,
                token_available: false,
                payload_digest_blake3: authorization.digest_blake3().to_owned(),
                authorization: authorization_value,
            };
            let response_json = serde_json::to_string(&stored)
                .map_err(|_| RepositoryError::SecurityInputInvalid)?;
            if response_json.len() > MAX_STORED_RESPONSE_BYTES {
                return Err(RepositoryError::SecurityInputInvalid);
            }
            let confirmation_record = ConfirmationSpec::new(
                confirmation.confirmation_token_id.clone(),
                confirmation.token_digest_blake3,
                "retention_delete",
                request.client_sid_hash.clone(),
                "artifact_replica",
                confirmation.recovery_point_id.as_str(),
                confirmation.destination_fingerprint,
                authorization.digest_blake3(),
                request.accepted_at_ms,
                confirmation.expires_at_ms,
            );
            confirmation_record.validate()?;
            insert_confirmation_in_transaction(&transaction, &confirmation_record)?;
            let completed = transaction.execute(
                "UPDATE ipc_request_ledger
                 SET state = 'succeeded', result_ref_type = ?2,
                     result_ref_id = ?3, response_json = ?4, completed_at_ms = ?5
                 WHERE request_id = ?1
                   AND state = 'accepted'
                   AND payload_digest_blake3 = ?6",
                params![
                    request.request_id.as_str(),
                    "confirmation_token",
                    confirmation.confirmation_token_id.as_str(),
                    response_json,
                    request.accepted_at_ms,
                    request.payload_digest_blake3
                ],
            )?;
            if completed != 1 {
                return Err(RepositoryError::IpcRequestIncomplete);
            }
            transaction.commit()?;
            Ok(RetentionConfirmationIssueResult {
                created: true,
                confirmation_token_id: confirmation.confirmation_token_id,
                authorization,
                issued_at_ms: request.accepted_at_ms,
                expires_at_ms: confirmation.expires_at_ms,
            })
        })
    }

    pub fn issue_restore_overwrite_confirmation_request(
        &self,
        request: IpcRequestSpec,
        confirmation: RestoreOverwriteConfirmationRequestSpec,
    ) -> Result<RestoreOverwriteConfirmationIssueResult, RepositoryError> {
        request.validate()?;
        confirmation.validate(request.accepted_at_ms)?;
        if request.command_name != "confirmation.prepare" {
            return Err(RepositoryError::SecurityInputInvalid);
        }
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let existing: Option<ExistingLedgerRow> = transaction
                .query_row(
                    "SELECT payload_digest_blake3, state, response_json,
                            result_ref_type, result_ref_id
                     FROM ipc_request_ledger
                     WHERE client_sid_hash = ?1
                       AND command_name = ?2
                       AND idempotency_key = ?3",
                    params![
                        request.client_sid_hash,
                        request.command_name,
                        request.idempotency_key
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
            if let Some((payload_digest, state, response_json, result_ref_type, result_ref_id)) =
                existing
            {
                if payload_digest != request.payload_digest_blake3 {
                    return Err(RepositoryError::IpcIdempotencyConflict);
                }
                if state != "succeeded" || result_ref_type.as_deref() != Some("confirmation_token")
                {
                    return Err(RepositoryError::IpcRequestIncomplete);
                }
                let response_json = response_json.ok_or(RepositoryError::IpcRequestIncomplete)?;
                let result_ref_id = result_ref_id.ok_or(RepositoryError::IpcRequestIncomplete)?;
                let confirmation_token_id = ConfirmationTokenId::from_str(&result_ref_id)
                    .map_err(|_| RepositoryError::IpcRequestIncomplete)?;
                let stored = serde_json::from_str::<RestoreOverwriteConfirmationStoredResponseV1>(
                    &response_json,
                )
                .map_err(|_| RepositoryError::IpcRequestIncomplete)?;
                let recovery_point_id = RecoveryPointId::from_str(&stored.recovery_point_id)
                    .map_err(|_| RepositoryError::IpcRequestIncomplete)?;
                let target_connection_profile_id =
                    ConnectionProfileId::from_str(&stored.target_connection_profile_id)
                        .map_err(|_| RepositoryError::IpcRequestIncomplete)?;
                if stored.schema_version != 1
                    || stored.confirmation_token_id != confirmation_token_id.as_str()
                    || stored.action != "restore_overwrite"
                    || stored.token_available
                    || recovery_point_id != confirmation.recovery_point_id
                    || stored.source_replica_id != confirmation.source_replica_id
                    || target_connection_profile_id != confirmation.target_connection_profile_id
                    || stored.target_database_name != confirmation.target_database_name
                    || stored.target_fingerprint != confirmation.target_fingerprint
                    || stored.preflight_version != confirmation.preflight_version
                    || stored.required_verification_level
                        != confirmation.required_verification_level
                    || stored.issued_at_ms < 0
                    || stored.expires_at_ms <= stored.issued_at_ms
                    || !is_digest(&stored.payload_digest_blake3)
                {
                    return Err(RepositoryError::IpcRequestIncomplete);
                }
                let persisted_token_digest: Option<String> = transaction
                    .query_row(
                        "SELECT token_digest_blake3
                         FROM confirmation_tokens
                         WHERE id = ?1
                           AND action = 'restore_overwrite'
                           AND client_sid_hash = ?2
                           AND resource_type = 'restore_target'
                           AND resource_id = ?3
                           AND resource_fingerprint = ?4
                           AND payload_digest_blake3 = ?5
                           AND issued_at_ms = ?6
                           AND expires_at_ms = ?7",
                        params![
                            confirmation_token_id.as_str(),
                            request.client_sid_hash,
                            stored.target_database_name,
                            stored.target_fingerprint,
                            stored.payload_digest_blake3,
                            stored.issued_at_ms,
                            stored.expires_at_ms
                        ],
                        |row| row.get(0),
                    )
                    .optional()?;
                if !persisted_token_digest.as_deref().is_some_and(is_digest) {
                    return Err(RepositoryError::IpcRequestIncomplete);
                }
                transaction.commit()?;
                return Ok(RestoreOverwriteConfirmationIssueResult {
                    created: false,
                    confirmation_token_id,
                    recovery_point_id,
                    source_replica_id: stored.source_replica_id,
                    target_connection_profile_id,
                    target_database_name: stored.target_database_name,
                    target_fingerprint: stored.target_fingerprint,
                    preflight_version: stored.preflight_version,
                    required_verification_level: stored.required_verification_level,
                    payload_digest_blake3: stored.payload_digest_blake3,
                    issued_at_ms: stored.issued_at_ms,
                    expires_at_ms: stored.expires_at_ms,
                });
            }

            ensure_restore_source_target_in_transaction(
                &transaction,
                &confirmation.recovery_point_id,
                &confirmation.source_replica_id,
                &confirmation.target_connection_profile_id,
            )?;
            transaction.execute(
                "INSERT INTO ipc_request_ledger(
                     request_id, command_name, client_sid_hash, idempotency_key,
                     payload_digest_blake3, state, accepted_at_ms, expires_at_ms, created_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'accepted', ?6, ?7, ?6)",
                params![
                    request.request_id.as_str(),
                    request.command_name,
                    request.client_sid_hash,
                    request.idempotency_key,
                    request.payload_digest_blake3,
                    request.accepted_at_ms,
                    request.expires_at_ms
                ],
            )?;
            let stored = RestoreOverwriteConfirmationStoredResponseV1 {
                schema_version: 1,
                confirmation_token_id: confirmation.confirmation_token_id.as_str().to_owned(),
                action: "restore_overwrite".to_owned(),
                recovery_point_id: confirmation.recovery_point_id.as_str().to_owned(),
                source_replica_id: confirmation.source_replica_id.clone(),
                target_connection_profile_id: confirmation
                    .target_connection_profile_id
                    .as_str()
                    .to_owned(),
                target_database_name: confirmation.target_database_name.clone(),
                target_fingerprint: confirmation.target_fingerprint.clone(),
                preflight_version: confirmation.preflight_version.clone(),
                required_verification_level: confirmation.required_verification_level,
                issued_at_ms: request.accepted_at_ms,
                expires_at_ms: confirmation.expires_at_ms,
                token_available: false,
                payload_digest_blake3: confirmation.payload_digest_blake3.clone(),
            };
            let response_json = serde_json::to_string(&stored)
                .map_err(|_| RepositoryError::SecurityInputInvalid)?;
            if response_json.len() > MAX_STORED_RESPONSE_BYTES {
                return Err(RepositoryError::SecurityInputInvalid);
            }
            let confirmation_record = ConfirmationSpec::restore_overwrite(
                confirmation.confirmation_token_id.clone(),
                confirmation.token_digest_blake3,
                request.client_sid_hash.clone(),
                "restore_target",
                confirmation.target_database_name.clone(),
                confirmation.target_fingerprint.clone(),
                confirmation.payload_digest_blake3.clone(),
                request.accepted_at_ms,
                confirmation.expires_at_ms,
            );
            confirmation_record.validate()?;
            insert_confirmation_in_transaction(&transaction, &confirmation_record)?;
            let completed = transaction.execute(
                "UPDATE ipc_request_ledger
                 SET state = 'succeeded', result_ref_type = 'confirmation_token',
                     result_ref_id = ?2, response_json = ?3, completed_at_ms = ?4
                 WHERE request_id = ?1
                   AND state = 'accepted'
                   AND payload_digest_blake3 = ?5",
                params![
                    request.request_id.as_str(),
                    confirmation.confirmation_token_id.as_str(),
                    response_json,
                    request.accepted_at_ms,
                    request.payload_digest_blake3
                ],
            )?;
            if completed != 1 {
                return Err(RepositoryError::IpcRequestIncomplete);
            }
            transaction.commit()?;
            Ok(RestoreOverwriteConfirmationIssueResult {
                created: true,
                confirmation_token_id: confirmation.confirmation_token_id,
                recovery_point_id: confirmation.recovery_point_id,
                source_replica_id: confirmation.source_replica_id,
                target_connection_profile_id: confirmation.target_connection_profile_id,
                target_database_name: confirmation.target_database_name,
                target_fingerprint: confirmation.target_fingerprint,
                preflight_version: confirmation.preflight_version,
                required_verification_level: confirmation.required_verification_level,
                payload_digest_blake3: confirmation.payload_digest_blake3,
                issued_at_ms: request.accepted_at_ms,
                expires_at_ms: confirmation.expires_at_ms,
            })
        })
    }

    pub fn issue_config_cutover_confirmation_request(
        &self,
        request: IpcRequestSpec,
        confirmation: ConfigCutoverConfirmationRequestSpec,
    ) -> Result<ConfigCutoverConfirmationIssueResult, RepositoryError> {
        request.validate()?;
        confirmation.validate(request.accepted_at_ms)?;
        if request.command_name != "confirmation.prepare" {
            return Err(RepositoryError::CutoverInputInvalid);
        }
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let existing: Option<ExistingLedgerRow> = transaction
                .query_row(
                    "SELECT payload_digest_blake3, state, response_json,
                            result_ref_type, result_ref_id
                     FROM ipc_request_ledger
                     WHERE client_sid_hash = ?1
                       AND command_name = ?2
                       AND idempotency_key = ?3",
                    params![
                        request.client_sid_hash,
                        request.command_name,
                        request.idempotency_key
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
            if let Some((payload_digest, state, response_json, result_ref_type, result_ref_id)) =
                existing
            {
                if payload_digest != request.payload_digest_blake3 {
                    return Err(RepositoryError::IpcIdempotencyConflict);
                }
                if state != "succeeded" || result_ref_type.as_deref() != Some("confirmation_token")
                {
                    return Err(RepositoryError::IpcRequestIncomplete);
                }
                let response_json = response_json.ok_or(RepositoryError::IpcRequestIncomplete)?;
                let result_ref_id = result_ref_id.ok_or(RepositoryError::IpcRequestIncomplete)?;
                let confirmation_token_id = ConfirmationTokenId::from_str(&result_ref_id)
                    .map_err(|_| RepositoryError::IpcRequestIncomplete)?;
                let stored = serde_json::from_str::<ConfigCutoverConfirmationStoredResponseV1>(
                    &response_json,
                )
                .map_err(|_| RepositoryError::IpcRequestIncomplete)?;
                let cutover_run_id = uuid::Uuid::parse_str(&stored.cutover_run_id)
                    .map_err(|_| RepositoryError::IpcRequestIncomplete)?;
                let shadow_import_id = uuid::Uuid::parse_str(&stored.shadow_import_id)
                    .map_err(|_| RepositoryError::IpcRequestIncomplete)?;
                if stored.schema_version != 1
                    || stored.confirmation_token_id != confirmation_token_id.as_str()
                    || stored.action != "config_cutover"
                    || stored.token_available
                    || cutover_run_id != confirmation.cutover_run_id
                    || shadow_import_id != confirmation.shadow_import_id
                    || stored.cutover_marker != confirmation.cutover_marker
                    || stored.issued_at_ms < 0
                    || stored.expires_at_ms <= stored.issued_at_ms
                {
                    return Err(RepositoryError::IpcRequestIncomplete);
                }
                let persisted_token_digest: Option<String> = transaction
                    .query_row(
                        "SELECT token_digest_blake3
                         FROM confirmation_tokens
                         WHERE id = ?1
                           AND action = 'config_cutover'
                           AND client_sid_hash = ?2
                           AND resource_type = 'config_import_run'
                           AND resource_id = ?3
                           AND resource_fingerprint = ?4
                           AND payload_digest_blake3 = ?5
                           AND issued_at_ms = ?6
                           AND expires_at_ms = ?7",
                        params![
                            confirmation_token_id.as_str(),
                            request.client_sid_hash,
                            stored.shadow_import_id,
                            stored.cutover_marker,
                            stored.payload_digest_blake3,
                            stored.issued_at_ms,
                            stored.expires_at_ms
                        ],
                        |row| row.get(0),
                    )
                    .optional()?;
                if !persisted_token_digest.as_deref().is_some_and(is_digest) {
                    return Err(RepositoryError::IpcRequestIncomplete);
                }
                transaction.commit()?;
                return Ok(ConfigCutoverConfirmationIssueResult {
                    created: false,
                    confirmation_token_id,
                    cutover_run_id,
                    shadow_import_id,
                    cutover_marker: stored.cutover_marker,
                    payload_digest_blake3: stored.payload_digest_blake3,
                    issued_at_ms: stored.issued_at_ms,
                    expires_at_ms: stored.expires_at_ms,
                });
            }

            transaction.execute(
                "INSERT INTO ipc_request_ledger(
                     request_id, command_name, client_sid_hash, idempotency_key,
                     payload_digest_blake3, state, accepted_at_ms, expires_at_ms, created_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'accepted', ?6, ?7, ?6)",
                params![
                    request.request_id.as_str(),
                    request.command_name,
                    request.client_sid_hash,
                    request.idempotency_key,
                    request.payload_digest_blake3,
                    request.accepted_at_ms,
                    request.expires_at_ms
                ],
            )?;

            let stored = ConfigCutoverConfirmationStoredResponseV1 {
                schema_version: 1,
                confirmation_token_id: confirmation.confirmation_token_id.as_str().to_owned(),
                action: "config_cutover".to_owned(),
                cutover_run_id: confirmation.cutover_run_id.to_string(),
                shadow_import_id: confirmation.shadow_import_id.to_string(),
                cutover_marker: confirmation.cutover_marker.clone(),
                issued_at_ms: request.accepted_at_ms,
                expires_at_ms: confirmation.expires_at_ms,
                token_available: false,
                payload_digest_blake3: confirmation.payload_digest_blake3.clone(),
            };
            let response_json =
                serde_json::to_string(&stored).map_err(|_| RepositoryError::CutoverInputInvalid)?;
            if response_json.len() > MAX_STORED_RESPONSE_BYTES {
                return Err(RepositoryError::CutoverInputInvalid);
            }
            let confirmation_record = ConfirmationSpec::new(
                confirmation.confirmation_token_id.clone(),
                confirmation.token_digest_blake3,
                "config_cutover",
                request.client_sid_hash.clone(),
                "config_import_run",
                confirmation.shadow_import_id.to_string(),
                confirmation.cutover_marker,
                confirmation.payload_digest_blake3,
                request.accepted_at_ms,
                confirmation.expires_at_ms,
            );
            confirmation_record.validate()?;
            insert_confirmation_in_transaction(&transaction, &confirmation_record)?;
            let completed = transaction.execute(
                "UPDATE ipc_request_ledger
                 SET state = 'succeeded', result_ref_type = 'confirmation_token',
                     result_ref_id = ?2, response_json = ?3, completed_at_ms = ?4
                 WHERE request_id = ?1
                   AND state = 'accepted'
                   AND payload_digest_blake3 = ?5",
                params![
                    request.request_id.as_str(),
                    confirmation.confirmation_token_id.as_str(),
                    response_json,
                    request.accepted_at_ms,
                    request.payload_digest_blake3
                ],
            )?;
            if completed != 1 {
                return Err(RepositoryError::IpcRequestIncomplete);
            }
            transaction.commit()?;
            Ok(ConfigCutoverConfirmationIssueResult {
                created: true,
                confirmation_token_id: confirmation.confirmation_token_id,
                cutover_run_id: confirmation.cutover_run_id,
                shadow_import_id: confirmation.shadow_import_id,
                cutover_marker: stored.cutover_marker,
                payload_digest_blake3: stored.payload_digest_blake3,
                issued_at_ms: request.accepted_at_ms,
                expires_at_ms: confirmation.expires_at_ms,
            })
        })
    }

    pub fn issue_confirmation(&self, spec: ConfirmationSpec) -> Result<(), RepositoryError> {
        spec.validate()?;
        self.write(move |connection| {
            let transaction = connection.transaction()?;
            insert_confirmation_in_transaction(&transaction, &spec)?;
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn confirmation(
        &self,
        confirmation_token_id: &ConfirmationTokenId,
    ) -> Result<ConfirmationRecord, RepositoryError> {
        let reader = self.store.open_reader()?;
        reader
            .connection()
            .query_row(
                "SELECT token_digest_blake3, action, client_sid_hash,
                        resource_type, resource_id, resource_fingerprint,
                        payload_digest_blake3, state, issued_at_ms, expires_at_ms
                 FROM confirmation_tokens
                 WHERE id = ?1",
                [confirmation_token_id.as_str()],
                |row| {
                    Ok(ConfirmationRecord {
                        id: confirmation_token_id.clone(),
                        token_digest_blake3: row.get(0)?,
                        action: row.get(1)?,
                        client_sid_hash: row.get(2)?,
                        resource_type: row.get(3)?,
                        resource_id: row.get(4)?,
                        resource_fingerprint: row.get(5)?,
                        payload_digest_blake3: row.get(6)?,
                        state: row.get(7)?,
                        issued_at_ms: row.get(8)?,
                        expires_at_ms: row.get(9)?,
                    })
                },
            )
            .optional()?
            .ok_or(RepositoryError::NotFound)
    }

    pub fn revoke_confirmation(
        &self,
        confirmation_token_id: &ConfirmationTokenId,
        client_sid_hash: &str,
        revoked_at_ms: i64,
    ) -> Result<(), RepositoryError> {
        if !is_digest(client_sid_hash) || revoked_at_ms < 0 {
            return Err(RepositoryError::SecurityInputInvalid);
        }
        let confirmation_token_id = confirmation_token_id.clone();
        let client_sid_hash = client_sid_hash.to_owned();
        self.write(move |connection| {
            let updated = connection.execute(
                "UPDATE confirmation_tokens
                 SET state = 'revoked'
                 WHERE id = ?1
                   AND client_sid_hash = ?2
                   AND state = 'issued'
                   AND issued_at_ms <= ?3
                   AND expires_at_ms > ?3",
                params![
                    confirmation_token_id.as_str(),
                    client_sid_hash,
                    revoked_at_ms
                ],
            )?;
            if updated != 1 {
                return Err(RepositoryError::ConfirmationInvalidOrReplayed);
            }
            Ok(())
        })
    }

    pub fn expire_confirmations(&self, now_ms: i64) -> Result<usize, RepositoryError> {
        if now_ms < 0 {
            return Err(RepositoryError::SecurityInputInvalid);
        }
        self.write(move |connection| {
            connection
                .execute(
                    "UPDATE confirmation_tokens
                     SET state = 'expired'
                     WHERE state = 'issued'
                       AND expires_at_ms <= ?1",
                    [now_ms],
                )
                .map_err(RepositoryError::from)
        })
    }

    pub fn commit_config_cutover(
        &self,
        request: IpcRequestSpec,
        confirmation_token_id: &ConfirmationTokenId,
        confirmation_token_digest: &str,
        cutover: ConfigCutoverSpec,
    ) -> Result<ConfigCutoverResult, RepositoryError> {
        request.validate()?;
        cutover.validate(request.accepted_at_ms, request.expires_at_ms)?;
        if request.command_name != "config.cutover" || !is_digest(confirmation_token_digest) {
            return Err(RepositoryError::CutoverInputInvalid);
        }
        let confirmation_token_id = confirmation_token_id.clone();
        let confirmation_token_digest = confirmation_token_digest.to_owned();
        let expected_marker = cutover.cutover_marker().to_owned();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let existing: Option<ExistingLedgerRow> = transaction
                .query_row(
                    "SELECT payload_digest_blake3, state, response_json,
                            result_ref_type, result_ref_id
                     FROM ipc_request_ledger
                     WHERE client_sid_hash = ?1
                       AND command_name = ?2
                       AND idempotency_key = ?3",
                    params![
                        request.client_sid_hash,
                        request.command_name,
                        request.idempotency_key
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
            if let Some((payload_digest, state, response_json, result_ref_type, result_ref_id)) =
                existing
            {
                if payload_digest != request.payload_digest_blake3 {
                    return Err(RepositoryError::IpcIdempotencyConflict);
                }
                let expected_response = expected_response_json(&expected_marker);
                if state != "succeeded"
                    || response_json.as_deref() != Some(expected_response.as_str())
                    || result_ref_type.as_deref() != Some("config_cutover")
                    || result_ref_id.as_deref() != Some(expected_marker.as_str())
                {
                    return Err(RepositoryError::IpcRequestIncomplete);
                }
                let authority = load_authority(&transaction)?;
                if authority.cutover_marker() != Some(expected_marker.as_str()) {
                    return Err(RepositoryError::CutoverStateInvalid);
                }
                transaction.commit()?;
                return Ok(ConfigCutoverResult::new(expected_marker, false));
            }

            ensure_cutover_not_committed(&transaction)?;
            let shadow_import_id = cutover.shadow_import_id();
            verify_shadow_import_for_cutover(
                &transaction,
                &shadow_import_id,
                cutover.source_checksum_blake3(),
                cutover.relation_digest_blake3(),
            )?;

            transaction.execute(
                "INSERT INTO ipc_request_ledger(
                     request_id, command_name, client_sid_hash, idempotency_key,
                     payload_digest_blake3, state, accepted_at_ms, expires_at_ms, created_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'accepted', ?6, ?7, ?6)",
                params![
                    request.request_id.as_str(),
                    request.command_name,
                    request.client_sid_hash,
                    request.idempotency_key,
                    request.payload_digest_blake3,
                    request.accepted_at_ms,
                    request.expires_at_ms
                ],
            )?;

            let consumed = transaction.execute(
                "UPDATE confirmation_tokens
                 SET state = 'consumed', consumed_at_ms = ?8, consumed_by_request_id = ?9
                 WHERE id = ?1
                   AND token_digest_blake3 = ?2
                   AND action = 'config_cutover'
                   AND client_sid_hash = ?3
                   AND resource_type = ?4
                   AND resource_id = ?5
                   AND resource_fingerprint = ?6
                   AND payload_digest_blake3 = ?7
                   AND state = 'issued'
                   AND issued_at_ms <= ?10
                   AND expires_at_ms > ?10",
                params![
                    confirmation_token_id.as_str(),
                    confirmation_token_digest,
                    request.client_sid_hash,
                    cutover.confirmation_resource_type(),
                    shadow_import_id,
                    expected_marker,
                    request.payload_digest_blake3,
                    cutover.committed_at_ms(),
                    request.request_id.as_str(),
                    request.accepted_at_ms,
                ],
            )?;
            if consumed != 1 {
                return Err(RepositoryError::ConfirmationInvalidOrReplayed);
            }

            insert_cutover_state(
                &transaction,
                request.request_id.as_str(),
                &request.payload_digest_blake3,
                request.accepted_at_ms,
                confirmation_token_id.as_str(),
                &cutover,
            )?;
            let response_json = expected_response_json(&expected_marker);
            let completed = transaction.execute(
                "UPDATE ipc_request_ledger
                 SET state = 'succeeded', result_ref_type = 'config_cutover',
                     result_ref_id = ?2, response_json = ?3, completed_at_ms = ?4
                 WHERE request_id = ?1
                   AND state = 'accepted'
                   AND payload_digest_blake3 = ?5",
                params![
                    request.request_id.as_str(),
                    expected_marker,
                    response_json,
                    cutover.committed_at_ms(),
                    request.payload_digest_blake3
                ],
            )?;
            if completed != 1 {
                return Err(RepositoryError::IpcRequestIncomplete);
            }
            let authority = load_authority(&transaction)?;
            if authority.cutover_marker() != Some(expected_marker.as_str()) {
                return Err(RepositoryError::CutoverStateInvalid);
            }
            transaction.commit()?;
            Ok(ConfigCutoverResult::new(expected_marker, true))
        })
    }

    pub fn enqueue_overwrite_restore(
        &self,
        request: IpcRequestSpec,
        confirmation_token_id: &ConfirmationTokenId,
        confirmation_token_digest: &str,
        restore: OverwriteRestoreSpec,
        completion: IpcCompletionSpec,
    ) -> Result<IpcAtomicMutationResult, RepositoryError> {
        request.validate()?;
        restore.validate()?;
        completion.validate()?;
        if request.command_name != "restore.enqueue_overwrite"
            || !is_digest(confirmation_token_digest)
            || restore.created_at_ms != request.accepted_at_ms
            || !completion.succeeded
            || completion.result_ref_type.as_deref() != Some("restore_run")
            || completion.result_ref_id.as_deref() != Some(restore.restore_run_id.as_str())
            || completion.completed_at_ms != request.accepted_at_ms
        {
            return Err(RepositoryError::SecurityInputInvalid);
        }
        let confirmation_token_id = confirmation_token_id.clone();
        let confirmation_token_digest = confirmation_token_digest.to_owned();
        let stored = completion.stored();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

            let existing: Option<ExistingLedgerRow> = transaction
                .query_row(
                    "SELECT payload_digest_blake3, state, response_json,
                            result_ref_type, result_ref_id
                     FROM ipc_request_ledger
                     WHERE client_sid_hash = ?1
                       AND command_name = ?2
                       AND idempotency_key = ?3",
                    params![
                        request.client_sid_hash,
                        request.command_name,
                        request.idempotency_key
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
            if let Some((payload_digest, state, response_json, result_ref_type, result_ref_id)) =
                existing
            {
                if payload_digest != request.payload_digest_blake3 {
                    return Err(RepositoryError::IpcIdempotencyConflict);
                }
                if state != "succeeded"
                    || result_ref_type.as_deref() != Some("restore_run")
                    || result_ref_id
                        .as_deref()
                        .and_then(|value| RestoreRunId::from_str(value).ok())
                        .is_none()
                {
                    return Err(RepositoryError::IpcRequestIncomplete);
                }
                let response_json = response_json.ok_or(RepositoryError::IpcRequestIncomplete)?;
                transaction.commit()?;
                return Ok(IpcAtomicMutationResult {
                    outcome: IpcStoredOutcome {
                        succeeded: true,
                        response_json,
                        result_ref_type,
                        result_ref_id,
                    },
                    created: false,
                });
            }

            ensure_restore_source_target_in_transaction(
                &transaction,
                &restore.recovery_point_id,
                &restore.source_replica_id,
                &restore.target_connection_profile_id,
            )?;
            transaction.execute(
                "INSERT INTO ipc_request_ledger(
                     request_id, command_name, client_sid_hash, idempotency_key,
                     payload_digest_blake3, state, accepted_at_ms, expires_at_ms, created_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'accepted', ?6, ?7, ?6)",
                params![
                    request.request_id.as_str(),
                    request.command_name,
                    request.client_sid_hash,
                    request.idempotency_key,
                    request.payload_digest_blake3,
                    request.accepted_at_ms,
                    request.expires_at_ms
                ],
            )?;

            let consumed = transaction.execute(
                "UPDATE confirmation_tokens
                 SET state = 'consumed', consumed_at_ms = ?8, consumed_by_request_id = ?9
                 WHERE id = ?1
                   AND token_digest_blake3 = ?2
                   AND action = 'restore_overwrite'
                   AND client_sid_hash = ?3
                   AND resource_type = 'restore_target'
                   AND resource_id = ?4
                   AND resource_fingerprint = ?5
                   AND payload_digest_blake3 = ?6
                   AND state = 'issued'
                   AND issued_at_ms <= ?7
                   AND expires_at_ms > ?7",
                params![
                    confirmation_token_id.as_str(),
                    confirmation_token_digest,
                    request.client_sid_hash,
                    restore.target_database_name,
                    restore.target_fingerprint,
                    request.payload_digest_blake3,
                    request.accepted_at_ms,
                    request.accepted_at_ms,
                    request.request_id.as_str()
                ],
            )?;
            if consumed != 1 {
                return Err(RepositoryError::ConfirmationInvalidOrReplayed);
            }

            transaction.execute(
                "INSERT INTO restore_runs(
                     id, recovery_point_id, source_replica_id,
                     target_connection_profile_id,
                     target_database_name, target_fingerprint, purpose, conflict_policy,
                     eligibility_snapshot_json, confirmation_token_id, state,
                     created_at_ms, updated_at_ms
                 ) VALUES (
                     ?1, ?2, ?3, ?4,
                     ?5, ?6, 'incident_recovery', 'overwrite',
                     ?7, ?8, 'queued',
                     ?9, ?9
                 )",
                params![
                    restore.restore_run_id.as_str(),
                    restore.recovery_point_id.as_str(),
                    restore.source_replica_id,
                    restore.target_connection_profile_id.as_str(),
                    restore.target_database_name,
                    restore.target_fingerprint,
                    restore.eligibility_snapshot_json,
                    confirmation_token_id.as_str(),
                    restore.created_at_ms
                ],
            )?;
            transaction.execute(
                "INSERT INTO work_items(
                     id, kind, restore_run_id, priority, state, available_at_ms,
                     attempt_limit, created_at_ms, updated_at_ms
                 ) VALUES (?1, 'restore', ?2, 500, 'queued', ?3, 3, ?3, ?3)",
                params![
                    restore.work_item_id.as_str(),
                    restore.restore_run_id.as_str(),
                    restore.created_at_ms
                ],
            )?;
            let completed = transaction.execute(
                "UPDATE ipc_request_ledger
                 SET state = 'succeeded', result_ref_type = ?2,
                     result_ref_id = ?3, response_json = ?4, completed_at_ms = ?5
                 WHERE request_id = ?1
                   AND state = 'accepted'
                   AND payload_digest_blake3 = ?6",
                params![
                    request.request_id.as_str(),
                    completion.result_ref_type,
                    completion.result_ref_id,
                    completion.response_json,
                    completion.completed_at_ms,
                    request.payload_digest_blake3
                ],
            )?;
            if completed != 1 {
                return Err(RepositoryError::IpcRequestIncomplete);
            }
            transaction.commit()?;
            Ok(IpcAtomicMutationResult {
                outcome: stored,
                created: true,
            })
        })
    }

    pub fn stage_retention_tombstone(
        &self,
        spec: RetentionTombstoneSpec,
    ) -> Result<RetentionTombstoneResult, RepositoryError> {
        spec.validate()?;
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let created = stage_retention_tombstone_in_transaction(&transaction, &spec)?;
            transaction.commit()?;
            Ok(RetentionTombstoneResult {
                operation_run_id: spec.operation_run_id,
                created,
            })
        })
    }

    pub fn enqueue_retention_delete(
        &self,
        request: IpcRequestSpec,
        confirmation_token_id: &ConfirmationTokenId,
        confirmation_token_digest: &str,
        retention: RetentionDeleteSpec,
    ) -> Result<RetentionDeleteEnqueueResult, RepositoryError> {
        let workspace_id = WorkspaceId::from_str(DEFAULT_WORKSPACE_ID)
            .expect("the built-in workspace ID is valid");
        self.enqueue_retention_delete_in_workspace(
            &workspace_id,
            request,
            confirmation_token_id,
            confirmation_token_digest,
            retention,
        )
    }

    pub fn enqueue_retention_delete_in_workspace(
        &self,
        workspace_id: &WorkspaceId,
        request: IpcRequestSpec,
        confirmation_token_id: &ConfirmationTokenId,
        confirmation_token_digest: &str,
        retention: RetentionDeleteSpec,
    ) -> Result<RetentionDeleteEnqueueResult, RepositoryError> {
        request.validate()?;
        retention.validate()?;
        let confirmation_authorization = retention.authorization;
        let recovery_point_id =
            RecoveryPointId::from_str(&confirmation_authorization.recovery_point_id().to_string())
                .map_err(RepositoryError::from)?;
        let destination_fingerprint = confirmation_authorization
            .destination_fingerprint()
            .to_owned();
        let confirmation_digest_blake3 = confirmation_authorization.digest_blake3().to_owned();
        let grace_period_ms = confirmation_authorization.grace_period_ms();
        let tombstoned_at_ms = confirmation_authorization
            .tombstoned_at_ms()
            .ok_or(RepositoryError::SecurityInputInvalid)?;
        let confirmation_authorization_json = confirmation_authorization
            .to_json()
            .map_err(|_| RepositoryError::SecurityInputInvalid)?;
        let confirmation_authorization_value: serde_json::Value =
            serde_json::from_str(&confirmation_authorization_json)
                .map_err(|_| RepositoryError::SecurityInputInvalid)?;
        if request.command_name != "retention.delete"
            || !is_digest(confirmation_token_digest)
            || tombstoned_at_ms > request.accepted_at_ms
            || confirmation_authorization.evaluated_at_ms() > request.accepted_at_ms
            || !valid_delete_secret_binding(
                &destination_fingerprint,
                confirmation_authorization.delete_secret_ref_id(),
            )
        {
            return Err(RepositoryError::SecurityInputInvalid);
        }
        let confirmation_token_id = confirmation_token_id.clone();
        let confirmation_token_digest = confirmation_token_digest.to_owned();
        let operation_run_id = retention.operation_run_id;
        let work_item_id = retention.work_item_id;
        let workspace_id = workspace_id.clone();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let existing: Option<ExistingLedgerRow> = transaction
                .query_row(
                    "SELECT payload_digest_blake3, state, response_json,
                            result_ref_type, result_ref_id
                     FROM ipc_request_ledger
                     WHERE client_sid_hash = ?1
                       AND command_name = ?2
                       AND idempotency_key = ?3",
                    params![
                        request.client_sid_hash,
                        request.command_name,
                        request.idempotency_key
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
            if let Some((payload_digest, state, response_json, result_ref_type, result_ref_id)) =
                existing
            {
                if payload_digest != request.payload_digest_blake3 {
                    return Err(RepositoryError::IpcIdempotencyConflict);
                }
                if state != "succeeded" || result_ref_type.as_deref() != Some("work_item") {
                    return Err(RepositoryError::IpcRequestIncomplete);
                }
                let work_item_id = result_ref_id
                    .ok_or(RepositoryError::IpcRequestIncomplete)
                    .and_then(|raw| WorkItemId::from_str(&raw).map_err(RepositoryError::from))?;
                let response: serde_json::Value = serde_json::from_str(
                    response_json
                        .as_deref()
                        .ok_or(RepositoryError::IpcRequestIncomplete)?,
                )
                .map_err(|_| RepositoryError::IpcRequestIncomplete)?;
                let stored_operation_run_id: Option<String> = transaction
                    .query_row(
                        "SELECT operation_run_id
                         FROM work_items
                         WHERE id = ?1 AND kind = 'operation'",
                        [work_item_id.as_str()],
                        |row| row.get(0),
                    )
                    .optional()?;
                if response != serde_json::json!({ "workItemId": work_item_id.as_str() })
                    || stored_operation_run_id.as_deref() != Some(operation_run_id.as_str())
                {
                    return Err(RepositoryError::IpcRequestIncomplete);
                }
                transaction.commit()?;
                return Ok(RetentionDeleteEnqueueResult {
                    work_item_id,
                    created: false,
                });
            }

            ensure_workspace_recovery_point_in_transaction(
                &transaction,
                &workspace_id,
                &recovery_point_id,
            )?;
            let confirmation_is_issued: bool = transaction.query_row(
                "SELECT EXISTS(
                     SELECT 1
                     FROM confirmation_tokens
                     WHERE id = ?1
                       AND token_digest_blake3 = ?2
                       AND action = 'retention_delete'
                       AND client_sid_hash = ?3
                       AND resource_type = 'artifact_replica'
                       AND resource_id = ?4
                       AND resource_fingerprint = ?5
                       AND payload_digest_blake3 = ?6
                       AND state = 'issued'
                       AND issued_at_ms <= ?7
                       AND expires_at_ms > ?7
                 )",
                params![
                    confirmation_token_id.as_str(),
                    confirmation_token_digest,
                    request.client_sid_hash,
                    recovery_point_id.as_str(),
                    destination_fingerprint,
                    confirmation_digest_blake3,
                    request.accepted_at_ms
                ],
                |row| row.get(0),
            )?;
            if !confirmation_is_issued {
                return Err(RepositoryError::ConfirmationInvalidOrReplayed);
            }

            let staged: Option<(String, String, String, String, String, i64)> = transaction
                .query_row(
                    "SELECT operation_kind, subject_type, subject_id, request_json,
                            state, created_at_ms
                     FROM operation_runs
                     WHERE id = ?1",
                    [operation_run_id.as_str()],
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
                staged_kind,
                staged_subject_type,
                staged_subject_id,
                staged_request_json,
                staged_state,
                staged_created_at_ms,
            )) = staged
            else {
                return Err(RepositoryError::RetentionTombstoneInvalid);
            };
            let staged_tombstone =
                serde_json::from_str::<RetentionTombstoneDocumentV1>(&staged_request_json)
                    .map_err(|_| RepositoryError::RetentionTombstoneInvalid)?;
            let current_plan = prepare_delete_plan_in_transaction(
                &transaction,
                &recovery_point_id,
                &destination_fingerprint,
                request.accepted_at_ms,
                grace_period_ms,
            )
            .map_err(|error| match error {
                RepositoryError::RetentionAuthorityInvalid => {
                    RepositoryError::RetentionTombstoneInvalid
                }
                error => error,
            })?;
            if current_plan.decision().disposition != RetentionDispositionV2::DeleteEligible
                || !confirmation_authorization
                    .has_same_delete_scope(current_plan.authorization())
                    .map_err(|_| RepositoryError::RetentionTombstoneInvalid)?
            {
                return Err(RepositoryError::RetentionTombstoneInvalid);
            }
            let current_authorization = current_plan.authorization().clone();
            let current_authorization_json = current_authorization
                .to_json()
                .map_err(|_| RepositoryError::RetentionAuthorityInvalid)?;
            let current_authorization_value: serde_json::Value =
                serde_json::from_str(&current_authorization_json)
                    .map_err(|_| RepositoryError::RetentionAuthorityInvalid)?;
            let current_authorization_plan = current_authorization_value
                .get("plan")
                .and_then(serde_json::Value::as_object)
                .ok_or(RepositoryError::RetentionAuthorityInvalid)?;
            let authorized_objects = serde_json::from_value::<Vec<RetentionTombstoneObjectV1>>(
                current_authorization_plan
                    .get("objects")
                    .cloned()
                    .ok_or(RepositoryError::RetentionAuthorityInvalid)?,
            )
            .map_err(|_| RepositoryError::RetentionAuthorityInvalid)?;
            let delete_secret_ref_id = current_authorization
                .delete_secret_ref_id()
                .map(str::to_owned);
            let dry_run_digest_blake3 = current_authorization.digest_blake3().to_owned();
            let current_objects = load_retention_tombstone_objects(
                &transaction,
                recovery_point_id.as_str(),
                &destination_fingerprint,
            )?;
            let expected_tombstone = RetentionTombstoneDocumentV1 {
                destination_fingerprint: destination_fingerprint.clone(),
                grace_period_ms,
                objects: authorized_objects,
                phase: "tombstoned".to_owned(),
                recovery_point_id: recovery_point_id.as_str().to_owned(),
                schema_version: 1,
                tombstoned_at_ms,
            };
            let eligible_after_ms = staged_tombstone
                .tombstoned_at_ms
                .checked_add(staged_tombstone.grace_period_ms)
                .ok_or(RepositoryError::RetentionTombstoneInvalid)?;
            let work_count: i64 = transaction.query_row(
                "SELECT COUNT(*) FROM work_items WHERE operation_run_id = ?1",
                [operation_run_id.as_str()],
                |row| row.get(0),
            )?;
            if staged_kind != "retention_delete"
                || staged_subject_type != "artifact_replica"
                || staged_subject_id != recovery_point_id.as_str()
                || staged_state != "created"
                || staged_created_at_ms != tombstoned_at_ms
                || work_count != 0
                || staged_tombstone != expected_tombstone
                || current_objects != staged_tombstone.objects
                || current_authorization.evaluated_at_ms() != request.accepted_at_ms
                || request.accepted_at_ms < eligible_after_ms
            {
                return Err(RepositoryError::RetentionTombstoneInvalid);
            }

            transaction.execute(
                "INSERT INTO ipc_request_ledger(
                     request_id, command_name, client_sid_hash, idempotency_key,
                     payload_digest_blake3, state, accepted_at_ms, expires_at_ms, created_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'accepted', ?6, ?7, ?6)",
                params![
                    request.request_id.as_str(),
                    request.command_name,
                    request.client_sid_hash,
                    request.idempotency_key,
                    request.payload_digest_blake3,
                    request.accepted_at_ms,
                    request.expires_at_ms
                ],
            )?;
            let consumed = transaction.execute(
                "UPDATE confirmation_tokens
                 SET state = 'consumed', consumed_at_ms = ?7, consumed_by_request_id = ?8
                 WHERE id = ?1
                   AND token_digest_blake3 = ?2
                   AND action = 'retention_delete'
                   AND client_sid_hash = ?3
                   AND resource_type = 'artifact_replica'
                   AND resource_id = ?4
                   AND resource_fingerprint = ?5
                   AND payload_digest_blake3 = ?6
                   AND state = 'issued'
                   AND issued_at_ms <= ?7
                   AND expires_at_ms > ?7",
                params![
                    confirmation_token_id.as_str(),
                    confirmation_token_digest,
                    request.client_sid_hash,
                    recovery_point_id.as_str(),
                    destination_fingerprint,
                    confirmation_digest_blake3,
                    request.accepted_at_ms,
                    request.request_id.as_str()
                ],
            )?;
            if consumed != 1 {
                return Err(RepositoryError::ConfirmationInvalidOrReplayed);
            }

            let operation_request_json = serde_json::to_string(&serde_json::json!({
                "authorization": current_authorization_value,
                "authorized_at_ms": request.accepted_at_ms,
                "confirmation_authorization": confirmation_authorization_value,
                "confirmation_digest_blake3": confirmation_digest_blake3,
                "confirmation_token_id": confirmation_token_id.as_str(),
                "delete_secret_ref_id": delete_secret_ref_id,
                "destination_fingerprint": destination_fingerprint,
                "dry_run_digest_blake3": dry_run_digest_blake3,
                "recovery_point_id": recovery_point_id.as_str(),
                "request_payload_digest_blake3": request.payload_digest_blake3,
                "schema_version": 2,
                "tombstone": staged_tombstone,
                "tombstoned_at_ms": tombstoned_at_ms,
            }))
            .map_err(|_| RepositoryError::SecurityInputInvalid)?;
            let operation_updated = transaction.execute(
                "UPDATE operation_runs
                 SET request_json = ?2, state = 'queued', updated_at_ms = ?3
                 WHERE id = ?1
                   AND state = 'created'
                   AND request_json = ?4",
                params![
                    operation_run_id.as_str(),
                    operation_request_json,
                    request.accepted_at_ms,
                    staged_request_json
                ],
            )?;
            if operation_updated != 1 {
                return Err(RepositoryError::RetentionTombstoneInvalid);
            }
            transaction.execute(
                "INSERT INTO work_items(
                     id, kind, operation_run_id, priority, state,
                     available_at_ms, attempt_limit, created_at_ms, updated_at_ms
                 ) VALUES (?1, 'operation', ?2, 500, 'queued', ?3, 3, ?3, ?3)",
                params![
                    work_item_id.as_str(),
                    operation_run_id.as_str(),
                    request.accepted_at_ms
                ],
            )?;
            let response_json =
                serde_json::json!({ "workItemId": work_item_id.as_str() }).to_string();
            let completed = transaction.execute(
                "UPDATE ipc_request_ledger
                 SET state = 'succeeded', result_ref_type = 'work_item',
                     result_ref_id = ?2, response_json = ?3, completed_at_ms = ?4
                 WHERE request_id = ?1
                   AND state = 'accepted'
                   AND payload_digest_blake3 = ?5",
                params![
                    request.request_id.as_str(),
                    work_item_id.as_str(),
                    response_json,
                    request.accepted_at_ms,
                    request.payload_digest_blake3
                ],
            )?;
            if completed != 1 {
                return Err(RepositoryError::IpcRequestIncomplete);
            }
            transaction.commit()?;
            Ok(RetentionDeleteEnqueueResult {
                work_item_id,
                created: true,
            })
        })
    }

    pub fn request_count(&self) -> Result<i64, RepositoryError> {
        let reader = self.store.open_reader()?;
        reader
            .connection()
            .query_row("SELECT COUNT(*) FROM ipc_request_ledger", [], |row| {
                row.get(0)
            })
            .map_err(RepositoryError::from)
    }

    pub fn restore_count(&self) -> Result<i64, RepositoryError> {
        let reader = self.store.open_reader()?;
        reader
            .connection()
            .query_row("SELECT COUNT(*) FROM restore_runs", [], |row| row.get(0))
            .map_err(RepositoryError::from)
    }

    pub fn confirmation_state(
        &self,
        confirmation_token_id: &ConfirmationTokenId,
    ) -> Result<String, RepositoryError> {
        let reader = self.store.open_reader()?;
        reader
            .connection()
            .query_row(
                "SELECT state FROM confirmation_tokens WHERE id = ?1",
                [confirmation_token_id.as_str()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(RepositoryError::NotFound)
    }

    fn write<F, T>(&self, job: F) -> Result<T, RepositoryError>
    where
        F: FnOnce(&mut rusqlite::Connection) -> Result<T, RepositoryError> + Send + 'static,
        T: Send + 'static,
    {
        self.store.run_write(job).map_err(RepositoryError::from)?
    }
}

fn insert_confirmation_in_transaction(
    transaction: &Transaction<'_>,
    spec: &ConfirmationSpec,
) -> Result<(), RepositoryError> {
    transaction.execute(
        "INSERT INTO confirmation_tokens(
             id, token_digest_blake3, action, client_sid_hash,
             resource_type, resource_id, resource_fingerprint,
             payload_digest_blake3, state, issued_at_ms, expires_at_ms, created_at_ms
         ) VALUES (
             ?1, ?2, ?3, ?4,
             ?5, ?6, ?7,
             ?8, 'issued', ?9, ?10, ?9
         )",
        params![
            spec.id.as_str(),
            spec.token_digest_blake3,
            spec.action,
            spec.client_sid_hash,
            spec.resource_type,
            spec.resource_id,
            spec.resource_fingerprint,
            spec.payload_digest_blake3,
            spec.issued_at_ms,
            spec.expires_at_ms
        ],
    )?;
    Ok(())
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RetentionConfirmationStoredResponseV1 {
    schema_version: u32,
    confirmation_token_id: String,
    action: String,
    issued_at_ms: i64,
    expires_at_ms: i64,
    token_available: bool,
    payload_digest_blake3: String,
    authorization: serde_json::Value,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RestoreOverwriteConfirmationStoredResponseV1 {
    schema_version: u32,
    confirmation_token_id: String,
    action: String,
    recovery_point_id: String,
    source_replica_id: String,
    target_connection_profile_id: String,
    target_database_name: String,
    target_fingerprint: String,
    preflight_version: String,
    required_verification_level: i64,
    issued_at_ms: i64,
    expires_at_ms: i64,
    token_available: bool,
    payload_digest_blake3: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReplicaRepairStoredResponseV1 {
    schema_version: u32,
    operation_run_id: String,
    work_item_id: String,
    state: String,
    activity_after_seq: i64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ManualVerificationStoredResponseV1 {
    schema_version: u32,
    verification_run_id: String,
    work_item_id: Option<String>,
    state: String,
    requested_level: i64,
    source_replica_id: String,
    support_status: Option<String>,
    limitation: Option<String>,
    error_code: Option<String>,
    activity_after_seq: i64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ManualBackupStoredResponseV1 {
    schema_version: u32,
    plan_id: String,
    plan_revision_id: String,
    plan_occurrence_id: String,
    plan_name: String,
    state: String,
    work_items: Vec<ManualBackupStoredWorkItemV1>,
    activity_after_seq: i64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ManualBackupStoredWorkItemV1 {
    backup_run_id: String,
    work_item_id: String,
    target_id: String,
    target_name: String,
    #[serde(default)]
    scope_id: Option<String>,
    #[serde(default)]
    scope_name: Option<String>,
    #[serde(default)]
    scope_mode: Option<String>,
    engine: String,
    state: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ConfigCutoverConfirmationStoredResponseV1 {
    schema_version: u32,
    confirmation_token_id: String,
    action: String,
    cutover_run_id: String,
    shadow_import_id: String,
    cutover_marker: String,
    issued_at_ms: i64,
    expires_at_ms: i64,
    token_available: bool,
    payload_digest_blake3: String,
}

pub(super) fn stage_retention_tombstone_in_transaction(
    transaction: &Transaction<'_>,
    spec: &RetentionTombstoneSpec,
) -> Result<bool, RepositoryError> {
    let objects = load_retention_tombstone_objects(
        transaction,
        spec.recovery_point_id.as_str(),
        &spec.destination_fingerprint,
    )?;
    let tombstone = RetentionTombstoneDocumentV1 {
        destination_fingerprint: spec.destination_fingerprint.clone(),
        grace_period_ms: spec.grace_period_ms,
        objects,
        phase: "tombstoned".to_owned(),
        recovery_point_id: spec.recovery_point_id.as_str().to_owned(),
        schema_version: 1,
        tombstoned_at_ms: spec.tombstoned_at_ms,
    };
    let request_json = serde_json::to_string(&tombstone)
        .map_err(|_| RepositoryError::RetentionTombstoneInvalid)?;
    let existing = transaction
        .query_row(
            "SELECT operation_kind, subject_type, subject_id, request_json, state
             FROM operation_runs
             WHERE id = ?1",
            [spec.operation_run_id.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .optional()?;
    if let Some((kind, subject_type, subject_id, existing_json, state)) = existing {
        let existing_tombstone =
            serde_json::from_str::<RetentionTombstoneDocumentV1>(&existing_json)
                .map_err(|_| RepositoryError::RetentionTombstoneInvalid)?;
        let work_count: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM work_items WHERE operation_run_id = ?1",
            [spec.operation_run_id.as_str()],
            |row| row.get(0),
        )?;
        if kind != "retention_delete"
            || subject_type != "artifact_replica"
            || subject_id != spec.recovery_point_id.as_str()
            || state != "created"
            || work_count != 0
            || existing_tombstone != tombstone
        {
            return Err(RepositoryError::RetentionTombstoneInvalid);
        }
        return Ok(false);
    }

    transaction.execute(
        "INSERT INTO operation_runs(
             id, operation_kind, subject_type, subject_id, request_json,
             state, created_at_ms, updated_at_ms
         ) VALUES (?1, 'retention_delete', 'artifact_replica', ?2, ?3,
                   'created', ?4, ?4)",
        params![
            spec.operation_run_id.as_str(),
            spec.recovery_point_id.as_str(),
            request_json,
            spec.tombstoned_at_ms
        ],
    )?;
    Ok(true)
}

fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn supported_backup_engine(value: &str) -> bool {
    matches!(value, "postgresql" | "mysql" | "mariadb" | "sqlite")
}

fn invalid_text(value: &str) -> bool {
    value.contains(['\0', '\r', '\n'])
}

fn is_cutover_marker(value: &str) -> bool {
    value
        .strip_prefix("config-cutover-v1:")
        .is_some_and(is_digest)
}

fn load_retention_tombstone_objects(
    transaction: &Transaction<'_>,
    recovery_point_id: &str,
    destination_fingerprint: &str,
) -> Result<Vec<RetentionTombstoneObjectV1>, RepositoryError> {
    let replica_id = transaction
        .query_row(
            "SELECT replicas.id
             FROM catalog_artifact_replicas AS replicas
             JOIN catalog_recovery_points AS recovery
               ON recovery.id = replicas.recovery_point_id
             WHERE replicas.recovery_point_id = ?1
               AND replicas.destination_fingerprint = ?2
               AND replicas.state IN ('committed', 'degraded')
               AND recovery.state = 'complete'
               AND recovery.signature_status = 'valid'",
            params![recovery_point_id, destination_fingerprint],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or(RepositoryError::RetentionTombstoneInvalid)?;
    let mut statement = transaction.prepare(
        "SELECT object_key, object_version, object_role
         FROM catalog_stored_objects
         WHERE recovery_point_id = ?1
           AND artifact_replica_id = ?2
           AND state = 'present'
         ORDER BY CASE object_role
                      WHEN 'set_manifest' THEN 0
                      WHEN 'part_manifest' THEN 1
                      WHEN 'verification_evidence' THEN 2
                      WHEN 'payload' THEN 3
                      ELSE 4
                  END,
                  object_key,
                  object_version",
    )?;
    let rows = statement.query_map(params![recovery_point_id, replica_id], |row| {
        Ok(RetentionTombstoneObjectV1 {
            key: row.get(0)?,
            version: row.get(1)?,
            role: row.get(2)?,
        })
    })?;
    let objects = rows.collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    if !retention_tombstone_objects_valid(&objects) {
        return Err(RepositoryError::RetentionTombstoneInvalid);
    }
    Ok(objects)
}

fn retention_tombstone_objects_valid(objects: &[RetentionTombstoneObjectV1]) -> bool {
    if objects.is_empty()
        || objects
            .iter()
            .filter(|object| object.role == "set_manifest")
            .count()
            != 1
    {
        return false;
    }
    let mut identities = std::collections::HashSet::with_capacity(objects.len());
    objects.iter().all(|object| {
        matches!(
            object.role.as_str(),
            "payload" | "part_manifest" | "set_manifest" | "verification_evidence"
        ) && !object.key.is_empty()
            && object.key.len() <= 2_048
            && !object.key.starts_with('/')
            && !object.key.contains('\\')
            && !object
                .key
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
            && object.version.as_deref().is_none_or(|version| {
                !version.is_empty()
                    && version.len() <= 1_024
                    && !version
                        .chars()
                        .any(|value| matches!(value, '\0' | '\r' | '\n'))
            })
            && identities.insert((object.key.as_str(), object.version.as_deref()))
    })
}

fn valid_delete_secret_binding(
    destination_fingerprint: &str,
    delete_secret_ref_id: Option<&str>,
) -> bool {
    let Some((kind, _)) = destination_fingerprint.split_once(':') else {
        return false;
    };
    match kind {
        "local" => delete_secret_ref_id.is_none(),
        "s3" | "ssh_sftp" => delete_secret_ref_id.is_some_and(|value| {
            uuid::Uuid::parse_str(value)
                .map(|parsed| parsed.to_string() == value)
                .unwrap_or(false)
        }),
        _ => false,
    }
}

fn ensure_workspace_recovery_point_in_transaction(
    transaction: &Transaction<'_>,
    workspace_id: &WorkspaceId,
    recovery_point_id: &RecoveryPointId,
) -> Result<(), RepositoryError> {
    let owned: bool = transaction.query_row(
        "SELECT EXISTS(
             SELECT 1
             FROM workspace_recovery_points AS ownership
             JOIN workspaces AS workspace ON workspace.id = ownership.workspace_id
             WHERE ownership.workspace_id = ?1
               AND ownership.recovery_point_id = ?2
               AND workspace.state = 'active'
         )",
        params![workspace_id.as_str(), recovery_point_id.as_str()],
        |row| row.get(0),
    )?;
    if owned {
        Ok(())
    } else {
        Err(RepositoryError::WorkspaceScopeMismatch)
    }
}

fn is_confirmation_action(value: &str) -> bool {
    matches!(
        value,
        "restore_overwrite"
            | "retention_delete"
            | "connection_archive"
            | "destination_archive"
            | "secret_rotate"
            | "config_cutover"
            | "control_store_restore"
            | "acl_change"
    )
}
