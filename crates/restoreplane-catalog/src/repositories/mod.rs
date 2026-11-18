mod activity;
mod backup_scope;
mod catalog_projection;
mod certification;
mod configuration;
mod console_query;
mod control_store;
mod cutover;
mod diagnostics;
mod error;
mod ipc;
mod notification;
mod rebuild_v2;
mod recovery;
mod retention_planner;
mod retention_runtime;
mod runtime;
mod scheduler;
mod secret_ref;
mod shadow_import;
mod snapshot_detail;
mod telemetry_maintenance;
mod verification;
mod workbench;
mod workspace;
pub use workbench::{WorkbenchBindingRepository, WorkbenchBindingSnapshot};

pub(super) fn server_identity_from_capabilities(
    engine: &str,
    capabilities_json: &str,
) -> Option<String> {
    let capabilities = serde_json::from_str::<serde_json::Value>(capabilities_json).ok()?;
    let raw = capabilities.get("serverIdentity")?.as_str()?;
    canonical_server_identity(engine, raw)
}

pub(super) fn canonical_server_identity(engine: &str, raw: &str) -> Option<String> {
    let value = raw.trim();
    match engine {
        "postgresql" => {
            let raw_identifier = value.strip_prefix("postgresql-system-id:")?;
            let parsed = raw_identifier.parse::<u64>().ok()?;
            (parsed != 0 && parsed.to_string() == raw_identifier).then(|| value.to_owned())
        }
        "mysql" => {
            let raw_identifier = value.strip_prefix("mysql-server-uuid:")?;
            let parsed = uuid::Uuid::parse_str(raw_identifier).ok()?;
            (!parsed.is_nil()).then(|| format!("mysql-server-uuid:{}", parsed.hyphenated()))
        }
        "mariadb" => {
            let raw_identifier = value.strip_prefix("mariadb-server-uid:")?;
            ((20..=64).contains(&raw_identifier.len())
                && raw_identifier
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=')))
            .then(|| value.to_owned())
        }
        _ => None,
    }
}

pub use activity::{
    ActivityEventRecord, ActivityEventSpec, ActivityRepository, AttentionObservation,
    AttentionRecord,
};
pub use backup_scope::{
    BackupScopeMode, BackupScopeObjectPage, BackupScopeRecord, BackupScopeRepository,
    DatabaseObjectDiscoverySpec, DiscoveredDatabaseObjectRecord, DiscoveredDatabaseObjectSpec,
    ExplicitBackupScopeSpec,
};
pub use catalog_projection::{
    ArtifactPartSpec, ArtifactReplicaSpec, ArtifactReplicaState, CatalogProjectionRepository,
    CompleteRecoveryPointSpec, ControlStateProbe, ReplicaReaderStatus, SnapshotAxes,
    SnapshotRecord, StoredObjectSpec,
};
pub use certification::{
    CertificationCapability, CertificationRepository, EffectiveCapability, EngineCertificationSpec,
    NativeToolSpec, PackageCertificationImportOutcome, ToolObservation, TrustedToolIdentity,
    VerifiedPackageCertificationSpec,
};
pub use configuration::{
    BackupDestinationBinding, ConfigurationRepository, ConnectionExecutionBinding,
    ConnectionProbeSpec, ConnectionProfileSpec, DatabaseTargetSpec, DestinationDeleteBindingV2,
    DestinationProbeBinding, DestinationProbeSpec, DestinationRepairBindingV2,
    DestinationSecretBindingSpec, DestinationSecretRoleV2, DestinationSpec, DiscoveredDatabaseSpec,
    DiscoverySnapshotReport, DiscoverySnapshotSpec, PlanDraftSpec, ReplicaRepairSecretRefsV2,
    SelectedDatabaseTarget, TargetDiscoveryStatus,
};
pub use console_query::{
    ConsoleActivityEvent, ConsoleBackupScopeHealth, ConsoleConnection, ConsoleControlBackup,
    ConsoleDatabaseTarget, ConsoleDestination, ConsoleDestinationRole, ConsoleDiscoveredDatabase,
    ConsoleEngineCapability, ConsoleEngineMatrix, ConsoleEngineMatrixRow,
    ConsoleNotificationChannel, ConsolePlan, ConsoleQueryRepository, ConsoleResourcePool,
    ConsoleRestoreTargetFacts, ConsoleRetentionHealth, ConsoleSchedulerHealth, ConsoleSettings,
    ConsoleSettingsInventoryCounts, ConsoleSnapshot, ConsoleSnapshotPart, ConsoleSnapshotQuery,
    ConsoleSnapshotReplica, ConsoleSpoolVolume, ConsoleToolInventoryItem, ConsoleWorkItem,
    ConsoleWorkItemProgress, ConsoleWorkItemSubject, CursorPage,
};
pub use control_store::{ControlBackupReport, ControlBackupSpec, ControlStoreRepository};
pub use cutover::{
    ConfigCutoverRepository, ConfigCutoverResult, ConfigCutoverRollbackReason,
    ConfigCutoverRollbackResult, ConfigCutoverRollbackSpec, ConfigCutoverSpec, G1EvidenceBindingV1,
    MinimumReaderVersionSnapshot, PersistedAuthorityOrigin, PersistedAuthoritySnapshot,
    PersistedCutoverMode, PersistedCutoverRecoveryDisposition, PersistedExecutionAuthority,
    StandaloneAuthorityBootstrapResult, StandaloneAuthorityBootstrapSpec,
};
pub use diagnostics::{
    ActivityHealth, AttentionHealth, DestinationHealth, DiagnosticsRepository, LeaseHealth,
    MigrationHealth, NotificationHealth, PermitHealth, QueueHealth, SchemaHealth,
    ServiceDiagnostics, SpoolHealth, StateCount, SupportActivityEvent, ToolHealth,
};
pub use error::RepositoryError;
pub use ipc::{
    ConfigCutoverConfirmationIssueResult, ConfigCutoverConfirmationRequestSpec, ConfirmationRecord,
    ConfirmationSpec, IpcAtomicMutationResult, IpcCompletionSpec, IpcLedgerReservation,
    IpcRepository, IpcRequestSpec, IpcStoredOutcome, ManualBackupEnqueueResult,
    ManualBackupRequestSpec, ManualBackupWorkItem, ManualVerificationEnqueueResult,
    ManualVerificationRequestSpec, OverwriteRestoreSpec, ReplicaRepairEnqueueResult,
    ReplicaRepairRequestSpec, RestoreEnqueueResult, RestoreOverwriteConfirmationIssueResult,
    RestoreOverwriteConfirmationRequestSpec, RetentionConfirmationIssueResult,
    RetentionConfirmationRequestSpec, RetentionDeleteEnqueueResult, RetentionDeleteSpec,
    RetentionTombstoneResult, RetentionTombstoneSpec,
};
pub use notification::{
    DeliveryClaim, DeliveryClaimRequest, NotificationChannelSpec, NotificationRepository,
    NotificationRuleSpec, OutboxSpec, OutboxUpsert,
};
pub use rebuild_v2::{
    prepare_catalog_rebuild_v2, CatalogRebuildApplyModeV2, CatalogRebuildCandidateV2,
    CatalogRebuildDispositionV2, CatalogRebuildPlanV2, CatalogRebuildRejectReasonV2,
};
pub use recovery::{RecoveryRepository, RestorePurpose, SafeRestoreEnqueueResult, SafeRestoreSpec};
pub use retention_planner::{RetentionPlannerRepository, RetentionSafetyControlSpec};
pub use retention_runtime::{
    RetentionMaintenanceReport, RetentionMaintenanceSpec, RetentionRuntimeRepository,
};
pub use runtime::{
    AdmissionOutcome, AdmissionRequest, BackupClaimContext, BackupCommitRecoveryContext,
    BackupWorkSpec, BeginCommitDisposition, CancelRequestOutcome, CatalogRebuildRunModeV2,
    CatalogRebuildSourceKindV2, CatalogRebuildWorkSpec, CatalogRebuildWorkUpsert,
    ClaimCompletionSpec, ClaimRequest, ClaimTerminalOutcome, ClaimedReplicaRepairV2,
    ClaimedRetentionDeleteV2, ClaimedWork, DestructiveStepDisposition, ExpiredClaim,
    OccurrenceMaterialization, OccurrenceMaterializationSpec, OccurrenceSpec, OccurrenceUpsert,
    OperationClaimContext, OperationWorkSpec, OperationWorkUpsert, ProcessExitSpec,
    ProcessStartSpec, RecordedProcessIdentity, ReplicaRepairWorkSpec, ResourcePoolSpec,
    ResourceRequirement, RestoreClaimContext, RetentionDeleteSettlementOutcome,
    RetentionDeleteSettlementSpec, RetryDisposition, RuntimeRepository, SpoolRequirement,
    SpoolVolumeSpec, VerificationClaimContext,
};
pub use scheduler::{
    ScheduledPlanRecord, SchedulerCursorAdvance, SchedulerCursorAdvanceSpec, SchedulerRepository,
};
pub use secret_ref::{SecretRefRecord, SecretRefRepository, SecretRefSpec};
pub use shadow_import::{ShadowImportReport, ShadowImportRepository};
pub use snapshot_detail::{
    ConsoleSnapshotDetailPage, ConsoleSnapshotRestoreDrillSummary,
    ConsoleSnapshotVerificationSummary, SnapshotDetailRepository,
};
pub use telemetry_maintenance::{
    ArchivedActivityEvent, ArchivedNotificationOutbox, TelemetryArchiveBatch,
    TelemetryArchiveCommit, TelemetryMaintenanceRepository,
};
pub use verification::{
    PostBackupVerificationDisposition, PostBackupVerificationOutcome, UnsupportedVerificationSpec,
    VerificationEnqueueResult, VerificationRepository, VerificationWorkSpec,
};
pub use workspace::{WorkspacePage, WorkspaceRecord, WorkspaceRepository, WorkspaceSpec};
