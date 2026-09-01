use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use dbbackup_application::{AuthoritySnapshot, CutoverMode, ExecutionAuthority};
use dbbackup_catalog::{
    ConnectionDiagnostics, OperationalStore, PersistedAuthoritySnapshot, PersistedCutoverMode,
    PersistedCutoverRecoveryDisposition, PersistedExecutionAuthority,
    StandaloneAuthorityBootstrapSpec,
};
use thiserror::Error;
use uuid::Uuid;

use super::singleton::SingletonAuthority;
use crate::update::ReaderVersion;

const EXPECTED_SCHEMA_VERSION: i64 = 4;
const EXPECTED_APPLICATION_TABLES: i64 = 67;

#[derive(Clone, Debug)]
pub struct HostConfig {
    instance_key: String,
    database_path: PathBuf,
    current_build_g1_proven: bool,
    current_binary_version: String,
    update_pending: bool,
    pristine_standalone_bootstrap: Option<StandaloneAuthorityBootstrapSpec>,
}

impl HostConfig {
    pub fn new(instance_key: impl Into<String>, database_path: PathBuf) -> Result<Self, HostError> {
        let instance_key = instance_key.into();
        if instance_key.trim().is_empty()
            || instance_key.len() > 256
            || database_path.as_os_str().is_empty()
        {
            return Err(HostError::InvalidConfig);
        }
        Ok(Self {
            instance_key,
            database_path,
            current_build_g1_proven: false,
            current_binary_version: env!("CARGO_PKG_VERSION").to_owned(),
            update_pending: false,
            pristine_standalone_bootstrap: None,
        })
    }

    pub const fn with_current_build_g1_proven(mut self, proven: bool) -> Self {
        self.current_build_g1_proven = proven;
        self
    }

    pub fn database_path(&self) -> &std::path::Path {
        &self.database_path
    }

    pub const fn current_build_g1_proven(&self) -> bool {
        self.current_build_g1_proven
    }

    pub fn with_current_binary_version(mut self, version: impl Into<String>) -> Self {
        self.current_binary_version = version.into();
        self
    }

    pub fn current_binary_version(&self) -> &str {
        &self.current_binary_version
    }

    pub const fn with_update_pending(mut self, pending: bool) -> Self {
        self.update_pending = pending;
        self
    }

    pub fn with_pristine_standalone_bootstrap(
        mut self,
        bootstrap: StandaloneAuthorityBootstrapSpec,
    ) -> Self {
        self.pristine_standalone_bootstrap = Some(bootstrap);
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostPhase {
    Starting,
    RehearsalReady,
    ProductionReady,
    DiagnosticsOnly,
    Draining,
    Stopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownReason {
    Stop,
    Preshutdown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceHealth {
    boot_id: Uuid,
    phase: HostPhase,
    schema_version: Option<i64>,
    application_table_count: Option<i64>,
    execution_authority: ExecutionAuthority,
    cutover_mode: CutoverMode,
    g1_proven: bool,
    cutover_marker: Option<String>,
    recovery_disposition: Option<PersistedCutoverRecoveryDisposition>,
    current_binary_version: String,
    minimum_reader_version: Option<String>,
    update_pending: bool,
    claims_enabled: bool,
    accepts_ipc: bool,
    diagnostic_code: Option<&'static str>,
}

#[derive(Clone)]
pub struct ServiceHealthProbe {
    inner: Arc<Mutex<ServiceHealth>>,
}

impl ServiceHealthProbe {
    fn new(health: ServiceHealth) -> Self {
        Self {
            inner: Arc::new(Mutex::new(health)),
        }
    }

    pub fn snapshot(&self) -> ServiceHealth {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn update(&self, update: impl FnOnce(&mut ServiceHealth)) -> Result<(), HostError> {
        let mut health = self.inner.lock().map_err(|_| HostError::InternalState)?;
        update(&mut health);
        Ok(())
    }

    pub fn begin_drain(&self) -> Result<(), HostError> {
        self.update(|health| {
            if !matches!(
                health.phase,
                HostPhase::DiagnosticsOnly | HostPhase::Stopped
            ) {
                health.phase = HostPhase::Draining;
                health.claims_enabled = false;
            }
        })
    }
}

impl ServiceHealth {
    pub const fn boot_id(&self) -> Uuid {
        self.boot_id
    }

    pub const fn phase(&self) -> HostPhase {
        self.phase
    }

    pub const fn schema_version(&self) -> Option<i64> {
        self.schema_version
    }

    pub const fn application_table_count(&self) -> Option<i64> {
        self.application_table_count
    }

    pub const fn execution_authority(&self) -> ExecutionAuthority {
        self.execution_authority
    }

    pub const fn cutover_mode(&self) -> CutoverMode {
        self.cutover_mode
    }

    pub const fn authority_snapshot(&self) -> AuthoritySnapshot {
        AuthoritySnapshot::new(
            self.execution_authority,
            self.cutover_mode,
            self.g1_proven,
            self.cutover_marker.is_some(),
        )
    }

    pub fn cutover_marker(&self) -> Option<&str> {
        self.cutover_marker.as_deref()
    }

    pub const fn recovery_disposition(&self) -> Option<PersistedCutoverRecoveryDisposition> {
        self.recovery_disposition
    }

    pub fn current_binary_version(&self) -> &str {
        &self.current_binary_version
    }

    pub fn minimum_reader_version(&self) -> Option<&str> {
        self.minimum_reader_version.as_deref()
    }

    pub const fn update_pending(&self) -> bool {
        self.update_pending
    }

    pub const fn claims_enabled(&self) -> bool {
        self.claims_enabled
    }

    pub const fn accepts_ipc(&self) -> bool {
        self.accepts_ipc
    }

    pub const fn diagnostic_code(&self) -> Option<&'static str> {
        self.diagnostic_code
    }
}

pub trait HostRuntime: Send + Sync + 'static {
    fn started(&self, diagnostics_only: bool) -> Result<(), HostError>;
    fn stop_accepting_ipc(&self) -> Result<(), HostError>;
    fn stop_claiming_work(&self) -> Result<(), HostError>;
    fn drain_clients(&self) -> Result<(), HostError>;
    fn checkpointed(&self) -> Result<(), HostError>;
    fn stopped(&self, reason: ShutdownReason);
}

pub trait ClaimRuntime: Send + Sync + 'static {
    fn request_stop(&self);
    fn wait_stopped(&self) -> Result<(), HostError>;
}

#[derive(Debug, Error)]
pub enum HostError {
    #[error("SERVICE_CONFIG_INVALID")]
    InvalidConfig,
    #[error("SERVICE_SINGLETON_ALREADY_RUNNING")]
    SingletonAlreadyRunning,
    #[error("SERVICE_OS_API_FAILED")]
    OsApiFailed(&'static str),
    #[error("SERVICE_RUNTIME_FAILED")]
    RuntimeFailed,
    #[error("SERVICE_OPERATIONAL_STORE")]
    OperationalStore(&'static str),
    #[error("SERVICE_INTERNAL_STATE")]
    InternalState,
}

impl HostError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidConfig => "SERVICE_CONFIG_INVALID",
            Self::SingletonAlreadyRunning => "SERVICE_SINGLETON_ALREADY_RUNNING",
            Self::OsApiFailed(_) => "SERVICE_OS_API_FAILED",
            Self::RuntimeFailed => "SERVICE_RUNTIME_FAILED",
            Self::OperationalStore(code) => code,
            Self::InternalState => "SERVICE_INTERNAL_STATE",
        }
    }
}

pub struct ServiceHost<R: HostRuntime> {
    _authority_guard: SingletonAuthority,
    runtime: Arc<R>,
    store: Option<OperationalStore>,
    health: ServiceHealthProbe,
    shutdown_complete: Mutex<bool>,
}

impl<R: HostRuntime> fmt::Debug for ServiceHost<R> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServiceHost")
            .field("health", &self.health())
            .finish_non_exhaustive()
    }
}

impl<R: HostRuntime> ServiceHost<R> {
    pub fn start(config: HostConfig, runtime: Arc<R>) -> Result<Self, HostError> {
        let database_existed_at_start = config
            .database_path
            .try_exists()
            .map_err(|_| HostError::OsApiFailed("database path probe"))?;
        let current_reader = ReaderVersion::parse(&config.current_binary_version)
            .map_err(|_| HostError::InvalidConfig)?;
        let authority_guard = SingletonAuthority::acquire(&config.instance_key)?;
        let boot_id = Uuid::new_v4();
        let mut store = None;
        let safe_authority = rehearsal_authority(config.current_build_g1_proven);
        let current_binary_version = config.current_binary_version.clone();

        let health = match OperationalStore::open(config.database_path) {
            Ok(opened) => {
                if !database_existed_at_start {
                    if let Some(bootstrap) = config.pristine_standalone_bootstrap.clone() {
                        opened
                            .cutover()
                            .bootstrap_standalone_authority(bootstrap)
                            .map_err(|error| HostError::OperationalStore(error.code()))?;
                    }
                }
                match opened.writer_diagnostics() {
                    Ok(diagnostics) => match opened.cutover().authority() {
                        Ok(persisted) => {
                            let authority = authority_from_persisted(
                                &persisted,
                                config.current_build_g1_proven,
                            );
                            let minimum_reader = opened
                                .cutover()
                                .minimum_reader_version()
                                .map_err(|error| error.code());
                            let mut health = match minimum_reader {
                                Ok(minimum_reader) => health_from_diagnostics(
                                    boot_id,
                                    authority,
                                    persisted.cutover_marker().map(str::to_owned),
                                    persisted.recovery_disposition(),
                                    current_binary_version.clone(),
                                    minimum_reader
                                        .as_ref()
                                        .map(|minimum| minimum.minimum_reader_version().to_owned()),
                                    &diagnostics,
                                ),
                                Err(code) => diagnostics_health(
                                    boot_id,
                                    safe_authority,
                                    current_binary_version.clone(),
                                    None,
                                    code,
                                ),
                            };
                            if let Some(minimum) = health.minimum_reader_version.as_deref() {
                                let compatible = ReaderVersion::parse(minimum)
                                    .and_then(|minimum| current_reader.ensure_at_least(minimum));
                                if compatible.is_err() {
                                    health.phase = HostPhase::DiagnosticsOnly;
                                    health.claims_enabled = false;
                                    health.diagnostic_code =
                                        Some("SERVICE_MINIMUM_READER_VERSION_UNMET");
                                }
                            }
                            if config.update_pending && health.phase != HostPhase::DiagnosticsOnly {
                                health.phase = HostPhase::Draining;
                                health.claims_enabled = false;
                                health.update_pending = true;
                                health.diagnostic_code = Some("SERVICE_UPDATE_PENDING_COMMIT");
                            }
                            store = Some(opened);
                            health
                        }
                        Err(error) => diagnostics_health(
                            boot_id,
                            safe_authority,
                            current_binary_version.clone(),
                            None,
                            error.code(),
                        ),
                    },
                    Err(error) => diagnostics_health(
                        boot_id,
                        safe_authority,
                        current_binary_version.clone(),
                        None,
                        error.code(),
                    ),
                }
            }
            Err(error) => diagnostics_health(
                boot_id,
                safe_authority,
                current_binary_version,
                None,
                error.code(),
            ),
        };
        runtime.started(health.phase == HostPhase::DiagnosticsOnly)?;

        Ok(Self {
            _authority_guard: authority_guard,
            runtime,
            store,
            health: ServiceHealthProbe::new(health),
            shutdown_complete: Mutex::new(false),
        })
    }

    pub fn health(&self) -> ServiceHealth {
        self.health.snapshot()
    }

    pub fn health_probe(&self) -> ServiceHealthProbe {
        self.health.clone()
    }

    pub fn operational_store(&self) -> Option<OperationalStore> {
        self.store.clone()
    }

    pub fn shutdown(&self, reason: ShutdownReason) -> Result<(), HostError> {
        let mut shutdown_complete = self
            .shutdown_complete
            .lock()
            .map_err(|_| HostError::InternalState)?;
        if *shutdown_complete {
            return Ok(());
        }
        *shutdown_complete = true;

        self.health.update(|health| {
            health.phase = HostPhase::Draining;
            health.claims_enabled = false;
            health.accepts_ipc = false;
        })?;

        let mut first_error = None;
        record_first(&mut first_error, self.runtime.stop_accepting_ipc());
        record_first(&mut first_error, self.runtime.stop_claiming_work());
        record_first(&mut first_error, self.runtime.drain_clients());
        if let Some(store) = &self.store {
            record_first(
                &mut first_error,
                store
                    .checkpoint_wal()
                    .map_err(|error| HostError::OperationalStore(error.code())),
            );
        }
        record_first(&mut first_error, self.runtime.checkpointed());
        self.runtime.stopped(reason);

        self.health.update(|health| {
            health.phase = HostPhase::Stopped;
        })?;
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

fn authority_from_persisted(
    persisted: &PersistedAuthoritySnapshot,
    current_build_g1_proven: bool,
) -> AuthoritySnapshot {
    match (persisted.execution_authority(), persisted.cutover_mode()) {
        (PersistedExecutionAuthority::Service, PersistedCutoverMode::Production) => {
            AuthoritySnapshot::new(
                ExecutionAuthority::Service,
                CutoverMode::Production,
                persisted.g1_proven(),
                persisted.cutover_marker().is_some(),
            )
        }
        (PersistedExecutionAuthority::LegacyDirect, PersistedCutoverMode::RehearsalOnly) => {
            rehearsal_authority(current_build_g1_proven)
        }
        _ => rehearsal_authority(false),
    }
}

const fn rehearsal_authority(current_build_g1_proven: bool) -> AuthoritySnapshot {
    AuthoritySnapshot::new(
        ExecutionAuthority::LegacyDirect,
        CutoverMode::RehearsalOnly,
        current_build_g1_proven,
        false,
    )
}

fn record_first(first_error: &mut Option<HostError>, result: Result<(), HostError>) {
    if let Err(error) = result {
        if first_error.is_none() {
            *first_error = Some(error);
        }
    }
}

fn health_from_diagnostics(
    boot_id: Uuid,
    authority: AuthoritySnapshot,
    cutover_marker: Option<String>,
    recovery_disposition: Option<PersistedCutoverRecoveryDisposition>,
    current_binary_version: String,
    minimum_reader_version: Option<String>,
    diagnostics: &ConnectionDiagnostics,
) -> ServiceHealth {
    let diagnostic_code = validate_diagnostics(diagnostics).err();
    if let Some(code) = diagnostic_code {
        return ServiceHealth {
            boot_id,
            phase: HostPhase::DiagnosticsOnly,
            schema_version: Some(diagnostics.user_version),
            application_table_count: Some(diagnostics.application_table_count),
            execution_authority: authority.execution_authority(),
            cutover_mode: authority.cutover_mode(),
            g1_proven: authority.g1_proven(),
            cutover_marker,
            recovery_disposition,
            current_binary_version,
            minimum_reader_version,
            update_pending: false,
            claims_enabled: false,
            accepts_ipc: true,
            diagnostic_code: Some(code),
        };
    }

    let claims_enabled = authority.execution_authority() == ExecutionAuthority::Service
        && authority.cutover_mode() == CutoverMode::Production
        && authority.g1_proven()
        && authority.cutover_marker_present();
    ServiceHealth {
        boot_id,
        phase: if claims_enabled {
            HostPhase::ProductionReady
        } else {
            HostPhase::RehearsalReady
        },
        schema_version: Some(diagnostics.user_version),
        application_table_count: Some(diagnostics.application_table_count),
        execution_authority: authority.execution_authority(),
        cutover_mode: authority.cutover_mode(),
        g1_proven: authority.g1_proven(),
        cutover_marker,
        recovery_disposition,
        current_binary_version,
        minimum_reader_version,
        update_pending: false,
        claims_enabled,
        accepts_ipc: true,
        diagnostic_code: None,
    }
}

fn diagnostics_health(
    boot_id: Uuid,
    authority: AuthoritySnapshot,
    current_binary_version: String,
    minimum_reader_version: Option<String>,
    diagnostic_code: &'static str,
) -> ServiceHealth {
    ServiceHealth {
        boot_id,
        phase: HostPhase::DiagnosticsOnly,
        schema_version: None,
        application_table_count: None,
        execution_authority: authority.execution_authority(),
        cutover_mode: authority.cutover_mode(),
        g1_proven: authority.g1_proven(),
        cutover_marker: None,
        recovery_disposition: None,
        current_binary_version,
        minimum_reader_version,
        update_pending: false,
        claims_enabled: false,
        accepts_ipc: true,
        diagnostic_code: Some(diagnostic_code),
    }
}

fn validate_diagnostics(diagnostics: &ConnectionDiagnostics) -> Result<(), &'static str> {
    if diagnostics.user_version != EXPECTED_SCHEMA_VERSION {
        return Err("SERVICE_SCHEMA_VERSION_INVALID");
    }
    if diagnostics.application_table_count != EXPECTED_APPLICATION_TABLES {
        return Err("SERVICE_SCHEMA_TABLE_COUNT_INVALID");
    }
    if !diagnostics.journal_mode.eq_ignore_ascii_case("wal") {
        return Err("SERVICE_JOURNAL_MODE_INVALID");
    }
    if diagnostics.synchronous != 2 {
        return Err("SERVICE_SYNCHRONOUS_MODE_INVALID");
    }
    if diagnostics.foreign_keys != 1 {
        return Err("SERVICE_FOREIGN_KEYS_DISABLED");
    }
    Ok(())
}
