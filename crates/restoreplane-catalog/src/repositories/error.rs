use restoreplane_domain::DomainError;
use thiserror::Error;

use crate::OperationalStoreError;

#[derive(Debug, Error)]
pub enum RepositoryError {
    #[error("WB_BINDING_UNPROVEN")]
    WorkbenchBindingUnproven,
    #[error("{0}")]
    Domain(#[from] DomainError),
    #[error("OPERATIONAL_REPOSITORY_NOT_FOUND")]
    NotFound,
    #[error("WORKSPACE_INPUT_INVALID")]
    WorkspaceInputInvalid,
    #[error("WORKSPACE_NAME_CONFLICT")]
    WorkspaceNameConflict,
    #[error("WORKSPACE_SCOPE_MISMATCH")]
    WorkspaceScopeMismatch,
    #[error("OCCURRENCE_IDENTITY_CONFLICT")]
    OccurrenceIdentityConflict,
    #[error("WORK_ITEM_FENCE_STALE")]
    StaleFence,
    #[error("WORK_ITEM_LEASE_INVALID")]
    LeaseInvalid,
    #[error("OPERATIONAL_RUNTIME_STATE_INVALID")]
    RuntimeStateInvalid,
    #[error("BACKUP_SCOPE_INPUT_INVALID")]
    BackupScopeInputInvalid,
    #[error("BACKUP_SCOPE_DISCOVERY_INVALID")]
    BackupScopeDiscoveryInvalid,
    #[error("BACKUP_SCOPE_OBJECT_MISSING")]
    BackupScopeObjectMissing,
    #[error("BACKUP_SCOPE_CONFLICT")]
    BackupScopeConflict,
    #[error("IPC_IDEMPOTENCY_CONFLICT")]
    IpcIdempotencyConflict,
    #[error("IPC_REQUEST_INCOMPLETE")]
    IpcRequestIncomplete,
    #[error("CONFIRMATION_TOKEN_INVALID_OR_REPLAYED")]
    ConfirmationInvalidOrReplayed,
    #[error("OPERATIONAL_SECURITY_INPUT_INVALID")]
    SecurityInputInvalid,
    #[error("RETENTION_TOMBSTONE_INVALID")]
    RetentionTombstoneInvalid,
    #[error("RETENTION_AUTHORITY_INVALID")]
    RetentionAuthorityInvalid,
    #[error("RECOVERY_POINT_INCOMPLETE")]
    RecoveryPointIncomplete,
    #[error("RESTORE_PREFLIGHT_STALE")]
    RestorePreflightStale,
    #[error("CATALOG_PROJECTION_INPUT_INVALID")]
    CatalogProjectionInputInvalid,
    #[error("NOTIFICATION_DEDUPLICATION_CONFLICT")]
    NotificationDeduplicationConflict,
    #[error("NOTIFICATION_FENCE_STALE")]
    NotificationFenceStale,
    #[error("NOTIFICATION_INPUT_INVALID")]
    NotificationInputInvalid,
    #[error("CERTIFICATION_INPUT_INVALID")]
    CertificationInputInvalid,
    #[error("VERIFICATION_INPUT_INVALID")]
    VerificationInputInvalid,
    #[error("VERIFICATION_IDENTITY_CONFLICT")]
    VerificationIdentityConflict,
    #[error("VERIFICATION_RESTORE_RUN_REQUIRED")]
    VerificationRestoreRunRequired,
    #[error("CONTROL_STORE_BACKUP_INPUT_INVALID")]
    ControlStoreBackupInputInvalid,
    #[error("CONTROL_STORE_BACKUP_IO")]
    ControlStoreBackupIo,
    #[error("CONTROL_STORE_BACKUP_INVALID")]
    ControlStoreBackupInvalid,
    #[error("DIAGNOSTICS_INPUT_INVALID")]
    DiagnosticsInputInvalid,
    #[error("TELEMETRY_ARCHIVE_INPUT_INVALID")]
    TelemetryArchiveInputInvalid,
    #[error("TELEMETRY_ARCHIVE_CONFLICT")]
    TelemetryArchiveConflict,
    #[error("SHADOW_IMPORT_INPUT_INVALID")]
    ShadowImportInputInvalid,
    #[error("SHADOW_IMPORT_CONFLICT")]
    ShadowImportConflict,
    #[error("CONFIG_CUTOVER_INPUT_INVALID")]
    CutoverInputInvalid,
    #[error("CONFIG_CUTOVER_PRECONDITION_FAILED")]
    CutoverPreconditionFailed,
    #[error("CONFIG_CUTOVER_ALREADY_COMMITTED")]
    CutoverAlreadyCommitted,
    #[error("CONFIG_CUTOVER_STATE_INVALID")]
    CutoverStateInvalid,
    #[error("STANDALONE_AUTHORITY_BOOTSTRAP_INPUT_INVALID")]
    StandaloneBootstrapInputInvalid,
    #[error("STANDALONE_AUTHORITY_BOOTSTRAP_PRECONDITION_FAILED")]
    StandaloneBootstrapPreconditionFailed,
    #[error("CONFIG_CUTOVER_ROLLBACK_INPUT_INVALID")]
    CutoverRollbackInputInvalid,
    #[error("CONFIG_CUTOVER_ROLLBACK_PRECONDITION_FAILED")]
    CutoverRollbackPreconditionFailed,
    #[error("CONFIG_CUTOVER_ROLLBACK_CONFLICT")]
    CutoverRollbackConflict,
    #[error("CONFIG_CUTOVER_FORWARD_FIX_REQUIRED")]
    CutoverForwardFixRequired,
    #[error("MINIMUM_READER_INPUT_INVALID")]
    MinimumReaderInputInvalid,
    #[error("MINIMUM_READER_VERSION_REGRESSION")]
    MinimumReaderRegression,
    #[error("LEGACY_DIRECT_EXECUTION_BLOCKED")]
    LegacyDirectExecutionBlocked,
    #[error("OPERATIONAL_REPOSITORY_SQLITE")]
    Sqlite(#[source] rusqlite::Error),
    #[error("OPERATIONAL_REPOSITORY_STORE")]
    Store(#[source] OperationalStoreError),
}

impl RepositoryError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::WorkbenchBindingUnproven => "WB_BINDING_UNPROVEN",
            Self::Domain(error) => error.code(),
            Self::NotFound => "OPERATIONAL_REPOSITORY_NOT_FOUND",
            Self::WorkspaceInputInvalid => "WORKSPACE_INPUT_INVALID",
            Self::WorkspaceNameConflict => "WORKSPACE_NAME_CONFLICT",
            Self::WorkspaceScopeMismatch => "WORKSPACE_SCOPE_MISMATCH",
            Self::OccurrenceIdentityConflict => "OCCURRENCE_IDENTITY_CONFLICT",
            Self::StaleFence => "WORK_ITEM_FENCE_STALE",
            Self::LeaseInvalid => "WORK_ITEM_LEASE_INVALID",
            Self::RuntimeStateInvalid => "OPERATIONAL_RUNTIME_STATE_INVALID",
            Self::BackupScopeInputInvalid => "BACKUP_SCOPE_INPUT_INVALID",
            Self::BackupScopeDiscoveryInvalid => "BACKUP_SCOPE_DISCOVERY_INVALID",
            Self::BackupScopeObjectMissing => "BACKUP_SCOPE_OBJECT_MISSING",
            Self::BackupScopeConflict => "BACKUP_SCOPE_CONFLICT",
            Self::IpcIdempotencyConflict => "IPC_IDEMPOTENCY_CONFLICT",
            Self::IpcRequestIncomplete => "IPC_REQUEST_INCOMPLETE",
            Self::ConfirmationInvalidOrReplayed => "CONFIRMATION_TOKEN_INVALID_OR_REPLAYED",
            Self::SecurityInputInvalid => "OPERATIONAL_SECURITY_INPUT_INVALID",
            Self::RetentionTombstoneInvalid => "RETENTION_TOMBSTONE_INVALID",
            Self::RetentionAuthorityInvalid => "RETENTION_AUTHORITY_INVALID",
            Self::RecoveryPointIncomplete => "RECOVERY_POINT_INCOMPLETE",
            Self::RestorePreflightStale => "RESTORE_PREFLIGHT_STALE",
            Self::CatalogProjectionInputInvalid => "CATALOG_PROJECTION_INPUT_INVALID",
            Self::NotificationDeduplicationConflict => "NOTIFICATION_DEDUPLICATION_CONFLICT",
            Self::NotificationFenceStale => "NOTIFICATION_FENCE_STALE",
            Self::NotificationInputInvalid => "NOTIFICATION_INPUT_INVALID",
            Self::CertificationInputInvalid => "CERTIFICATION_INPUT_INVALID",
            Self::VerificationInputInvalid => "VERIFICATION_INPUT_INVALID",
            Self::VerificationIdentityConflict => "VERIFICATION_IDENTITY_CONFLICT",
            Self::VerificationRestoreRunRequired => "VERIFICATION_RESTORE_RUN_REQUIRED",
            Self::ControlStoreBackupInputInvalid => "CONTROL_STORE_BACKUP_INPUT_INVALID",
            Self::ControlStoreBackupIo => "CONTROL_STORE_BACKUP_IO",
            Self::ControlStoreBackupInvalid => "CONTROL_STORE_BACKUP_INVALID",
            Self::DiagnosticsInputInvalid => "DIAGNOSTICS_INPUT_INVALID",
            Self::TelemetryArchiveInputInvalid => "TELEMETRY_ARCHIVE_INPUT_INVALID",
            Self::TelemetryArchiveConflict => "TELEMETRY_ARCHIVE_CONFLICT",
            Self::ShadowImportInputInvalid => "SHADOW_IMPORT_INPUT_INVALID",
            Self::ShadowImportConflict => "SHADOW_IMPORT_CONFLICT",
            Self::CutoverInputInvalid => "CONFIG_CUTOVER_INPUT_INVALID",
            Self::CutoverPreconditionFailed => "CONFIG_CUTOVER_PRECONDITION_FAILED",
            Self::CutoverAlreadyCommitted => "CONFIG_CUTOVER_ALREADY_COMMITTED",
            Self::CutoverStateInvalid => "CONFIG_CUTOVER_STATE_INVALID",
            Self::StandaloneBootstrapInputInvalid => "STANDALONE_AUTHORITY_BOOTSTRAP_INPUT_INVALID",
            Self::StandaloneBootstrapPreconditionFailed => {
                "STANDALONE_AUTHORITY_BOOTSTRAP_PRECONDITION_FAILED"
            }
            Self::CutoverRollbackInputInvalid => "CONFIG_CUTOVER_ROLLBACK_INPUT_INVALID",
            Self::CutoverRollbackPreconditionFailed => {
                "CONFIG_CUTOVER_ROLLBACK_PRECONDITION_FAILED"
            }
            Self::CutoverRollbackConflict => "CONFIG_CUTOVER_ROLLBACK_CONFLICT",
            Self::CutoverForwardFixRequired => "CONFIG_CUTOVER_FORWARD_FIX_REQUIRED",
            Self::MinimumReaderInputInvalid => "MINIMUM_READER_INPUT_INVALID",
            Self::MinimumReaderRegression => "MINIMUM_READER_VERSION_REGRESSION",
            Self::LegacyDirectExecutionBlocked => "LEGACY_DIRECT_EXECUTION_BLOCKED",
            Self::Sqlite(_) => "OPERATIONAL_REPOSITORY_SQLITE",
            Self::Store(error) => error.code(),
        }
    }
}

impl From<rusqlite::Error> for RepositoryError {
    fn from(source: rusqlite::Error) -> Self {
        Self::Sqlite(source)
    }
}

impl From<OperationalStoreError> for RepositoryError {
    fn from(source: OperationalStoreError) -> Self {
        Self::Store(source)
    }
}
