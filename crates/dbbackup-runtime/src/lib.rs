// Durable dispatcher runtime and Windows process supervision.

mod capture;
mod dispatcher;
mod notification;
mod reconcile;
mod replica_repair_worker;
mod restore_worker;
mod retention_worker;
mod spec;
#[cfg(windows)]
mod supervisor;
mod verification_worker;

pub use capture::CapturedStream;
#[cfg(windows)]
pub use dispatcher::WindowsProcessLauncher;
pub use dispatcher::{
    DispatchConfig, DispatchError, DispatchOutcome, DispatchPlan, DispatchPlanner, Dispatcher,
    ManagedProcess, ManagedProcessExit, ManagedProcessIdentity, ProcessLauncher, RuntimeClock,
    SystemRuntimeClock,
};
pub use notification::{
    NotificationDelivery, NotificationDeliveryContractError, NotificationDeliveryError,
    NotificationDeliveryReceipt, NotificationDeliveryRequest, NotificationDeliveryWorker,
    NotificationWorkerError, NotificationWorkerOutcome, WebhookNotificationDelivery,
};
#[cfg(windows)]
pub use reconcile::WindowsProcessInspector;
pub use reconcile::{
    snapshot_spawned_process, ProcessInspector, ProcessObservation, ProcessProbe, ReconcileConfig,
    ReconcileError, ReconcileOutcome, SpawnedProcessSnapshot, StartupReconciler,
    VerifiedRecoveryPointSource,
};
pub use replica_repair_worker::{
    ReplicaRepairResolutionErrorV2, ReplicaRepairResolverV2, ReplicaRepairWorkerErrorV2,
    ReplicaRepairWorkerOutcomeV2, ReplicaRepairWorkerV2, ResolvedReplicaRepairV2,
};
pub use restore_worker::{
    RestoreAdmission, RestoreExecutionError, RestoreExecutionEvidence, RestoreExecutor,
    RestoreValidationError, RestoreValidationEvidence, RestoreWorker, RestoreWorkerConfig,
    RestoreWorkerError, RestoreWorkerOutcome,
};
pub use retention_worker::{
    RetentionDeleteCancellationToken, RetentionDeleteOperatorResolverV2,
    RetentionDeleteResolutionErrorV2, RetentionDeleteWorkerErrorV2, RetentionDeleteWorkerV2,
};
pub use spec::{
    hash_executable, LaunchSpec, SupervisorError, TrustedDependency, MAX_TRUSTED_DEPENDENCIES,
    MAX_TRUSTED_DEPENDENCY_PATH_UNITS,
};
#[cfg(windows)]
pub use supervisor::{
    ProcessExit, ProcessSupervisor, StreamingProcessExit, SupervisedProcess,
    SupervisedStreamingProcess,
};
pub use verification_worker::{
    VerificationAdmission, VerificationExecutionError, VerificationExecutionEvidence,
    VerificationExecutor, VerificationWorker, VerificationWorkerConfig, VerificationWorkerError,
    VerificationWorkerOutcome,
};
