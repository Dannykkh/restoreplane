// restoreplane-catalog — S08: SQLite 로컬 캐시(신뢰 불가) + 서명 매니페스트 rebuild/게이트.
//
// 설계 전제(H5/P0-6): 백업 호스트가 랜섬웨어에 장악돼도 오프사이트 단독 복원이 가능해야 한다.
// → 로컬 SQLite는 조회 UX용 캐시일 뿐이며, 복원/검증 판단은 목적지에 동봉된 ed25519 서명
//   매니페스트를 다시 읽어 재검증해서 내린다(gate.rs). 카탈로그가 소실돼도 `catalog rebuild`가
//   목적지 매니페스트만으로 전체 이력을 재구성한다(rebuild.rs).
//
// 매니페스트/서명은 restoreplane-pipeline의 Manifest/SignedManifest(ed25519)를 그대로 재사용한다
// (병렬 매니페스트 타입을 새로 만들지 않는다).

// verify 등 하위 크레이트가 rusqlite 타입을 직접 의존하지 않고 쓰도록 재노출한다.
pub use rusqlite;

pub mod conn;
pub mod connection;
pub mod gate;
pub mod migrations;
pub mod overlap;
pub mod query;
pub mod rebuild;
pub mod repositories;
pub mod source;
pub mod upsert;
pub mod writer;

pub use conn::{open_reader_conn, open_writer_conn, SCHEMA_SQL};
pub use connection::{
    ConnectionDiagnostics, OperationalReader, OperationalStore, OperationalStoreError,
};
pub use gate::{
    load_and_verify_manifest_for_restore, verify_bytes, EmbeddedKeyVerifier, GateError,
    ManifestVerifier, PinnedKeyVerifier,
};
pub use migrations::{
    apply_operational_store_migrations, apply_operational_store_v2, apply_operational_store_v3,
    apply_operational_store_v4, operational_schema_v2_checksum, operational_schema_v3_checksum,
    operational_schema_v4_checksum, MigrationError, MigrationOutcome, OPERATIONAL_SCHEMA_V2,
    OPERATIONAL_SCHEMA_V3, OPERATIONAL_SCHEMA_V4, WHOLE_DATABASE_SELECTOR_HASH_BLAKE3,
    WHOLE_DATABASE_SELECTOR_JSON,
};

pub const DEFAULT_WORKSPACE_ID: &str = "00000000-0000-4000-8000-000000000001";
pub use overlap::{try_acquire_run, OverlapError, RunGuard, MAX_RUN_SECONDS};
pub use query::{
    artifacts_for_backup, latest_backup_per_job, latest_run_per_job, recent_backups,
    ArtifactRecord, BackupRecord, LatestRunRecord,
};
pub use rebuild::{catalog_rebuild, RebuildArgs, RebuildReport};
pub use repositories::prepare_catalog_rebuild_v2;
pub use repositories::{
    ActivityEventRecord, ActivityEventSpec, ActivityRepository, AdmissionOutcome, AdmissionRequest,
    ArtifactPartSpec, ArtifactReplicaSpec, ArtifactReplicaState, AttentionObservation,
    AttentionRecord, BackupClaimContext, BackupCommitRecoveryContext, BackupDestinationBinding,
    BackupScopeMode, BackupScopeObjectPage, BackupScopeRecord, BackupScopeRepository,
    BackupWorkSpec, BeginCommitDisposition, CancelRequestOutcome, CatalogProjectionRepository,
    CatalogRebuildApplyModeV2, CatalogRebuildCandidateV2, CatalogRebuildDispositionV2,
    CatalogRebuildPlanV2, CatalogRebuildRejectReasonV2, CatalogRebuildRunModeV2,
    CatalogRebuildSourceKindV2, CatalogRebuildWorkSpec, CatalogRebuildWorkUpsert,
    CertificationCapability, CertificationRepository, ClaimCompletionSpec, ClaimRequest,
    ClaimTerminalOutcome, ClaimedReplicaRepairV2, ClaimedRetentionDeleteV2, ClaimedWork,
    CompleteRecoveryPointSpec, ConfigCutoverConfirmationIssueResult,
    ConfigCutoverConfirmationRequestSpec, ConfigCutoverRepository, ConfigCutoverResult,
    ConfigCutoverRollbackReason, ConfigCutoverRollbackResult, ConfigCutoverRollbackSpec,
    ConfigCutoverSpec, ConfigurationRepository, ConfirmationRecord, ConfirmationSpec,
    ConnectionExecutionBinding, ConnectionProbeSpec, ConnectionProfileSpec, ConsoleActivityEvent,
    ConsoleBackupScopeHealth, ConsoleConnection, ConsoleControlBackup, ConsoleDatabaseTarget,
    ConsoleDestination, ConsoleDestinationRole, ConsoleDiscoveredDatabase, ConsoleEngineCapability,
    ConsoleEngineMatrix, ConsoleEngineMatrixRow, ConsoleNotificationChannel, ConsolePlan,
    ConsoleQueryRepository, ConsoleResourcePool, ConsoleRestoreTargetFacts, ConsoleRetentionHealth,
    ConsoleSchedulerHealth, ConsoleSettings, ConsoleSettingsInventoryCounts, ConsoleSnapshot,
    ConsoleSnapshotDetailPage, ConsoleSnapshotPart, ConsoleSnapshotQuery, ConsoleSnapshotReplica,
    ConsoleSnapshotRestoreDrillSummary, ConsoleSnapshotVerificationSummary, ConsoleSpoolVolume,
    ConsoleToolInventoryItem, ConsoleWorkItem, ConsoleWorkItemProgress, ConsoleWorkItemSubject,
    ControlBackupReport, ControlBackupSpec, ControlStateProbe, ControlStoreRepository, CursorPage,
    DatabaseObjectDiscoverySpec, DatabaseTargetSpec, DeliveryClaim, DeliveryClaimRequest,
    DestinationDeleteBindingV2, DestinationProbeBinding, DestinationProbeSpec,
    DestinationRepairBindingV2, DestinationSecretBindingSpec, DestinationSecretRoleV2,
    DestinationSpec, DestructiveStepDisposition, DiagnosticsRepository,
    DiscoveredDatabaseObjectRecord, DiscoveredDatabaseObjectSpec, DiscoveredDatabaseSpec,
    DiscoverySnapshotReport, DiscoverySnapshotSpec, EffectiveCapability, EngineCertificationSpec,
    ExpiredClaim, ExplicitBackupScopeSpec, G1EvidenceBindingV1, IpcAtomicMutationResult,
    IpcCompletionSpec, IpcLedgerReservation, IpcRepository, IpcRequestSpec, IpcStoredOutcome,
    ManualBackupEnqueueResult, ManualBackupRequestSpec, ManualBackupWorkItem,
    ManualVerificationEnqueueResult, ManualVerificationRequestSpec, MinimumReaderVersionSnapshot,
    NativeToolSpec, NotificationChannelSpec, NotificationRepository, NotificationRuleSpec,
    OccurrenceMaterialization, OccurrenceMaterializationSpec, OccurrenceSpec, OccurrenceUpsert,
    OperationClaimContext, OperationWorkSpec, OperationWorkUpsert, OutboxSpec, OutboxUpsert,
    OverwriteRestoreSpec, PackageCertificationImportOutcome, PersistedAuthorityOrigin,
    PersistedAuthoritySnapshot, PersistedCutoverMode, PersistedCutoverRecoveryDisposition,
    PersistedExecutionAuthority, PlanDraftSpec, PostBackupVerificationDisposition,
    PostBackupVerificationOutcome, ProcessExitSpec, ProcessStartSpec, RecordedProcessIdentity,
    RecoveryRepository, ReplicaReaderStatus, ReplicaRepairEnqueueResult, ReplicaRepairRequestSpec,
    ReplicaRepairSecretRefsV2, ReplicaRepairWorkSpec, RepositoryError, ResourcePoolSpec,
    ResourceRequirement, RestoreClaimContext, RestoreEnqueueResult,
    RestoreOverwriteConfirmationIssueResult, RestoreOverwriteConfirmationRequestSpec,
    RestorePurpose, RetentionConfirmationIssueResult, RetentionConfirmationRequestSpec,
    RetentionDeleteEnqueueResult, RetentionDeleteSettlementOutcome, RetentionDeleteSettlementSpec,
    RetentionDeleteSpec, RetentionMaintenanceReport, RetentionMaintenanceSpec,
    RetentionPlannerRepository, RetentionRuntimeRepository, RetentionSafetyControlSpec,
    RetentionTombstoneResult, RetentionTombstoneSpec, RetryDisposition, RuntimeRepository,
    SafeRestoreEnqueueResult, SafeRestoreSpec, ScheduledPlanRecord, SchedulerCursorAdvance,
    SchedulerCursorAdvanceSpec, SchedulerRepository, SecretRefRecord, SecretRefRepository,
    SecretRefSpec, SelectedDatabaseTarget, ServiceDiagnostics, ShadowImportReport,
    ShadowImportRepository, SnapshotAxes, SnapshotDetailRepository, SnapshotRecord,
    SpoolRequirement, SpoolVolumeSpec, StandaloneAuthorityBootstrapResult,
    StandaloneAuthorityBootstrapSpec, StoredObjectSpec, TargetDiscoveryStatus,
    TelemetryArchiveBatch, TelemetryArchiveCommit, TelemetryMaintenanceRepository, ToolObservation,
    UnsupportedVerificationSpec, VerificationClaimContext, VerificationEnqueueResult,
    VerificationRepository, VerificationWorkSpec, VerifiedPackageCertificationSpec, WorkspacePage,
    WorkspaceRecord, WorkspaceRepository, WorkspaceSpec,
};
pub use source::{LocalManifestSource, ManifestSource};
pub use upsert::{upsert_backup_from_manifest, upsert_backup_from_manifest_conn};
pub use writer::CatalogWriterHandle;
