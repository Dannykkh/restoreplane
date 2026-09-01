use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dbbackup_runtime::ReconcileError;
use thiserror::Error;
use windows_service::define_windows_service;
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{
    self, ServiceControlHandlerResult, ServiceStatusHandle,
};
use windows_service::service_dispatcher;

use crate::application::ServiceApplication;
use crate::backup::{BackupWorkerStartError, NativeBackupExecutor, ServiceBackupWorkerHandle};
use crate::cutover::{
    CutoverReadinessError, CutoverReadinessVerifierV1, FileCutoverReadinessVerifierV1,
    LegacySchedulerProbeV1, WindowsLegacySchedulerProbeV1,
};
use crate::host::{HostConfig, HostError, HostPhase, PipeHostRuntime, ServiceHost, ShutdownReason};
use crate::identity_config::{ServiceIdentityConfig, ServiceIdentityError};
use crate::ipc::{
    current_process_identity, ApplicationFrameHandler, AuthorizationPolicy, DurableIpcDispatcher,
    IpcError, PipeSecurityError, PipeSecurityPolicy, PipeServerConfig, PipeTransportError,
    ReadOnlyApplicationFrameHandler, SecurePipeServer,
};
use crate::operations::{OperationWorkerStartError, ServiceOperationWorkerHandle};
use crate::package_certification::{
    bootstrap_verified_package, PackageCertificationBootstrapError,
};
use crate::repair::{
    NotificationRuntimeStartError, ReplicaRepairRuntimeStartError, ServiceCatalogRebuildPreviewer,
    ServiceNotificationDeliveryRuntimeHandle, ServiceReplicaRepairRuntimeHandle,
};
use crate::restore_runtime::{
    RecoveryExecutionRuntimeStartError, ServiceRecoveryExecutionRuntimeHandle,
    ServiceVerificationRuntimeHandle,
};
use crate::retention_runtime::{RetentionRuntimeStartError, ServiceRetentionRuntimeHandle};
use crate::scheduler::{ServiceSchedulerHandle, ServiceSchedulerStartError};
use crate::secrets::{AclPolicy, SecretStoreError, ServiceSecretStore};
use crate::startup::{
    reconcile_expired_work, ServiceRecoveryRuntimeHandle, ServiceVerifiedRecoveryPointSource,
};

pub const SERVICE_NAME: &str = "DbBackupService";
pub const CONTROL_PIPE_NAME: &str = r"\\.\pipe\DbBackup.control.v1";
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;
const START_WAIT_HINT: Duration = Duration::from_secs(30);
const STOP_WAIT_HINT: Duration = Duration::from_secs(180);
const PIPE_MAX_CONCURRENT_CLIENTS: usize = 32;
const PIPE_IO_TIMEOUT: Duration = Duration::from_secs(5);
const G1_REQUIREMENTS_JSON: &str =
    include_str!("../../../release/gates/g1-evidence-requirements.json");
const G1_MATRIX_JSON: &str = include_str!("../../../release/gates/g1-fault-matrix.json");
static SERVICE_PROCESS_ARGUMENTS: OnceLock<Vec<OsString>> = OnceLock::new();

#[derive(Clone, Debug, PartialEq, Eq)]
struct ServiceRuntimeRoots {
    install_root: PathBuf,
    data_root: PathBuf,
}

impl ServiceRuntimeRoots {
    fn from_current_executable(data_root: PathBuf) -> Result<Self, HostError> {
        let executable = std::env::current_exe()
            .map_err(|_| HostError::OsApiFailed("current executable path"))?;
        Self::from_executable(&executable, data_root)
    }

    fn from_executable(executable: &Path, data_root: PathBuf) -> Result<Self, HostError> {
        let install_root = executable
            .parent()
            .filter(|path| path.is_absolute())
            .ok_or(HostError::InvalidConfig)?
            .to_path_buf();
        if !valid_absolute_service_root(&data_root)
            || install_root == data_root
            || install_root.starts_with(&data_root)
            || data_root.starts_with(&install_root)
        {
            return Err(HostError::InvalidConfig);
        }
        Ok(Self {
            install_root,
            data_root,
        })
    }

    fn install_root(&self) -> &Path {
        &self.install_root
    }

    fn data_root(&self) -> &Path {
        &self.data_root
    }

    fn tool_root(&self) -> PathBuf {
        self.install_root.join("tools")
    }

    fn secret_root(&self) -> PathBuf {
        self.data_root.join("secrets")
    }

    fn spool_root(&self) -> PathBuf {
        self.data_root.join("spool")
    }

    fn update_root(&self) -> PathBuf {
        self.data_root.join("updates")
    }
}

pub fn run_dispatcher() -> windows_service::Result<()> {
    let _ = SERVICE_PROCESS_ARGUMENTS.set(std::env::args_os().skip(1).collect());
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
}

define_windows_service!(ffi_service_main, service_main);

fn service_main(_arguments: Vec<OsString>) {
    let _ = run_service();
}

fn run_service() -> Result<(), ScmError> {
    let (shutdown_tx, shutdown_rx) = mpsc::channel();
    let event_handler = move |control| match control {
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        ServiceControl::Stop => {
            let _ = shutdown_tx.send(ShutdownReason::Stop);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Preshutdown => {
            let _ = shutdown_tx.send(ShutdownReason::Preshutdown);
            ServiceControlHandlerResult::NoError
        }
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;
    report(
        &status_handle,
        HostPhase::Starting,
        ServiceExitCode::Win32(0),
    )?;

    let host = match start_default_host() {
        Ok(host) => host,
        Err(error) => {
            report(
                &status_handle,
                HostPhase::Stopped,
                ServiceExitCode::ServiceSpecific(stable_exit_code(error.code())),
            )?;
            return Err(error);
        }
    };
    report(
        &status_handle,
        host.health().phase(),
        ServiceExitCode::Win32(0),
    )?;

    let reason = shutdown_rx.recv().map_err(|_| ScmError::ControlChannel)?;
    status_handle.set_service_status(shutdown_pending_status())?;
    let shutdown_result = host.shutdown(reason);
    let exit_code = match &shutdown_result {
        Ok(()) => ServiceExitCode::Win32(0),
        Err(error) => ServiceExitCode::ServiceSpecific(stable_exit_code(error.code())),
    };
    report(&status_handle, HostPhase::Stopped, exit_code)?;
    shutdown_result.map_err(ScmError::Host)
}

fn start_default_host() -> Result<ServiceHost<PipeHostRuntime>, ScmError> {
    let data_root = configured_data_root()?;
    let roots = ServiceRuntimeRoots::from_current_executable(data_root.clone())?;
    start_host_from_runtime_roots(
        "default".to_owned(),
        data_root.join("operational-v2.db"),
        data_root.join("service-identity.json"),
        CONTROL_PIPE_NAME.to_owned(),
        roots,
    )
}

#[cfg(test)]
fn start_host_from_paths(
    instance_key: String,
    database_path: PathBuf,
    identity_config_path: PathBuf,
    pipe_name: String,
) -> Result<ServiceHost<PipeHostRuntime>, ScmError> {
    let data_root = identity_config_path
        .parent()
        .ok_or(HostError::InvalidConfig)?
        .to_path_buf();
    let roots = ServiceRuntimeRoots::from_current_executable(data_root)?;
    start_test_host_from_runtime_roots(
        instance_key,
        database_path,
        identity_config_path,
        pipe_name,
        roots,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProgramOwnedClaimRuntime {
    Recovery,
    Restore,
    Verification,
    Scheduler,
    NotificationDelivery,
    ReplicaRepair,
    Retention,
}

const OPERATIONAL_CLAIM_RUNTIMES: [ProgramOwnedClaimRuntime; 7] = [
    ProgramOwnedClaimRuntime::Recovery,
    ProgramOwnedClaimRuntime::Restore,
    ProgramOwnedClaimRuntime::Verification,
    ProgramOwnedClaimRuntime::Scheduler,
    ProgramOwnedClaimRuntime::NotificationDelivery,
    ProgramOwnedClaimRuntime::ReplicaRepair,
    ProgramOwnedClaimRuntime::Retention,
];

fn program_owned_claim_runtimes(phase: HostPhase) -> &'static [ProgramOwnedClaimRuntime] {
    match phase {
        HostPhase::RehearsalReady | HostPhase::ProductionReady => &OPERATIONAL_CLAIM_RUNTIMES,
        HostPhase::Starting
        | HostPhase::DiagnosticsOnly
        | HostPhase::Draining
        | HostPhase::Stopped => &[],
    }
}

fn start_host_from_runtime_roots(
    instance_key: String,
    database_path: PathBuf,
    identity_config_path: PathBuf,
    pipe_name: String,
    roots: ServiceRuntimeRoots,
) -> Result<ServiceHost<PipeHostRuntime>, ScmError> {
    start_host_from_runtime_roots_with_package_bootstrap(
        instance_key,
        database_path,
        identity_config_path,
        pipe_name,
        roots,
        bootstrap_package_certification,
    )
}

#[cfg(test)]
fn start_test_host_from_runtime_roots(
    instance_key: String,
    database_path: PathBuf,
    identity_config_path: PathBuf,
    pipe_name: String,
    roots: ServiceRuntimeRoots,
) -> Result<ServiceHost<PipeHostRuntime>, ScmError> {
    start_host_from_runtime_roots_with_package_bootstrap(
        instance_key,
        database_path,
        identity_config_path,
        pipe_name,
        roots,
        |_store, _install_root, _observed_at_ms, _service_sid, _owner_sid| Ok(()),
    )
}

fn bootstrap_package_certification(
    store: &dbbackup_catalog::OperationalStore,
    install_root: &Path,
    observed_at_ms: i64,
    service_sid: &str,
    owner_sid: &str,
) -> Result<(), ScmError> {
    let outcome =
        bootstrap_verified_package(store, install_root, observed_at_ms, service_sid, owner_sid)?;
    tracing::info!(
        native_tools = outcome.tool_count(),
        provider_certifications = outcome.certification_count(),
        deduplicated = outcome.deduplicated(),
        "signed native provider package certification imported"
    );
    Ok(())
}

fn start_host_from_runtime_roots_with_package_bootstrap(
    instance_key: String,
    database_path: PathBuf,
    identity_config_path: PathBuf,
    pipe_name: String,
    roots: ServiceRuntimeRoots,
    package_bootstrap: impl FnOnce(
        &dbbackup_catalog::OperationalStore,
        &Path,
        i64,
        &str,
        &str,
    ) -> Result<(), ScmError>,
) -> Result<ServiceHost<PipeHostRuntime>, ScmError> {
    if database_path.parent() != Some(roots.data_root())
        || identity_config_path.parent() != Some(roots.data_root())
    {
        return Err(HostError::InvalidConfig.into());
    }
    let process_identity = current_process_identity()?;
    let service_sid = process_identity.user_sid().clone();
    let identity = ServiceIdentityConfig::load(&identity_config_path, &service_sid)?;
    let owner_sid = identity.owner_sid().clone();
    let approved_administrators = identity.approved_administrator_sids().to_vec();
    let cutover_config = identity.cutover_verifier().cloned();
    let database_existed_at_start = database_path
        .try_exists()
        .map_err(|_| HostError::OsApiFailed("database path probe"))?;
    let standalone_bootstrap = match (identity.standalone_bootstrap(), database_existed_at_start) {
        (Some(bootstrap), false) => {
            bootstrap.verify_current_executable()?;
            let committed_at_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| HostError::OsApiFailed("system clock"))?
                .as_millis();
            let committed_at_ms = i64::try_from(committed_at_ms)
                .map_err(|_| HostError::OsApiFailed("system clock"))?;
            Some(bootstrap.authority_spec(committed_at_ms))
        }
        _ => None,
    };
    let trusted_signing_keys = identity.trusted_signing_keys().clone();
    let install_directory = roots.install_root().to_path_buf();
    let data_directory = roots.data_root().to_path_buf();
    let authorization = AuthorizationPolicy::new_unbound_owner(
        service_sid.clone(),
        owner_sid.clone(),
        approved_administrators.clone(),
    )?;
    let secret_acl = AclPolicy::new(service_sid.clone(), owner_sid.clone())?;
    let pipe_security = PipeSecurityPolicy::new(
        service_sid.clone(),
        owner_sid.clone(),
        approved_administrators,
    )?;
    let pipe_config =
        PipeServerConfig::new(pipe_name, PIPE_MAX_CONCURRENT_CLIENTS, PIPE_IO_TIMEOUT)?;
    let runtime = Arc::new(PipeHostRuntime::new());
    let update_pending = roots.update_root().join("update-pending.json").is_file();
    let manifest_instance_id = instance_key.clone();
    let mut host_config = HostConfig::new(instance_key, database_path.clone())?
        .with_current_build_g1_proven(cutover_config.is_some() || standalone_bootstrap.is_some())
        .with_update_pending(update_pending);
    if let Some(bootstrap) = standalone_bootstrap {
        host_config = host_config.with_pristine_standalone_bootstrap(bootstrap);
    }
    let host = ServiceHost::start(host_config, Arc::clone(&runtime))?;
    let authority = host.health().authority_snapshot();
    let operational_store = host.operational_store();
    let application = match operational_store.as_ref() {
        Some(store) => {
            let claim_runtime_plan = program_owned_claim_runtimes(host.health().phase());
            if !claim_runtime_plan.is_empty() {
                let observed_at_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|_| HostError::OsApiFailed("system clock"))?
                    .as_millis();
                let observed_at_ms = i64::try_from(observed_at_ms)
                    .map_err(|_| HostError::OsApiFailed("system clock"))?;
                if let Err(error) = package_bootstrap(
                    store,
                    roots.install_root(),
                    observed_at_ms,
                    service_sid.as_str(),
                    owner_sid.as_str(),
                ) {
                    let _ = host.shutdown(ShutdownReason::Stop);
                    return Err(error);
                }
            }
            let secrets = Arc::new(ServiceSecretStore::open(
                roots.secret_root(),
                store.secret_refs(),
                secret_acl,
            )?);
            let backup_executor = Arc::new(
                NativeBackupExecutor::new(
                    store.clone(),
                    Arc::clone(&secrets),
                    install_directory.clone(),
                    roots.spool_root(),
                    manifest_instance_id,
                )
                .map_err(BackupWorkerStartError::Initialization)?,
            );
            let mut effective_signing_keys = trusted_signing_keys;
            effective_signing_keys.insert(
                backup_executor.signing_key_id().to_owned(),
                backup_executor.verifying_key(),
            );
            let startup_recovery = reconcile_expired_work(
                store,
                Arc::new(ServiceVerifiedRecoveryPointSource::new(
                    store.clone(),
                    Arc::clone(&secrets),
                    effective_signing_keys.clone(),
                )),
            )?;
            if startup_recovery.attention_required > 0 || startup_recovery.cleanup_required > 0 {
                tracing::warn!(
                    recovered_commits = startup_recovery.recovered_commits,
                    requeued = startup_recovery.requeued,
                    cleanup_required = startup_recovery.cleanup_required,
                    attention_required = startup_recovery.attention_required,
                    "startup reconciliation preserved work requiring operator attention"
                );
            } else if startup_recovery.requeued > 0 || startup_recovery.recovered_commits > 0 {
                tracing::info!(
                    recovered_commits = startup_recovery.recovered_commits,
                    requeued = startup_recovery.requeued,
                    "startup reconciliation recovered expired work"
                );
            }
            let spool_report = backup_executor
                .scavenge_orphaned_spool()
                .map_err(BackupWorkerStartError::Initialization)?;
            if spool_report.removed_attempts() > 0 || spool_report.skipped_unmanaged() > 0 {
                tracing::info!(
                    removed_attempts = spool_report.removed_attempts(),
                    preserved_active = spool_report.preserved_active(),
                    skipped_unmanaged = spool_report.skipped_unmanaged(),
                    "managed spool startup scavenge completed"
                );
            }
            let application =
                ServiceApplication::new(host.health_probe(), operational_store.clone())
                    .with_secret_store(Arc::clone(&secrets))
                    .with_maintenance_root(data_directory.clone())
                    .with_catalog_rebuild_previewer(Arc::new(ServiceCatalogRebuildPreviewer::new(
                        store.configuration(),
                        Arc::clone(&secrets),
                        effective_signing_keys.clone(),
                    )));
            if !claim_runtime_plan.is_empty() {
                for runtime_kind in claim_runtime_plan {
                    match runtime_kind {
                        ProgramOwnedClaimRuntime::Recovery => {
                            let recovery_runtime = Arc::new(ServiceRecoveryRuntimeHandle::spawn(
                                store.clone(),
                                Arc::clone(&secrets),
                                effective_signing_keys.clone(),
                            )?);
                            runtime.attach_claim_runtime(recovery_runtime)?;
                        }
                        ProgramOwnedClaimRuntime::Restore => {
                            let restore_runtime =
                                Arc::new(ServiceRecoveryExecutionRuntimeHandle::spawn(
                                    store.clone(),
                                    database_path.clone(),
                                    Arc::clone(&secrets),
                                    effective_signing_keys.clone(),
                                    roots.tool_root(),
                                    roots.spool_root().join("restore"),
                                    host.health_probe(),
                                )?);
                            runtime.attach_claim_runtime(restore_runtime)?;
                        }
                        ProgramOwnedClaimRuntime::Verification => {
                            let verification_runtime =
                                Arc::new(ServiceVerificationRuntimeHandle::spawn(
                                    store.clone(),
                                    database_path.clone(),
                                    Arc::clone(&secrets),
                                    effective_signing_keys.clone(),
                                    roots.tool_root(),
                                    roots.spool_root().join("verification"),
                                    host.health_probe(),
                                )?);
                            runtime.attach_claim_runtime(verification_runtime)?;
                        }
                        ProgramOwnedClaimRuntime::Scheduler => {
                            let scheduler = Arc::new(ServiceSchedulerHandle::spawn(
                                store.clone(),
                                host.health_probe(),
                            )?);
                            runtime.attach_claim_runtime(scheduler)?;
                        }
                        ProgramOwnedClaimRuntime::NotificationDelivery => {
                            let notification_runtime =
                                Arc::new(ServiceNotificationDeliveryRuntimeHandle::spawn(
                                    store.clone(),
                                    host.health_probe(),
                                )?);
                            runtime.attach_claim_runtime(notification_runtime)?;
                        }
                        ProgramOwnedClaimRuntime::ReplicaRepair => {
                            let repair_runtime =
                                Arc::new(ServiceReplicaRepairRuntimeHandle::spawn(
                                    store.clone(),
                                    Arc::clone(&secrets),
                                    host.health_probe(),
                                    effective_signing_keys.clone(),
                                    dbbackup_pipeline::PipelineCfg::default(),
                                )?);
                            runtime.attach_claim_runtime(repair_runtime)?;
                        }
                        ProgramOwnedClaimRuntime::Retention => {
                            let retention_runtime = Arc::new(ServiceRetentionRuntimeHandle::spawn(
                                store.clone(),
                                Arc::clone(&secrets),
                                host.health_probe(),
                            )?);
                            runtime.attach_claim_runtime(retention_runtime)?;
                        }
                    }
                }
                let operation_worker =
                    Arc::new(ServiceOperationWorkerHandle::spawn_with_runtime_roots(
                        store.clone(),
                        Arc::clone(&secrets),
                        data_directory.join("support-bundles"),
                        Some(data_directory.join("telemetry-archives")),
                        Some(roots.tool_root()),
                    )?);
                let backup_worker = Arc::new(ServiceBackupWorkerHandle::spawn_with_executor(
                    store.clone(),
                    backup_executor,
                    host.health_probe(),
                )?);
                runtime.attach_claim_runtime(Arc::clone(&operation_worker))?;
                runtime.attach_claim_runtime(Arc::clone(&backup_worker))?;
                application
                    .with_operation_worker(operation_worker)
                    .with_backup_worker(backup_worker)
            } else {
                application
            }
        }
        None => ServiceApplication::new(host.health_probe(), None),
    };
    let application = Arc::new(application);
    let cutover_verifier = match cutover_config {
        Some(config) => {
            let scheduler =
                Arc::new(WindowsLegacySchedulerProbeV1) as Arc<dyn LegacySchedulerProbeV1>;
            match FileCutoverReadinessVerifierV1::new(
                data_directory.join(config.evidence_directory()),
                config.build_identity().clone(),
                host.health_probe(),
                scheduler,
                G1_REQUIREMENTS_JSON,
                G1_MATRIX_JSON,
            ) {
                Ok(verifier) => Some(Arc::new(verifier) as Arc<dyn CutoverReadinessVerifierV1>),
                Err(error) => {
                    let _ = host.shutdown(ShutdownReason::Stop);
                    return Err(ScmError::CutoverReadiness(error));
                }
            }
        }
        None => None,
    };

    let attach_result = match (
        host.operational_store(),
        host.health().phase() == HostPhase::DiagnosticsOnly,
    ) {
        (Some(store), false) => {
            let dispatcher = match cutover_verifier {
                Some(verifier) => DurableIpcDispatcher::with_cutover_verifier(
                    store.ipc(),
                    Arc::clone(&application),
                    authority,
                    verifier,
                ),
                None => DurableIpcDispatcher::new(store.ipc(), Arc::clone(&application), authority),
            };
            let handler = Arc::new(ApplicationFrameHandler::new(authorization, dispatcher));
            runtime.attach(SecurePipeServer::new(pipe_config, pipe_security, handler))
        }
        (Some(_), true) | (None, _) => {
            let handler = Arc::new(ReadOnlyApplicationFrameHandler::new(
                authorization,
                application,
            ));
            runtime.attach(SecurePipeServer::new(pipe_config, pipe_security, handler))
        }
    };
    if let Err(error) = attach_result {
        let _ = host.shutdown(ShutdownReason::Stop);
        return Err(ScmError::Host(error));
    }
    Ok(host)
}

fn configured_data_root() -> Result<PathBuf, ScmError> {
    let configured = parse_service_data_root(
        SERVICE_PROCESS_ARGUMENTS
            .get()
            .into_iter()
            .flatten()
            .cloned(),
    )?;
    let directory = match configured {
        Some(directory) => directory,
        None => default_program_data_directory()?,
    };
    std::fs::create_dir_all(&directory).map_err(|_| HostError::OsApiFailed("create_dir_all"))?;
    Ok(directory)
}

fn default_program_data_directory() -> Result<PathBuf, HostError> {
    let program_data = std::env::var_os("ProgramData").ok_or(HostError::InvalidConfig)?;
    Ok(PathBuf::from(program_data).join("DbBackup"))
}

fn parse_service_data_root(
    arguments: impl IntoIterator<Item = OsString>,
) -> Result<Option<PathBuf>, ServiceArgumentError> {
    let arguments = arguments.into_iter().collect::<Vec<_>>();
    if arguments.is_empty() {
        return Ok(None);
    }
    if arguments.len() != 2 || arguments[0] != "--data-root" {
        return Err(ServiceArgumentError::Invalid);
    }
    let path = PathBuf::from(&arguments[1]);
    if !valid_absolute_service_root(&path) {
        return Err(ServiceArgumentError::Invalid);
    }
    Ok(Some(path))
}

fn valid_absolute_service_root(path: &Path) -> bool {
    path.is_absolute()
        && path.file_name().is_some()
        && !path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
}

fn report(
    status_handle: &ServiceStatusHandle,
    phase: HostPhase,
    exit_code: ServiceExitCode,
) -> windows_service::Result<()> {
    let mut status = status_for_phase(phase);
    status.exit_code = exit_code;
    status_handle.set_service_status(status)
}

pub fn status_for_phase(phase: HostPhase) -> ServiceStatus {
    let (current_state, controls_accepted, checkpoint, wait_hint) = match phase {
        HostPhase::Starting => (
            ServiceState::StartPending,
            ServiceControlAccept::empty(),
            1,
            START_WAIT_HINT,
        ),
        HostPhase::RehearsalReady
        | HostPhase::ProductionReady
        | HostPhase::DiagnosticsOnly
        | HostPhase::Draining => (
            // Draining closes new claims but keeps diagnostics and control IPC alive.
            // SCM stop-pending is reserved for the bounded shutdown path below.
            ServiceState::Running,
            ServiceControlAccept::STOP | ServiceControlAccept::PRESHUTDOWN,
            0,
            Duration::ZERO,
        ),
        HostPhase::Stopped => (
            ServiceState::Stopped,
            ServiceControlAccept::empty(),
            0,
            Duration::ZERO,
        ),
    };
    ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state,
        controls_accepted,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint,
        wait_hint,
        process_id: None,
    }
}

pub fn shutdown_pending_status() -> ServiceStatus {
    ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: ServiceState::StopPending,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 1,
        wait_hint: STOP_WAIT_HINT,
        process_id: None,
    }
}

fn stable_exit_code(code: &str) -> u32 {
    let digest = blake3::hash(code.as_bytes());
    let bytes = digest.as_bytes();
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]).max(1)
}

#[derive(Clone, Copy, Debug, Error)]
enum ServiceArgumentError {
    #[error("SCM_SERVICE_ARGUMENTS_INVALID")]
    Invalid,
}

impl ServiceArgumentError {
    const fn code(self) -> &'static str {
        match self {
            Self::Invalid => "SCM_SERVICE_ARGUMENTS_INVALID",
        }
    }
}

#[derive(Debug, Error)]
enum ScmError {
    #[error("SCM_WINDOWS_SERVICE")]
    WindowsService(#[from] windows_service::Error),
    #[error("SCM_HOST")]
    Host(#[from] HostError),
    #[error("SCM_IDENTITY_CONFIG")]
    IdentityConfig(#[from] ServiceIdentityError),
    #[error("SCM_IPC_PROTOCOL")]
    IpcProtocol(#[from] IpcError),
    #[error("SCM_PIPE_SECURITY")]
    PipeSecurity(#[from] PipeSecurityError),
    #[error("SCM_PIPE_TRANSPORT")]
    PipeTransport(#[from] PipeTransportError),
    #[error("SCM_CONTROL_CHANNEL")]
    ControlChannel,
    #[error("SCM_SERVICE_ARGUMENTS")]
    ServiceArguments(#[from] ServiceArgumentError),
    #[error("SCM_CUTOVER_READINESS")]
    CutoverReadiness(#[from] CutoverReadinessError),
    #[error("SCM_SECRET_STORE")]
    SecretStore(#[from] SecretStoreError),
    #[error("SCM_OPERATION_WORKER")]
    OperationWorker(#[from] OperationWorkerStartError),
    #[error("SCM_BACKUP_WORKER")]
    BackupWorker(#[from] BackupWorkerStartError),
    #[error("SCM_PACKAGE_CERTIFICATION")]
    PackageCertification(#[from] PackageCertificationBootstrapError),
    #[error("SCM_SCHEDULER")]
    Scheduler(#[from] ServiceSchedulerStartError),
    #[error("SCM_RETENTION_RUNTIME")]
    RetentionRuntime(#[from] RetentionRuntimeStartError),
    #[error("SCM_RECOVERY_EXECUTION_RUNTIME")]
    RecoveryExecutionRuntime(#[from] RecoveryExecutionRuntimeStartError),
    #[error("SCM_NOTIFICATION_RUNTIME")]
    NotificationRuntime(#[from] NotificationRuntimeStartError),
    #[error("SCM_REPLICA_REPAIR_RUNTIME")]
    ReplicaRepairRuntime(#[from] ReplicaRepairRuntimeStartError),
    #[error("SCM_STARTUP_RECONCILIATION")]
    StartupReconciliation(#[from] ReconcileError),
}

impl ScmError {
    fn code(&self) -> &'static str {
        match self {
            Self::WindowsService(_) => "SCM_WINDOWS_SERVICE",
            Self::Host(error) => error.code(),
            Self::IdentityConfig(error) => error.code(),
            Self::IpcProtocol(error) => error.code(),
            Self::PipeSecurity(error) => error.code(),
            Self::PipeTransport(error) => error.code(),
            Self::ControlChannel => "SCM_CONTROL_CHANNEL",
            Self::ServiceArguments(error) => error.code(),
            Self::CutoverReadiness(error) => error.code(),
            Self::SecretStore(error) => error.code(),
            Self::OperationWorker(_) => "SCM_OPERATION_WORKER",
            Self::BackupWorker(_) => "SCM_BACKUP_WORKER",
            Self::PackageCertification(error) => error.code(),
            Self::Scheduler(_) => "SCM_SCHEDULER",
            Self::RetentionRuntime(_) => "SCM_RETENTION_RUNTIME",
            Self::RecoveryExecutionRuntime(_) => "SCM_RECOVERY_EXECUTION_RUNTIME",
            Self::NotificationRuntime(_) => "SCM_NOTIFICATION_RUNTIME",
            Self::ReplicaRepairRuntime(_) => "SCM_REPLICA_REPAIR_RUNTIME",
            Self::StartupReconciliation(error) => error.code(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use base64::Engine as _;
    use dbbackup_application::ApplicationCommand;
    use dbbackup_catalog::{
        BackupWorkSpec, ClaimRequest, ConnectionProfileSpec, DatabaseTargetSpec, DestinationSpec,
        OccurrenceSpec, OperationalStore, PlanDraftSpec,
    };
    use dbbackup_core::{ArtifactPartRole, ConsistencyGrade, Engine, PartSpec};
    use dbbackup_domain::{
        BackupPlanId, BackupRunId, ConnectionProfileId, DatabaseTargetId, DestinationId,
        DestinationRequirement, IpcRequestId, OccurrenceIdentity, OccurrenceOrigin, PlanRevisionId,
        RecoveryPointId, WorkItemId,
    };
    use dbbackup_pipeline::{
        EncodingEvidenceV2, ManifestPartV2, NativeToolIdentityV2, ReaderStatusV2, RecoveryOriginV2,
        ReplicaCommitmentV2, ReplicaRequirementV2, SetManifestV2, SignedSetManifestV2,
    };
    use ed25519_dalek::SigningKey;
    use serde_json::json;
    use tempfile::TempDir;
    use uuid::Uuid;

    use crate::ipc::{
        current_process_identity, decode_response, encode_request, pipe_request, RequestEnvelope,
        WindowsSid,
    };
    use crate::secrets::AclPolicy;

    use super::*;

    #[test]
    fn scm_claim_runtime_plan_attaches_all_program_owned_workers_only_to_operational_hosts() {
        let expected = [
            ProgramOwnedClaimRuntime::Recovery,
            ProgramOwnedClaimRuntime::Restore,
            ProgramOwnedClaimRuntime::Verification,
            ProgramOwnedClaimRuntime::Scheduler,
            ProgramOwnedClaimRuntime::NotificationDelivery,
            ProgramOwnedClaimRuntime::ReplicaRepair,
            ProgramOwnedClaimRuntime::Retention,
        ];
        assert_eq!(
            program_owned_claim_runtimes(HostPhase::RehearsalReady),
            expected.as_slice()
        );
        assert_eq!(
            program_owned_claim_runtimes(HostPhase::ProductionReady),
            expected.as_slice()
        );
        for phase in [
            HostPhase::Starting,
            HostPhase::DiagnosticsOnly,
            HostPhase::Draining,
            HostPhase::Stopped,
        ] {
            assert!(program_owned_claim_runtimes(phase).is_empty());
        }
    }

    #[test]
    fn service_process_arguments_accept_one_absolute_data_root_with_spaces_and_reject_ambiguity() {
        let temp = TempDir::new().expect("temp directory");
        let data_root = temp.path().join("custom data root");
        let parsed = parse_service_data_root([
            OsString::from("--data-root"),
            data_root.as_os_str().to_owned(),
        ])
        .expect("absolute custom data root");
        assert_eq!(parsed.as_deref(), Some(data_root.as_path()));

        assert!(parse_service_data_root([
            OsString::from("--data-root"),
            OsString::from("relative-data"),
        ])
        .is_err());
        assert!(parse_service_data_root([
            OsString::from("--data-root"),
            data_root.as_os_str().to_owned(),
            OsString::from("--unexpected"),
        ])
        .is_err());
    }

    #[test]
    fn service_runtime_roots_keep_immutable_install_files_separate_from_mutable_data() {
        let temp = TempDir::new().expect("temp directory");
        let install_root = temp.path().join("install root");
        let executable = install_root.join("dbbackup-service.exe");
        let data_root = temp.path().join("custom data root");
        let roots = ServiceRuntimeRoots::from_executable(&executable, data_root.clone())
            .expect("separate service roots");

        assert_eq!(roots.install_root(), install_root.as_path());
        assert_eq!(roots.data_root(), data_root.as_path());
        assert_eq!(roots.tool_root(), install_root.join("tools"));
        assert_eq!(roots.secret_root(), data_root.join("secrets"));
        assert_eq!(roots.spool_root(), data_root.join("spool"));
        assert_eq!(roots.update_root(), data_root.join("updates"));
    }

    #[test]
    fn scm_restart_keeps_incomplete_commit_for_attention_then_finalizes_exact_complete_intent() {
        let temp = TempDir::new().expect("temp directory");
        let install_root = temp.path().join("install root");
        let data_root = temp.path().join("custom data root");
        let vault_root = temp.path().join("vault");
        let replica_vault_root = temp.path().join("replica vault");
        fs::create_dir_all(&install_root).expect("install root");
        fs::create_dir_all(&data_root).expect("data root");
        fs::create_dir_all(&vault_root).expect("vault root");
        fs::create_dir_all(&replica_vault_root).expect("replica vault root");
        let database_path = data_root.join("operational-v2.db");
        let identity_path = data_root.join("service-identity.json");
        let store = OperationalStore::open(database_path.clone()).expect("open store");
        let connection_id = ConnectionProfileId::new();
        let target_id = DatabaseTargetId::new();
        let destination_id = DestinationId::new();
        let replica_destination_id = DestinationId::new();
        let plan_id = BackupPlanId::new();
        let revision_id = PlanRevisionId::new();
        let configuration = store.configuration();
        configuration
            .insert_connection(ConnectionProfileSpec::tcp(
                connection_id.clone(),
                "primary-pg",
                "postgresql",
                "db.internal",
                5432,
                100,
            ))
            .expect("insert connection");
        configuration
            .insert_target(DatabaseTargetSpec::new(
                target_id.clone(),
                connection_id,
                "oid:100",
                "orders",
                "orders",
                "source-fingerprint",
                110,
            ))
            .expect("insert target");
        configuration
            .insert_destination(DestinationSpec::local(
                destination_id.clone(),
                "local-vault",
                json!({ "path": vault_root }).to_string(),
                120,
            ))
            .expect("insert destination");
        configuration
            .insert_destination(DestinationSpec::local(
                replica_destination_id.clone(),
                "local-replica-vault",
                json!({ "path": replica_vault_root }).to_string(),
                121,
            ))
            .expect("insert replica destination");
        configuration
            .insert_plan_draft(PlanDraftSpec::manual(
                plan_id,
                revision_id.clone(),
                "nightly",
                1,
                "a".repeat(64),
                130,
            ))
            .expect("insert plan");
        configuration
            .add_revision_target(&revision_id, &target_id, 131)
            .expect("add target");
        configuration
            .add_revision_destination(
                &revision_id,
                &destination_id,
                DestinationRequirement::Required,
                132,
            )
            .expect("add destination");
        configuration
            .add_revision_destination(
                &revision_id,
                &replica_destination_id,
                DestinationRequirement::Required,
                133,
            )
            .expect("add replica destination");
        configuration
            .publish_revision(&revision_id, 140)
            .expect("publish revision");

        let occurrence_id =
            OccurrenceIdentity::scheduled(revision_id.clone(), OccurrenceOrigin::Scheduled, 1_000)
                .expect("occurrence identity")
                .deterministic_id(Uuid::NAMESPACE_URL);
        let backup_run_id = BackupRunId::new();
        let work_item_id = WorkItemId::new();
        let runtime = store.runtime();
        runtime
            .upsert_occurrence(OccurrenceSpec::scheduled(
                occurrence_id.clone(),
                revision_id.clone(),
                OccurrenceOrigin::Scheduled,
                1_000,
                900,
            ))
            .expect("insert occurrence");
        runtime
            .enqueue_backup_work(BackupWorkSpec::new(
                backup_run_id.clone(),
                work_item_id.clone(),
                occurrence_id.clone(),
                revision_id,
                target_id.clone(),
                "postgresql",
                "source-fingerprint",
                950,
            ))
            .expect("enqueue backup");
        let claim = runtime
            .claim_next(ClaimRequest::new("worker-old", "boot-old", 1_000, 100))
            .expect("claim backup")
            .expect("due backup");
        runtime.start_claim(&claim, 1_001).expect("start backup");
        runtime
            .mark_in_process_claim_running(&claim, 1_002)
            .expect("mark backup running");
        let recovery_point_id = RecoveryPointId::new();
        runtime
            .begin_backup_claim_commit(&claim, &recovery_point_id, 1_010)
            .expect("record exact recovery intent");

        let signing_key = SigningKey::from_bytes(&[42_u8; 32]);
        let prefix = format!("v2/{}/{}/{}", occurrence_id, target_id, recovery_point_id);
        let part_id = Uuid::new_v4();
        let payload = b"verified encrypted fixture";
        let payload_key = format!("{prefix}/parts/{part_id}.enc");
        let payload_path = vault_root.join(&payload_key);
        let replica_payload_path = replica_vault_root.join(&payload_key);
        fs::create_dir_all(payload_path.parent().expect("payload parent"))
            .expect("payload parent directory");
        fs::create_dir_all(
            replica_payload_path
                .parent()
                .expect("replica payload parent"),
        )
        .expect("replica payload parent directory");
        fs::write(&payload_path, payload).expect("payload object");
        fs::write(&replica_payload_path, payload).expect("replica payload object");
        let manifest_key = format!("{prefix}/set-manifest.json");
        let signed = SignedSetManifestV2::create(
            SetManifestV2::new(
                RecoveryOriginV2::new(
                    recovery_point_id.as_uuid(),
                    "instance-a",
                    occurrence_id.as_uuid(),
                    backup_run_id.as_uuid(),
                    target_id.as_uuid(),
                    "source-fingerprint",
                    "orders",
                )
                .expect("recovery origin"),
                Engine::Postgresql,
                ConsistencyGrade::ApplicationConsistent,
                1_009,
                "fixture-signing-key",
                NativeToolIdentityV2::new("pg_dump", "17.4", "b".repeat(64))
                    .expect("tool identity"),
                BTreeMap::new(),
                vec![ManifestPartV2::encoded(
                    PartSpec::new(
                        part_id,
                        ArtifactPartRole::DatabaseDump,
                        "orders.dump",
                        true,
                        "custom",
                    )
                    .expect("part spec"),
                    EncodingEvidenceV2::new(
                        "c".repeat(64),
                        blake3::hash(payload).to_hex().to_string(),
                        128,
                        payload.len() as u64,
                        "zstd",
                        "chacha20poly1305_stream",
                        1,
                    )
                    .expect("encoding evidence"),
                    payload_key,
                )
                .expect("manifest part")],
                vec![
                    ReplicaCommitmentV2::new(
                        format!("local:{destination_id}"),
                        "local",
                        ReplicaRequirementV2::Required,
                        ReaderStatusV2::Eligible,
                        "none",
                        manifest_key.clone(),
                    )
                    .expect("primary replica commitment"),
                    ReplicaCommitmentV2::new(
                        format!("local:{replica_destination_id}"),
                        "local",
                        ReplicaRequirementV2::Required,
                        ReaderStatusV2::Eligible,
                        "none",
                        manifest_key.clone(),
                    )
                    .expect("secondary replica commitment"),
                ],
            )
            .expect("set manifest"),
            &signing_key,
        )
        .expect("signed manifest");
        let manifest_path = vault_root.join(&manifest_key);
        fs::create_dir_all(manifest_path.parent().expect("manifest parent"))
            .expect("manifest parent directory");
        fs::write(&manifest_path, signed.to_json().expect("manifest JSON"))
            .expect("manifest object");
        let replica_manifest_path = replica_vault_root.join(&manifest_key);
        fs::create_dir_all(
            replica_manifest_path
                .parent()
                .expect("replica manifest parent"),
        )
        .expect("replica manifest parent directory");
        let mut divergent_manifest = signed.manifest.clone();
        divergent_manifest.source_display_name = "orders-divergent".to_owned();
        let divergent_signed = SignedSetManifestV2::create(divergent_manifest, &signing_key)
            .expect("divergent but valid signed manifest");
        fs::write(
            &replica_manifest_path,
            divergent_signed
                .to_json()
                .expect("divergent replica manifest JSON"),
        )
        .expect("divergent replica manifest object");
        drop(store);

        let process = current_process_identity().expect("current process identity");
        let service_sid = process.user_sid().clone();
        let owner_sid = if service_sid.as_str() == "S-1-5-18" {
            WindowsSid::parse("S-1-5-19").expect("LOCAL SERVICE SID")
        } else {
            WindowsSid::parse("S-1-5-18").expect("SYSTEM SID")
        };
        fs::write(
            &identity_path,
            json!({
                "version": 1,
                "ownerSid": owner_sid.as_str(),
                "approvedAdministratorSids": [],
                "trustedSigningKeys": [{
                    "keyId": "fixture-signing-key",
                    "ed25519PublicKeyBase64": base64::engine::general_purpose::STANDARD
                        .encode(signing_key.verifying_key().as_bytes()),
                }],
            })
            .to_string(),
        )
        .expect("identity config");
        AclPolicy::new(service_sid, owner_sid)
            .expect("identity ACL policy")
            .apply_file(&identity_path)
            .expect("identity ACL");

        let roots = ServiceRuntimeRoots::from_executable(
            &install_root.join("dbbackup-service.exe"),
            data_root.clone(),
        )
        .expect("service roots");
        let divergent_host = start_test_host_from_runtime_roots(
            format!("scm-recovery-divergent-{}", Uuid::new_v4()),
            database_path.clone(),
            identity_path.clone(),
            format!(
                r"\\.\pipe\DbBackup.scm.recovery.divergent.{}",
                Uuid::new_v4()
            ),
            roots.clone(),
        )
        .expect("restart service host with divergent required manifest");
        let divergent_probe = dbbackup_catalog::rusqlite::Connection::open(&database_path)
            .expect("open divergent recovery probe");
        let divergent_state: (String, String, String, Option<String>) = divergent_probe
            .query_row(
                "SELECT work.state, attempt.state, backup.state, backup.recovery_point_id
                 FROM work_items AS work
                 JOIN work_attempts AS attempt ON attempt.work_item_id = work.id
                 JOIN backup_runs AS backup ON backup.id = work.backup_run_id
                 WHERE work.id = ?1",
                [work_item_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("divergent recovery state");
        assert_eq!(
            divergent_state,
            (
                "committing".to_owned(),
                "committing".to_owned(),
                "committing".to_owned(),
                Some(recovery_point_id.to_string()),
            ),
            "different valid signed bytes across required replicas must fail closed"
        );
        drop(divergent_probe);
        divergent_host
            .shutdown(ShutdownReason::Stop)
            .expect("shutdown divergent recovery host");

        fs::write(
            &replica_manifest_path,
            signed.to_json().expect("matching replica manifest JSON"),
        )
        .expect("restore matching replica manifest object");
        fs::write(&replica_payload_path, b"corrupted payload")
            .expect("corrupt one required replica payload");
        let incomplete_host = start_test_host_from_runtime_roots(
            format!("scm-recovery-incomplete-{}", Uuid::new_v4()),
            database_path.clone(),
            identity_path.clone(),
            format!(
                r"\\.\pipe\DbBackup.scm.recovery.incomplete.{}",
                Uuid::new_v4()
            ),
            roots.clone(),
        )
        .expect("restart service host with incomplete payload");

        let probe = dbbackup_catalog::rusqlite::Connection::open(&database_path)
            .expect("open recovery probe");
        let incomplete_state: (String, String, String, Option<String>) = probe
            .query_row(
                "SELECT work.state, attempt.state, backup.state, backup.recovery_point_id
                 FROM work_items AS work
                 JOIN work_attempts AS attempt ON attempt.work_item_id = work.id
                 JOIN backup_runs AS backup ON backup.id = work.backup_run_id
                 WHERE work.id = ?1",
                [work_item_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("incomplete recovery state");
        assert_eq!(
            incomplete_state,
            (
                "committing".to_owned(),
                "committing".to_owned(),
                "committing".to_owned(),
                Some(recovery_point_id.to_string()),
            ),
            "a required payload mismatch must remain visible for operator attention"
        );
        drop(probe);
        incomplete_host
            .shutdown(ShutdownReason::Stop)
            .expect("shutdown incomplete recovery host");

        fs::write(&replica_payload_path, payload)
            .expect("restore complete required replica payload");
        let host = start_test_host_from_runtime_roots(
            format!("scm-recovery-complete-{}", Uuid::new_v4()),
            database_path.clone(),
            identity_path,
            format!(
                r"\\.\pipe\DbBackup.scm.recovery.complete.{}",
                Uuid::new_v4()
            ),
            roots,
        )
        .expect("restart service host with complete payload");
        let probe = dbbackup_catalog::rusqlite::Connection::open(&database_path)
            .expect("open completed recovery probe");
        let state: (String, String, String, Option<String>) = probe
            .query_row(
                "SELECT work.state, attempt.state, backup.state, backup.recovery_point_id
                 FROM work_items AS work
                 JOIN work_attempts AS attempt ON attempt.work_item_id = work.id
                 JOIN backup_runs AS backup ON backup.id = work.backup_run_id
                 WHERE work.id = ?1",
                [work_item_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("completed recovery state");
        assert_eq!(
            state,
            (
                "succeeded".to_owned(),
                "succeeded".to_owned(),
                "succeeded".to_owned(),
                Some(recovery_point_id.to_string()),
            )
        );
        assert!(data_root.join("secrets").is_dir());
        assert!(data_root.join("spool").is_dir());
        assert!(!install_root.join("secrets").exists());
        assert!(!install_root.join("spool").exists());
        host.shutdown(ShutdownReason::Stop)
            .expect("shutdown recovered host");
    }

    #[test]
    fn scm_bootstrap_loads_protected_identity_and_publishes_pipe_before_returning() {
        let temp = TempDir::new().expect("temp directory");
        let identity = current_process_identity().expect("current identity");
        let service_sid = identity.user_sid().clone();
        let owner_sid = if service_sid.as_str() == "S-1-5-18" {
            WindowsSid::parse("S-1-5-19").expect("LOCAL SERVICE SID")
        } else {
            WindowsSid::parse("S-1-5-18").expect("SYSTEM SID")
        };
        let owner_sid_text = owner_sid.as_str().to_owned();
        let identity_path = temp.path().join("service-identity.json");
        fs::create_dir(temp.path().join("cutover-evidence")).expect("cutover evidence directory");
        fs::write(
            &identity_path,
            format!(
                concat!(
                    "{{\"version\":1,\"ownerSid\":\"{}\",",
                    "\"approvedAdministratorSids\":[],",
                    "\"cutoverVerifier\":{{",
                    "\"evidenceDirectory\":\"cutover-evidence\",",
                    "\"sourceRevision\":\"1111111111111111111111111111111111111111\",",
                    "\"sourceTreeDigest\":\"2222222222222222222222222222222222222222222222222222222222222222\",",
                    "\"buildId\":\"3333333333333333333333333333333333333333333333333333333333333333\"",
                    "}}}}"
                ),
                owner_sid_text
            ),
        )
        .expect("identity config");
        AclPolicy::new(service_sid, owner_sid)
            .expect("identity ACL policy")
            .apply_file(&identity_path)
            .expect("identity ACL");

        let pipe_name = format!(r"\\.\pipe\DbBackup.scm.{}", Uuid::new_v4());
        let host = start_host_from_paths(
            format!("scm-bootstrap-{}", Uuid::new_v4()),
            temp.path().join("operational.db"),
            identity_path,
            pipe_name.clone(),
        )
        .expect("assembled service host");
        assert!(
            host.health().authority_snapshot().g1_proven(),
            "declared verifier enables only the per-request G1 cutover gate"
        );
        assert_eq!(host.health().phase(), HostPhase::RehearsalReady);
        assert!(!host.health().claims_enabled());
        let issued_at_ms = now_ms();
        let frame = encode_request(
            &RequestEnvelope::new(
                IpcRequestId::new(),
                ApplicationCommand::ServiceHealth,
                None,
                issued_at_ms,
                issued_at_ms + 30_000,
                json!({}),
            )
            .expect("request"),
        )
        .expect("frame");
        let runtime = tokio::runtime::Runtime::new().expect("client runtime");
        let response = runtime
            .block_on(pipe_request(&pipe_name, &frame, Duration::from_secs(1)))
            .expect("live production pipe");
        assert!(decode_response(&response).expect("response").is_success());
        host.shutdown(ShutdownReason::Stop)
            .expect("shutdown assembled host");
    }

    #[test]
    fn declared_cutover_verifier_fails_closed_and_releases_the_host_on_invalid_evidence_root() {
        let temp = TempDir::new().expect("temp directory");
        let identity = current_process_identity().expect("current identity");
        let service_sid = identity.user_sid().clone();
        let owner_sid = if service_sid.as_str() == "S-1-5-18" {
            WindowsSid::parse("S-1-5-19").expect("LOCAL SERVICE SID")
        } else {
            WindowsSid::parse("S-1-5-18").expect("SYSTEM SID")
        };
        let owner_sid_text = owner_sid.as_str().to_owned();
        let identity_path = temp.path().join("service-identity.json");
        fs::write(
            &identity_path,
            format!(
                concat!(
                    "{{\"version\":1,\"ownerSid\":\"{}\",",
                    "\"approvedAdministratorSids\":[],",
                    "\"cutoverVerifier\":{{",
                    "\"evidenceDirectory\":\"missing-cutover-evidence\",",
                    "\"sourceRevision\":\"1111111111111111111111111111111111111111\",",
                    "\"sourceTreeDigest\":\"2222222222222222222222222222222222222222222222222222222222222222\",",
                    "\"buildId\":\"3333333333333333333333333333333333333333333333333333333333333333\"",
                    "}}}}"
                ),
                owner_sid.as_str()
            ),
        )
        .expect("identity config");
        let acl = AclPolicy::new(service_sid, owner_sid).expect("identity ACL policy");
        acl.apply_file(&identity_path).expect("identity ACL");

        let instance_key = format!("scm-cutover-verifier-{}", Uuid::new_v4());
        let database_path = temp.path().join("operational.db");
        let error = start_host_from_paths(
            instance_key.clone(),
            database_path.clone(),
            identity_path.clone(),
            format!(r"\\.\pipe\DbBackup.scm.invalid.{}", Uuid::new_v4()),
        )
        .expect_err("declared but invalid verifier must fail service startup");
        assert_eq!(error.code(), "CONFIG_CUTOVER_READINESS_INVALID");

        fs::write(
            &identity_path,
            format!(
                "{{\"version\":1,\"ownerSid\":\"{}\",\"approvedAdministratorSids\":[]}}",
                owner_sid_text
            ),
        )
        .expect("rewrite identity without verifier");
        acl.apply_file(&identity_path)
            .expect("restore identity ACL");
        let host = start_host_from_paths(
            instance_key,
            database_path,
            identity_path,
            format!(r"\\.\pipe\DbBackup.scm.recovered.{}", Uuid::new_v4()),
        )
        .expect("failed verifier startup released singleton host");
        host.shutdown(ShutdownReason::Stop)
            .expect("shutdown recovered host");
    }

    fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_millis() as i64
    }
}
