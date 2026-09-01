// Connected Backup Platform v2 domain contracts.

mod entities;
mod errors;
mod ids;
mod state;

pub use entities::{
    OccurrenceIdentity, PlanRevisionDraft, PublishedPlanRevision, RevisionDestination,
};
pub use errors::DomainError;
pub use ids::{
    ArtifactPartId, ArtifactReplicaId, BackupPlanId, BackupRunId, BackupScopeId,
    CatalogRebuildRunId, ConfirmationTokenId, ConnectionProfileId, DatabaseTargetId, DestinationId,
    IpcRequestId, OperationRunId, PlanOccurrenceId, PlanRevisionId, RecoveryPointId, RestoreRunId,
    SecretRefId, StoredObjectId, VerificationRunId, WorkAttemptId, WorkItemId, WorkspaceId,
};
pub use state::{
    transition_backup_run, transition_plan_occurrence, transition_recovery_point,
    transition_work_attempt, transition_work_item, BackupRunCommand, BackupRunState,
    DestinationRequirement, OccurrenceCommand, OccurrenceOrigin, PlanOccurrenceState,
    ProtectionCompliance, RecoveryPointCommand, RecoveryPointState, RestoreEligibility,
    SnapshotArtifactCompleteness, SnapshotReplicaDurability, WorkAttemptCommand, WorkAttemptState,
    WorkItemCommand, WorkItemKind, WorkItemState,
};
