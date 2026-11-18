use std::collections::{BTreeSet, HashMap};
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use restoreplane_catalog::{
    AdmissionRequest, ClaimedWork, ConnectionExecutionBinding, DestructiveStepDisposition,
    OperationalStore, ProcessExitSpec, ProcessStartSpec, ResourcePoolSpec, ResourceRequirement,
    RestoreClaimContext, SpoolRequirement, SpoolVolumeSpec, VerificationClaimContext,
};
use restoreplane_core::Engine;
use restoreplane_runtime::{
    LaunchSpec, ProcessExit, ProcessSupervisor, RestoreAdmission, RestoreExecutionError,
    RestoreExecutionEvidence, RestoreExecutor, RestoreValidationError, RestoreValidationEvidence,
    RestoreWorker, RestoreWorkerConfig, RuntimeClock, SystemRuntimeClock, VerificationAdmission,
    VerificationExecutionError, VerificationExecutionEvidence, VerificationExecutor,
    VerificationWorker, VerificationWorkerConfig,
};
use restoreplane_source_mysql::binary::ClientKind;
use restoreplane_source_mysql::dump::tls_args;
use restoreplane_source_sqlite::{
    probe_sqlite_file, restore_create_new_observed, restore_overwrite_observed,
    validate_database_observed, BackupCancellationToken,
};
use serde::Deserialize;
use serde_json::json;
use thiserror::Error;
use zeroize::Zeroizing;

use crate::host::{ClaimRuntime, HostError, ServiceHealthProbe};
use crate::recovery_point::{
    normalized_sha256, MaterializedRecoveryPoint, RecoveryMaterializationControl,
    RecoveryPointReadError, RecoveryPointResolver, TrustedNativeTool,
};
use crate::secrets::{SecretPurpose, ServiceSecretStore};

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(250);
const NATIVE_POLL_INTERVAL: Duration = Duration::from_millis(100);
const NATIVE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const NATIVE_EXECUTION_TIMEOUT: Duration = Duration::from_secs(12 * 60 * 60);
const LEASE_DURATION_MS: i64 = 30_000;
const RETRY_BACKOFF_MS: i64 = 5_000;
const STOP_WAIT_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_ORACLE_TABLES: usize = 10_000;
const RESTORE_GLOBAL_CONCURRENCY: i64 = 2;
const RESTORE_RESOURCE_CONCURRENCY: i64 = 1;
const RESTORE_SPOOL_RESERVE_FLOOR_BYTES: i64 = 2 * 1_024 * 1_024 * 1_024;
type SqliteClaimControl = (
    BackupCancellationToken,
    Arc<dyn Fn() -> bool + Send + Sync>,
    Arc<Mutex<Option<NativeRunError>>>,
);

#[derive(Debug, Error)]
pub enum RecoveryExecutionRuntimeStartError {
    #[error("RECOVERY_EXECUTION_RUNTIME_INPUT_INVALID")]
    InputInvalid,
    #[error("RECOVERY_EXECUTION_RUNTIME_WORKER_INIT")]
    WorkerInit,
    #[error("RECOVERY_EXECUTION_RUNTIME_ASYNC_INIT")]
    AsyncRuntime(#[source] std::io::Error),
    #[error("RECOVERY_EXECUTION_RUNTIME_THREAD_START")]
    ThreadStart(#[source] std::io::Error),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredDatabaseCredentials {
    username: String,
    password: String,
}

struct ResolvedCredentials {
    username: Zeroizing<String>,
    password: Zeroizing<String>,
}

struct PendingRestore {
    materialized: MaterializedRecoveryPoint,
    profile: ConnectionExecutionBinding,
    target_name: String,
    engine: Engine,
    scope_tables: Option<BTreeSet<(String, String)>>,
    deadline: Instant,
}

struct PreparedRestore {
    verified: crate::recovery_point::VerifiedRecoveryPoint,
    profile: ConnectionExecutionBinding,
    scope_tables: Option<BTreeSet<(String, String)>>,
    deadline: Instant,
    stage_key: String,
}

struct EphemeralSqlScript {
    path: PathBuf,
}

impl EphemeralSqlScript {
    fn create(directory: &Path, contents: &str) -> Result<Self, NativeRunError> {
        let path = directory.join(format!(
            ".restoreplane-restore-{}.psql",
            uuid::Uuid::new_v4()
        ));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|_| NativeRunError::terminal("RESTORE_STAGING_INVALID"))?;
        let script = Self { path };
        let write_result = file
            .write_all(contents.as_bytes())
            .and_then(|()| file.sync_all());
        drop(file);
        write_result.map_err(|_| NativeRunError::terminal("RESTORE_STAGING_INVALID"))?;
        Ok(script)
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for EphemeralSqlScript {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

struct PreparedVerification {
    verified: crate::recovery_point::VerifiedRecoveryPoint,
    deadline: Instant,
    stage_key: String,
}

pub(crate) struct ServiceRestoreExecutor {
    store: OperationalStore,
    secrets: Arc<ServiceSecretStore>,
    resolver: Arc<RecoveryPointResolver>,
    spool_root: PathBuf,
    prepared: Mutex<HashMap<String, PreparedRestore>>,
    pending: Mutex<HashMap<String, PendingRestore>>,
    stopping: Arc<AtomicBool>,
    clock: Arc<dyn RuntimeClock>,
}

impl ServiceRestoreExecutor {
    pub(crate) fn new(
        store: OperationalStore,
        secrets: Arc<ServiceSecretStore>,
        resolver: Arc<RecoveryPointResolver>,
        spool_root: PathBuf,
        stopping: Arc<AtomicBool>,
        clock: Arc<dyn RuntimeClock>,
    ) -> Self {
        Self {
            store,
            secrets,
            resolver,
            spool_root,
            prepared: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            stopping,
            clock,
        }
    }

    fn materialization_control(
        &self,
        claim: &ClaimedWork,
        deadline: Instant,
    ) -> RecoveryMaterializationControl {
        let store = self.store.clone();
        let clock = Arc::clone(&self.clock);
        let claim = claim.clone();
        RecoveryMaterializationControl::new(
            Arc::clone(&self.stopping),
            deadline,
            Arc::new(move || {
                let heartbeat_at_ms = clock.now_ms();
                let lease_expires_at_ms = heartbeat_at_ms
                    .checked_add(LEASE_DURATION_MS)
                    .ok_or_else(|| {
                        RecoveryPointReadError::terminal("RECOVERY_EXECUTION_TIMEOUT")
                    })?;
                if heartbeat_at_ms < 0 {
                    return Err(RecoveryPointReadError::retryable("RECOVERY_CLOCK_INVALID"));
                }
                let runtime = store.runtime();
                if runtime
                    .claim_cancel_requested(&claim, heartbeat_at_ms)
                    .map_err(|_| RecoveryPointReadError::retryable("RECOVERY_LEASE_LOST"))?
                {
                    return Err(RecoveryPointReadError::retryable(
                        "RECOVERY_CANCEL_REQUESTED",
                    ));
                }
                runtime
                    .heartbeat_claim(&claim, heartbeat_at_ms, lease_expires_at_ms)
                    .map_err(|_| RecoveryPointReadError::retryable("RECOVERY_LEASE_LOST"))
            }),
        )
    }

    fn admit_capacity(
        &self,
        claim: &ClaimedWork,
        context: &RestoreClaimContext,
        verified: &crate::recovery_point::VerifiedRecoveryPoint,
        stage_key: &str,
    ) -> Result<RestoreAdmission, RestoreExecutionError> {
        std::fs::create_dir_all(&self.spool_root)
            .map_err(|_| restore_error("RESTORE_SPOOL_UNAVAILABLE", true, None))?;
        let free_bytes = fs2::available_space(&self.spool_root)
            .map_err(|_| restore_error("RESTORE_SPOOL_UNAVAILABLE", true, None))?;
        let free_bytes = i64::try_from(free_bytes).unwrap_or(i64::MAX);
        let reserved_bytes = verified
            .required_staging_bytes()
            .map_err(restore_read_error)?;
        let now_ms = self.clock.now_ms();
        if now_ms < 0 {
            return Err(restore_error("RESTORE_CLOCK_INVALID", true, None));
        }
        let runtime = self.store.runtime();
        let global_pool = runtime
            .upsert_resource_pool(ResourcePoolSpec::new(
                stable_capacity_id("restore", "global"),
                "restore",
                "global",
                RESTORE_GLOBAL_CONCURRENCY,
                now_ms,
            ))
            .map_err(|_| restore_error("RESTORE_ADMISSION_UNAVAILABLE", true, None))?;
        let source_key = verified.destination_fingerprint().to_owned();
        let source_pool = runtime
            .upsert_resource_pool(ResourcePoolSpec::new(
                stable_capacity_id("restore-source", &source_key),
                "destination",
                format!("restore:{source_key}"),
                RESTORE_RESOURCE_CONCURRENCY,
                now_ms,
            ))
            .map_err(|_| restore_error("RESTORE_ADMISSION_UNAVAILABLE", true, None))?;
        let target_key = context.target_fingerprint().to_owned();
        let target_pool = runtime
            .upsert_resource_pool(ResourcePoolSpec::new(
                stable_capacity_id("restore-target", &target_key),
                "database_exclusive",
                target_key,
                RESTORE_RESOURCE_CONCURRENCY,
                now_ms,
            ))
            .map_err(|_| restore_error("RESTORE_ADMISSION_UNAVAILABLE", true, None))?;
        let volume_key = normalized_spool_key(&self.spool_root)?;
        let volume_id = runtime
            .upsert_spool_volume(SpoolVolumeSpec::ready(
                stable_capacity_id("spool", &volume_key),
                volume_key,
                self.spool_root.to_string_lossy(),
                free_bytes.max(1),
                free_bytes.max(1),
                RESTORE_SPOOL_RESERVE_FLOOR_BYTES,
                free_bytes.max(0),
                now_ms,
            ))
            .map_err(|_| restore_error("RESTORE_ADMISSION_UNAVAILABLE", true, None))?;
        let spool_path = verified
            .staging_path(stage_key)
            .map_err(restore_read_error)?;
        let outcome = runtime
            .admit_claim(AdmissionRequest::new(
                claim.clone(),
                vec![
                    ResourceRequirement::new(global_pool, 1),
                    ResourceRequirement::new(source_pool, 1),
                    ResourceRequirement::new(target_pool, 1),
                ],
                Some(SpoolRequirement::new(
                    volume_id,
                    reserved_bytes,
                    spool_path.to_string_lossy(),
                )),
                now_ms,
            ))
            .map_err(|_| restore_error("RESTORE_ADMISSION_UNAVAILABLE", true, None))?;
        if outcome.admitted() {
            Ok(RestoreAdmission::Admitted)
        } else {
            RestoreAdmission::deferred(
                outcome
                    .wait_reason()
                    .unwrap_or("restore_admission_deferred"),
            )
            .map_err(|_| restore_error("RESTORE_ADMISSION_UNAVAILABLE", true, None))
        }
    }

    fn credentials(
        &self,
        profile: &ConnectionExecutionBinding,
    ) -> Result<ResolvedCredentials, &'static str> {
        let secret_ref = profile
            .database_auth_secret_ref_id()
            .ok_or("RESTORE_TARGET_CREDENTIAL_MISSING")?;
        let secret = self
            .secrets
            .resolve(secret_ref, SecretPurpose::DatabaseAuth)
            .map_err(|_| "RESTORE_TARGET_CREDENTIAL_UNAVAILABLE")?;
        let stored: StoredDatabaseCredentials = serde_json::from_slice(secret.expose())
            .map_err(|_| "RESTORE_TARGET_CREDENTIAL_INVALID")?;
        if stored.username.trim().is_empty()
            || stored.username.len() > 1_024
            || stored.password.is_empty()
        {
            return Err("RESTORE_TARGET_CREDENTIAL_INVALID");
        }
        Ok(ResolvedCredentials {
            username: Zeroizing::new(stored.username),
            password: Zeroizing::new(stored.password),
        })
    }

    fn profile(
        &self,
        context: &RestoreClaimContext,
        engine: Engine,
    ) -> Result<ConnectionExecutionBinding, &'static str> {
        let profile = self
            .store
            .configuration()
            .connection_execution_binding(context.target_connection_profile_id())
            .map_err(|_| "RESTORE_TARGET_BINDING_INVALID")?;
        if profile.engine() != engine_name(engine)
            || !matches!(engine, Engine::Sqlite) && profile.transport() != "tcp"
            || matches!(engine, Engine::Sqlite) && profile.transport() != "file"
            || !valid_identifier(context.target_database_name())
        {
            return Err("RESTORE_TARGET_BINDING_INVALID");
        }
        Ok(profile)
    }

    fn execute_sqlite_restore(
        &self,
        claim: &ClaimedWork,
        profile: &ConnectionExecutionBinding,
        context: &RestoreClaimContext,
        materialized: &MaterializedRecoveryPoint,
        deadline: Instant,
    ) -> Result<(), NativeRunError> {
        self.check_claim_control(claim, deadline)?;
        let target = profile
            .file_path()
            .map(Path::new)
            .ok_or_else(|| NativeRunError::terminal("RESTORE_TARGET_BINDING_INVALID"))?;
        if !target.is_absolute() {
            return Err(NativeRunError::terminal("RESTORE_TARGET_BINDING_INVALID"));
        }
        let artifact = materialized.database_part().map_err(read_error)?.path();
        let (cancellation, control, control_error) = self.sqlite_claim_control(claim, deadline);
        match context.conflict_policy() {
            "create_new" => {
                let target_exists = target
                    .try_exists()
                    .map_err(|_| NativeRunError::retryable("RESTORE_TARGET_PROBE_FAILED"))?;
                if target_exists {
                    return Err(NativeRunError::terminal("RESTORE_TARGET_EXISTS"));
                }
                self.arm_destructive_step(claim, None, None, target_exists)?;
                self.check_claim_control(claim, deadline)
                    .map_err(NativeRunError::after_target_created)?;
                let observer_control = Arc::clone(&control);
                restore_create_new_observed(
                    artifact,
                    target,
                    None,
                    &cancellation,
                    move |_| {
                        observer_control();
                    },
                    control,
                )
                .map(|_| ())
                .map_err(|_| sqlite_control_error(&control_error).after_target_created())
            }
            "overwrite" => {
                let bound = probe_sqlite_file(target)
                    .map_err(|_| NativeRunError::terminal("RESTORE_TARGET_CHANGED"))?;
                self.arm_destructive_step(claim, None, None, true)?;
                self.check_claim_control(claim, deadline)
                    .map_err(NativeRunError::after_target_created)?;
                let observer_control = Arc::clone(&control);
                restore_overwrite_observed(
                    artifact,
                    target,
                    &bound,
                    None,
                    &cancellation,
                    move |_| {
                        observer_control();
                    },
                    control,
                )
                .map(|_| ())
                .map_err(|_| sqlite_control_error(&control_error).after_target_created())
            }
            _ => Err(NativeRunError::terminal("RESTORE_CONFLICT_POLICY_INVALID")),
        }
    }

    fn arm_destructive_step(
        &self,
        claim: &ClaimedWork,
        live_server_version: Option<&str>,
        live_server_identity: Option<&str>,
        live_target_exists: bool,
    ) -> Result<(), NativeRunError> {
        let heartbeat_at_ms = self.clock.now_ms();
        let lease_expires_at_ms = heartbeat_at_ms.saturating_add(LEASE_DURATION_MS);
        match self.store.runtime().begin_restore_destructive_step(
            claim,
            heartbeat_at_ms,
            lease_expires_at_ms,
            live_server_version,
            live_server_identity,
            live_target_exists,
        ) {
            Ok(DestructiveStepDisposition::Proceed) => Ok(()),
            Ok(DestructiveStepDisposition::CancelRequested) => {
                Err(NativeRunError::retryable("RECOVERY_CANCEL_REQUESTED"))
            }
            Err(error) if error.code() == "RESTORE_PREFLIGHT_STALE" => {
                Err(NativeRunError::terminal("RESTORE_PREFLIGHT_STALE"))
            }
            Err(_) => Err(NativeRunError::retryable(
                "RESTORE_DESTRUCTIVE_FENCE_UNAVAILABLE",
            )),
        }
    }

    fn check_claim_control(
        &self,
        claim: &ClaimedWork,
        deadline: Instant,
    ) -> Result<(), NativeRunError> {
        check_claim_control_parts(
            &self.store,
            self.clock.as_ref(),
            self.stopping.as_ref(),
            claim,
            deadline,
        )
    }

    fn sqlite_claim_control(&self, claim: &ClaimedWork, deadline: Instant) -> SqliteClaimControl {
        let cancellation = BackupCancellationToken::default();
        let callback_cancellation = cancellation.clone();
        let store = self.store.clone();
        let clock = Arc::clone(&self.clock);
        let stopping = Arc::clone(&self.stopping);
        let claim = claim.clone();
        let error = Arc::new(Mutex::new(None));
        let callback_error = Arc::clone(&error);
        let last_heartbeat = Arc::new(Mutex::new(
            Instant::now()
                .checked_sub(NATIVE_HEARTBEAT_INTERVAL)
                .unwrap_or_else(Instant::now),
        ));
        let callback_heartbeat = Arc::clone(&last_heartbeat);
        let control: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(move || {
            let result = ensure_execution_window(stopping.as_ref(), deadline).and_then(|()| {
                let should_heartbeat = callback_heartbeat
                    .lock()
                    .map(|last| last.elapsed() >= NATIVE_HEARTBEAT_INTERVAL)
                    .unwrap_or(true);
                if !should_heartbeat {
                    return Ok(());
                }
                let result = check_claim_control_parts(
                    &store,
                    clock.as_ref(),
                    stopping.as_ref(),
                    &claim,
                    deadline,
                );
                if result.is_ok() {
                    if let Ok(mut last) = callback_heartbeat.lock() {
                        *last = Instant::now();
                    }
                }
                result
            });
            if let Err(error) = result {
                if let Ok(mut stored) = callback_error.lock() {
                    stored.get_or_insert(error);
                }
                callback_cancellation.cancel();
                true
            } else {
                false
            }
        });
        (cancellation, control, error)
    }

    fn execute_postgresql_restore(
        &self,
        claim: &ClaimedWork,
        profile: &ConnectionExecutionBinding,
        credentials: &ResolvedCredentials,
        context: &RestoreClaimContext,
        materialized: &MaterializedRecoveryPoint,
        deadline: Instant,
    ) -> Result<(), NativeRunError> {
        reject_unbound_network_overwrite(context)?;
        let dump = materialized.database_part().map_err(read_error)?;
        if dump.dump_format() != "custom" {
            return Err(NativeRunError::terminal("RESTORE_DUMP_FORMAT_UNSUPPORTED"));
        }
        let psql = self.resolver.trusted_tool("psql").map_err(read_error)?;
        let pg_restore = self
            .resolver
            .trusted_tool("pg_restore")
            .map_err(read_error)?;
        validate_tool_compatibility(
            Engine::Postgresql,
            dump.dump_format(),
            materialized.source_tool_version(),
            pg_restore.version(),
        )?;
        let target = context.target_database_name();
        let target_probe_sql = format!(
            "SELECT version(), (SELECT system_identifier::text FROM pg_control_system()), EXISTS(SELECT 1 FROM pg_database WHERE datname = '{}')",
            sql_literal(target)
        );
        let target_probe = self.run_pg_query(
            claim,
            &psql,
            profile,
            credentials,
            "postgres",
            &target_probe_sql,
            "restore_target_identity",
            deadline,
        )?;
        let (live_server_version, live_server_identity, exists) =
            parse_pg_target_probe(&target_probe)?;
        if context.target_server_identity() != Some(live_server_identity.as_str()) {
            return Err(NativeRunError::terminal("RESTORE_TARGET_CHANGED"));
        }
        if !matches!(context.conflict_policy(), "create_new" | "overwrite") {
            return Err(NativeRunError::terminal("RESTORE_CONFLICT_POLICY_INVALID"));
        }
        match context.conflict_policy() {
            "create_new" if exists => {
                return Err(NativeRunError::terminal("RESTORE_TARGET_EXISTS"))
            }
            "overwrite" if !exists => {
                return Err(NativeRunError::terminal("RESTORE_TARGET_MISSING"));
            }
            "create_new" => {}
            "overwrite" => {
                self.arm_destructive_step(
                    claim,
                    Some(&live_server_version),
                    Some(&live_server_identity),
                    exists,
                )?;
                let sql = format!("DROP DATABASE {} WITH (FORCE)", pg_ident(target));
                self.run_pg_query(
                    claim,
                    &psql,
                    profile,
                    credentials,
                    "postgres",
                    &sql,
                    "restore_target_drop",
                    deadline,
                )
                .map_err(NativeRunError::after_target_created)?;
            }
            _ => unreachable!("conflict policy was validated"),
        }
        if context.conflict_policy() == "create_new" {
            self.arm_destructive_step(
                claim,
                Some(&live_server_version),
                Some(&live_server_identity),
                exists,
            )?;
        }
        let script = guarded_pg_create_database_sql(target, &live_server_identity)?;
        let staging_directory = dump
            .path()
            .parent()
            .ok_or_else(|| NativeRunError::terminal("RESTORE_STAGING_INVALID"))?;
        self.run_pg_script(
            claim,
            &psql,
            profile,
            credentials,
            "postgres",
            &script,
            staging_directory,
            "restore_target_create",
            deadline,
        )
        .map_err(NativeRunError::after_target_created)?;
        let args = pg_connection_args(profile, credentials, target)
            .map_err(NativeRunError::after_target_created)?
            .into_iter()
            .chain([
                OsString::from("--exit-on-error"),
                OsString::from("--no-owner"),
                OsString::from("--no-privileges"),
                dump.path().as_os_str().to_owned(),
            ])
            .collect::<Vec<_>>();
        let working_directory = staging_directory;
        let environment =
            pg_environment(profile, credentials).map_err(NativeRunError::after_target_created)?;
        let output = self
            .run_native(
                claim,
                &pg_restore,
                working_directory,
                args,
                environment,
                credentials.password.as_bytes(),
                "restore_native:pg_restore",
                deadline,
            )
            .map_err(NativeRunError::after_target_created)?;
        require_success(output)
            .map(|_| ())
            .map_err(NativeRunError::after_target_created)
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_mysql_restore(
        &self,
        claim: &ClaimedWork,
        profile: &ConnectionExecutionBinding,
        credentials: &ResolvedCredentials,
        context: &RestoreClaimContext,
        materialized: &MaterializedRecoveryPoint,
        client_kind: ClientKind,
        deadline: Instant,
    ) -> Result<(), NativeRunError> {
        reject_unbound_network_overwrite(context)?;
        let dump = materialized.database_part().map_err(read_error)?;
        if !matches!(dump.dump_format(), "plain" | "sql") {
            return Err(NativeRunError::terminal("RESTORE_DUMP_FORMAT_UNSUPPORTED"));
        }
        let tool_name = if client_kind == ClientKind::MariaDb {
            "mariadb"
        } else {
            "mysql"
        };
        let client = self.resolver.trusted_tool(tool_name).map_err(read_error)?;
        validate_tool_compatibility(
            if client_kind == ClientKind::MariaDb {
                Engine::Mariadb
            } else {
                Engine::Mysql
            },
            dump.dump_format(),
            materialized.source_tool_version(),
            client.version(),
        )?;
        let target = context.target_database_name();
        let identity_expression = if client_kind == ClientKind::MariaDb {
            "@@GLOBAL.server_uid"
        } else {
            "@@GLOBAL.server_uuid"
        };
        let target_probe_sql = format!(
            "SELECT VERSION(), {identity_expression}, EXISTS(SELECT 1 FROM information_schema.schemata WHERE schema_name='{}')",
            sql_literal(target)
        );
        let target_probe = self.run_mysql_query(
            claim,
            &client,
            profile,
            credentials,
            None,
            &target_probe_sql,
            client_kind,
            "restore_target_identity",
            deadline,
        )?;
        let (live_server_version, live_server_identity, exists) =
            parse_mysql_target_probe(client_kind, &target_probe)?;
        if context.target_server_identity() != Some(live_server_identity.as_str()) {
            return Err(NativeRunError::terminal("RESTORE_TARGET_CHANGED"));
        }
        if !matches!(context.conflict_policy(), "create_new" | "overwrite") {
            return Err(NativeRunError::terminal("RESTORE_CONFLICT_POLICY_INVALID"));
        }
        match context.conflict_policy() {
            "create_new" if exists => {
                return Err(NativeRunError::terminal("RESTORE_TARGET_EXISTS"))
            }
            "overwrite" if !exists => {
                return Err(NativeRunError::terminal("RESTORE_TARGET_MISSING"));
            }
            "create_new" => {}
            "overwrite" => {
                self.arm_destructive_step(
                    claim,
                    Some(&live_server_version),
                    Some(&live_server_identity),
                    exists,
                )?;
                let sql = format!("DROP DATABASE {}", mysql_ident(target));
                self.run_mysql_query(
                    claim,
                    &client,
                    profile,
                    credentials,
                    None,
                    &sql,
                    client_kind,
                    "restore_target_drop",
                    deadline,
                )
                .map_err(NativeRunError::after_target_created)?;
            }
            _ => unreachable!("conflict policy was validated"),
        }
        if context.conflict_policy() == "create_new" {
            self.arm_destructive_step(
                claim,
                Some(&live_server_version),
                Some(&live_server_identity),
                exists,
            )?;
        }
        let sql = guarded_mysql_create_database_sql(target, &live_server_identity, client_kind)?;
        self.run_mysql_query(
            claim,
            &client,
            profile,
            credentials,
            None,
            &sql,
            client_kind,
            "restore_target_create",
            deadline,
        )
        .map_err(NativeRunError::after_target_created)?;
        let source_path = dump.path().to_string_lossy().replace('\\', "/");
        if source_path.contains(['\0', '\r', '\n', '"']) {
            return Err(NativeRunError::terminal("RESTORE_STAGING_INVALID").after_target_created());
        }
        let mut args = mysql_connection_args(profile, credentials, Some(target), client_kind)
            .map_err(NativeRunError::after_target_created)?;
        args.push(OsString::from("--binary-mode"));
        args.push(OsString::from(format!(
            "--execute=source \"{source_path}\""
        )));
        let working_directory = dump
            .path()
            .parent()
            .ok_or_else(|| NativeRunError::terminal("RESTORE_STAGING_INVALID"))
            .map_err(NativeRunError::after_target_created)?;
        let output = self
            .run_native(
                claim,
                &client,
                working_directory,
                args,
                mysql_environment(credentials),
                credentials.password.as_bytes(),
                &format!("restore_native:{tool_name}"),
                deadline,
            )
            .map_err(NativeRunError::after_target_created)?;
        require_success(output)
            .map(|_| ())
            .map_err(NativeRunError::after_target_created)
    }

    #[allow(clippy::too_many_arguments)]
    fn run_pg_query(
        &self,
        claim: &ClaimedWork,
        tool: &TrustedNativeTool,
        profile: &ConnectionExecutionBinding,
        credentials: &ResolvedCredentials,
        database: &str,
        sql: &str,
        phase: &str,
        deadline: Instant,
    ) -> Result<String, NativeRunError> {
        let mut args = pg_connection_args(profile, credentials, database)?;
        args.extend([
            OsString::from("--tuples-only"),
            OsString::from("--no-align"),
            OsString::from("--field-separator=\t"),
            OsString::from("--set=ON_ERROR_STOP=1"),
            OsString::from("--command"),
            OsString::from(sql),
        ]);
        let output = self.run_native(
            claim,
            tool,
            tool.path()
                .parent()
                .ok_or_else(|| NativeRunError::terminal("RESTORE_TOOL_INVALID"))?,
            args,
            pg_environment(profile, credentials)?,
            credentials.password.as_bytes(),
            phase,
            deadline,
        )?;
        output_text(require_success(output)?)
    }

    #[allow(clippy::too_many_arguments)]
    fn run_pg_script(
        &self,
        claim: &ClaimedWork,
        tool: &TrustedNativeTool,
        profile: &ConnectionExecutionBinding,
        credentials: &ResolvedCredentials,
        database: &str,
        script: &str,
        staging_directory: &Path,
        phase: &str,
        deadline: Instant,
    ) -> Result<String, NativeRunError> {
        self.check_claim_control(claim, deadline)?;
        let script = EphemeralSqlScript::create(staging_directory, script)?;
        let mut args = pg_connection_args(profile, credentials, database)?;
        args.extend([
            OsString::from("--tuples-only"),
            OsString::from("--no-align"),
            OsString::from("--set=ON_ERROR_STOP=1"),
            OsString::from("--file"),
            script.path().as_os_str().to_owned(),
        ]);
        let output = self.run_native(
            claim,
            tool,
            staging_directory,
            args,
            pg_environment(profile, credentials)?,
            credentials.password.as_bytes(),
            phase,
            deadline,
        )?;
        output_text(require_success(output)?)
    }

    #[allow(clippy::too_many_arguments)]
    fn run_mysql_query(
        &self,
        claim: &ClaimedWork,
        tool: &TrustedNativeTool,
        profile: &ConnectionExecutionBinding,
        credentials: &ResolvedCredentials,
        database: Option<&str>,
        sql: &str,
        client_kind: ClientKind,
        phase: &str,
        deadline: Instant,
    ) -> Result<String, NativeRunError> {
        let mut args = mysql_connection_args(profile, credentials, database, client_kind)?;
        args.extend([
            OsString::from("--skip-column-names"),
            OsString::from("--batch"),
            OsString::from("--execute"),
            OsString::from(sql),
        ]);
        let output = self.run_native(
            claim,
            tool,
            tool.path()
                .parent()
                .ok_or_else(|| NativeRunError::terminal("RESTORE_TOOL_INVALID"))?,
            args,
            mysql_environment(credentials),
            credentials.password.as_bytes(),
            phase,
            deadline,
        )?;
        output_text(require_success(output)?)
    }

    #[allow(clippy::too_many_arguments)]
    fn run_native(
        &self,
        claim: &ClaimedWork,
        tool: &TrustedNativeTool,
        working_directory: &Path,
        args: Vec<OsString>,
        environment: Vec<(OsString, OsString)>,
        redacted_password: &[u8],
        phase: &str,
        deadline: Instant,
    ) -> Result<ProcessExit, NativeRunError> {
        self.check_claim_control(claim, deadline)?;
        let spec = LaunchSpec::new(
            tool.path().to_path_buf(),
            working_directory.to_path_buf(),
            *tool.sha256(),
        )
        .and_then(|spec| spec.with_args(args))
        .and_then(|spec| spec.with_environment(environment))
        .and_then(|spec| spec.with_output_tail_bytes(1024 * 1024))
        .and_then(|spec| spec.with_redacted_output_literals([redacted_password]))
        .map_err(|_| NativeRunError::terminal("RESTORE_PROCESS_SPEC_INVALID"))?;
        let mut process = ProcessSupervisor::launch(&spec)
            .map_err(|_| NativeRunError::retryable("RESTORE_PROCESS_LAUNCH_FAILED"))?;
        let process_instance_id = uuid::Uuid::new_v4().to_string();
        let started_at_ms = self.clock.now_ms();
        let process_start_time_ms = windows_time_to_unix_ms(process.start_time_100ns())
            .unwrap_or(started_at_ms)
            .min(started_at_ms);
        if self
            .store
            .runtime()
            .record_process_start(ProcessStartSpec::new(
                process_instance_id.clone(),
                claim.clone(),
                phase,
                i64::from(process.id()),
                process_start_time_ms,
                claim.worker_boot_id(),
                process.job_key(),
                tool.path().to_string_lossy().into_owned(),
                Some(hex_sha256(tool.sha256())),
                started_at_ms,
            ))
            .and_then(|()| {
                self.store.runtime().mark_process_running(
                    claim,
                    &process_instance_id,
                    started_at_ms,
                )
            })
            .is_err()
        {
            let _ = process.terminate();
            return Err(NativeRunError::retryable("RESTORE_PROCESS_RECORD_FAILED"));
        }
        let mut last_heartbeat = Instant::now();
        loop {
            if self.stopping.load(Ordering::Acquire) {
                let exit = process
                    .terminate()
                    .map_err(|_| NativeRunError::terminal("RESTORE_PROCESS_TERMINATE_FAILED"))?;
                self.record_exit(claim, process_instance_id, &exit, "service_stopping");
                return Err(NativeRunError::retryable("RESTORE_SERVICE_STOPPING"));
            }
            if self
                .store
                .runtime()
                .claim_cancel_requested(claim, self.clock.now_ms())
                .unwrap_or(true)
            {
                let exit = process
                    .terminate()
                    .map_err(|_| NativeRunError::terminal("RESTORE_PROCESS_TERMINATE_FAILED"))?;
                self.record_exit(claim, process_instance_id, &exit, "cancelled");
                return Err(NativeRunError::terminal("CANCEL_REQUESTED"));
            }
            if Instant::now() >= deadline {
                let exit = process
                    .terminate()
                    .map_err(|_| NativeRunError::terminal("RESTORE_PROCESS_TERMINATE_FAILED"))?;
                self.record_exit(claim, process_instance_id, &exit, "timeout");
                return Err(NativeRunError::terminal("RESTORE_PROCESS_TIMEOUT"));
            }
            match process.wait_timeout(NATIVE_POLL_INTERVAL) {
                Ok(Some(exit)) => {
                    self.record_exit(claim, process_instance_id, &exit, "exited");
                    return Ok(exit);
                }
                Ok(None) => {}
                Err(_) => {
                    let exit = process.terminate().map_err(|_| {
                        NativeRunError::terminal("RESTORE_PROCESS_TERMINATE_FAILED")
                    })?;
                    self.record_exit(claim, process_instance_id, &exit, "wait_failed");
                    return Err(NativeRunError::terminal("RESTORE_PROCESS_WAIT_FAILED"));
                }
            }
            if last_heartbeat.elapsed() >= NATIVE_HEARTBEAT_INTERVAL {
                let now_ms = self.clock.now_ms();
                let lease_expires_at_ms = now_ms.saturating_add(LEASE_DURATION_MS);
                if self
                    .store
                    .runtime()
                    .heartbeat_claim(claim, now_ms, lease_expires_at_ms)
                    .is_err()
                {
                    let exit = process.terminate().map_err(|_| {
                        NativeRunError::terminal("RESTORE_PROCESS_TERMINATE_FAILED")
                    })?;
                    self.record_exit(claim, process_instance_id, &exit, "lease_lost");
                    return Err(NativeRunError::terminal("RESTORE_LEASE_LOST"));
                }
                last_heartbeat = Instant::now();
            }
        }
    }

    fn record_exit(
        &self,
        claim: &ClaimedWork,
        process_instance_id: String,
        exit: &ProcessExit,
        reason: &str,
    ) {
        if let Err(error) = self.store.runtime().record_process_exit(
            claim,
            ProcessExitSpec::new(
                process_instance_id,
                self.clock.now_ms(),
                i64::from(exit.exit_code()),
                reason,
            ),
        ) {
            tracing::error!(
                error_code = error.code(),
                "restore native process exit could not be recorded"
            );
        }
    }
}

#[async_trait]
impl RestoreExecutor for ServiceRestoreExecutor {
    fn release_claim_resources(&self, _claim: &ClaimedWork, context: &RestoreClaimContext) {
        let restore_run_id = context.restore_run_id().as_str();
        match self.prepared.lock() {
            Ok(mut prepared) => {
                prepared.remove(restore_run_id);
            }
            Err(error) => {
                tracing::error!(
                    error = %error,
                    restore_run_id,
                    "restore prepared-state cleanup failed"
                );
            }
        }
        match self.pending.lock() {
            Ok(mut pending) => {
                pending.remove(restore_run_id);
            }
            Err(error) => {
                tracing::error!(
                    error = %error,
                    restore_run_id,
                    "restore staging cleanup failed"
                );
            }
        }
    }

    async fn admit(
        &self,
        claim: &ClaimedWork,
        context: &RestoreClaimContext,
    ) -> Result<RestoreAdmission, RestoreExecutionError> {
        let deadline = Instant::now()
            .checked_add(NATIVE_EXECUTION_TIMEOUT)
            .ok_or_else(|| restore_error("RESTORE_EXECUTION_TIMEOUT", false, None))?;
        let source_replica_id = context
            .source_replica_id()
            .ok_or_else(|| restore_error("RESTORE_SOURCE_REPLICA_MISSING", false, None))?;
        let verified = await_claim_operation(
            self.resolver.resolve(
                context.recovery_point_id(),
                source_replica_id,
                self.clock.now_ms(),
            ),
            &self.store,
            self.clock.as_ref(),
            claim,
            &self.stopping,
            deadline,
        )
        .await
        .map_err(restore_read_error)?;
        let engine = verified.signed().manifest.engine;
        if !matches!(
            engine,
            Engine::Postgresql | Engine::Mysql | Engine::Mariadb | Engine::Sqlite
        ) {
            return Err(restore_error("RESTORE_ENGINE_UNSUPPORTED", false, None));
        }
        let profile = self
            .profile(context, engine)
            .map_err(|code| restore_error(code, false, None))?;
        let scope_tables = verified.scope_tables().cloned();
        validate_signed_restore_contract(verified.signed(), scope_tables.as_ref())
            .map_err(|code| restore_error(code, false, None))?;
        let stage_key = format!(
            "restore-{}-{}-{}",
            claim.work_item_id(),
            claim.attempt_no(),
            claim.fencing_token()
        );
        let admission = self.admit_capacity(claim, context, &verified, &stage_key)?;
        if admission != RestoreAdmission::Admitted {
            return Ok(admission);
        }
        let mut prepared = self
            .prepared
            .lock()
            .map_err(|_| restore_error("RESTORE_INTERNAL_STATE", false, None))?;
        if prepared
            .insert(
                context.restore_run_id().to_string(),
                PreparedRestore {
                    verified,
                    profile,
                    scope_tables,
                    deadline,
                    stage_key,
                },
            )
            .is_some()
        {
            return Err(restore_error("RESTORE_INTERNAL_STATE", false, None));
        }
        Ok(RestoreAdmission::Admitted)
    }

    async fn restore(
        &self,
        claim: &ClaimedWork,
        context: &RestoreClaimContext,
    ) -> Result<RestoreExecutionEvidence, RestoreExecutionError> {
        let prepared = self
            .prepared
            .lock()
            .map_err(|_| restore_error("RESTORE_INTERNAL_STATE", false, None))?
            .remove(context.restore_run_id().as_str())
            .ok_or_else(|| restore_error("RESTORE_ADMISSION_REQUIRED", true, None))?;
        let PreparedRestore {
            verified,
            profile,
            scope_tables,
            deadline,
            stage_key,
        } = prepared;
        let engine = verified.signed().manifest.engine;
        let control = self.materialization_control(claim, deadline);
        let materialized = await_claim_operation(
            verified.materialize(&control, &stage_key),
            &self.store,
            self.clock.as_ref(),
            claim,
            &self.stopping,
            deadline,
        )
        .await
        .map_err(restore_read_error)?;
        let native_result = match engine {
            Engine::Sqlite => {
                self.execute_sqlite_restore(claim, &profile, context, &materialized, deadline)
            }
            Engine::Postgresql => {
                let credentials = self
                    .credentials(&profile)
                    .map_err(|code| restore_error(code, false, None))?;
                self.execute_postgresql_restore(
                    claim,
                    &profile,
                    &credentials,
                    context,
                    &materialized,
                    deadline,
                )
            }
            Engine::Mysql => {
                let credentials = self
                    .credentials(&profile)
                    .map_err(|code| restore_error(code, false, None))?;
                self.execute_mysql_restore(
                    claim,
                    &profile,
                    &credentials,
                    context,
                    &materialized,
                    ClientKind::OracleMySql,
                    deadline,
                )
            }
            Engine::Mariadb => {
                let credentials = self
                    .credentials(&profile)
                    .map_err(|code| restore_error(code, false, None))?;
                self.execute_mysql_restore(
                    claim,
                    &profile,
                    &credentials,
                    context,
                    &materialized,
                    ClientKind::MariaDb,
                    deadline,
                )
            }
            _ => Err(NativeRunError::terminal("RESTORE_ENGINE_UNSUPPORTED")),
        };
        if let Err(error) = native_result {
            return Err(restore_native_error(error));
        }
        let workbench_restore_identity = if engine == Engine::Sqlite {
            // Optional Workbench proof does not change legacy restore support.
            // Unknown/ReFS/remote filesystems simply provide no new authority.
            (|| {
                let path = profile.file_path()?;
                let mut options = std::fs::OpenOptions::new();
                options.read(true);
                #[cfg(windows)]
                {
                    use std::os::windows::fs::OpenOptionsExt;
                    options.share_mode(3).custom_flags(0x0020_0000);
                }
                let file = options.open(path).ok()?;
                if !restoreplane_source_sqlite::opened_handle_is_local_ntfs(&file).ok()? {
                    return None;
                }
                let probe = probe_sqlite_file(Path::new(path)).ok()?;
                let actual = restoreplane_source_sqlite::opened_file_object_id(&file).ok()?;
                if actual != probe.object_id() {
                    return None;
                }
                Some(json!({
                    "signatureVersion":1,
                    "engine":"sqlite",
                    "connectionProfileId":context.target_connection_profile_id(),
                    "databaseName":context.target_database_name(),
                    "serverIdentity":null,
                    "actualDatabaseIdentity":format!("sqlite-file-object-id:{actual}"),
                }))
            })()
        } else {
            None
        };
        let recovery_evidence = materialized.evidence();
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| restore_error("RESTORE_INTERNAL_STATE", false, None))?;
        if pending
            .insert(
                context.restore_run_id().to_string(),
                PendingRestore {
                    materialized,
                    profile,
                    target_name: context.target_database_name().to_owned(),
                    engine,
                    scope_tables,
                    deadline,
                },
            )
            .is_some()
        {
            return Err(restore_error("RESTORE_INTERNAL_STATE", false, None));
        }
        let mut evidence = json!({
            "schemaVersion": 1,
            "engine": engine_name(engine),
            "source": recovery_evidence,
            "nativeRestore": {
                "status": "passed",
                "bundleOnlyTool": !matches!(engine, Engine::Sqlite),
            },
        });
        if let Some(identity) = workbench_restore_identity {
            evidence["workbenchRestoreIdentity"] = identity;
        }
        RestoreExecutionEvidence::completed(0, true, evidence)
            .map_err(|_| restore_error("RESTORE_EXECUTION_EVIDENCE_INVALID", false, None))
    }

    async fn validate(
        &self,
        claim: &ClaimedWork,
        context: &RestoreClaimContext,
        _restore: &RestoreExecutionEvidence,
    ) -> Result<RestoreValidationEvidence, RestoreValidationError> {
        let pending = self
            .pending
            .lock()
            .map_err(|_| validation_error("RESTORE_INTERNAL_STATE"))?
            .remove(context.restore_run_id().as_str())
            .ok_or_else(|| validation_error("RESTORE_VALIDATION_STATE_MISSING"))?;
        let oracle = match pending.engine {
            Engine::Sqlite => self.validate_sqlite(claim, &pending),
            Engine::Postgresql => self.validate_postgresql(claim, &pending),
            Engine::Mysql => self.validate_mysql(claim, &pending, ClientKind::OracleMySql),
            Engine::Mariadb => self.validate_mysql(claim, &pending, ClientKind::MariaDb),
            _ => Err(NativeRunError::terminal("RESTORE_ENGINE_UNSUPPORTED")),
        }
        .map_err(|error| validation_error(error.code))?;
        RestoreValidationEvidence::passed(oracle)
            .map_err(|_| validation_error("RESTORE_VALIDATION_EVIDENCE_INVALID"))
    }
}

impl ServiceRestoreExecutor {
    fn validate_sqlite(
        &self,
        claim: &ClaimedWork,
        pending: &PendingRestore,
    ) -> Result<serde_json::Value, NativeRunError> {
        self.check_claim_control(claim, pending.deadline)
            .map_err(NativeRunError::after_target_created)?;
        let target = pending
            .profile
            .file_path()
            .map(Path::new)
            .ok_or_else(|| NativeRunError::terminal("RESTORE_TARGET_BINDING_INVALID"))?;
        let (_cancellation, control, control_error) =
            self.sqlite_claim_control(claim, pending.deadline);
        let evidence = validate_database_observed(target, None, move || control())
            .map_err(|_| sqlite_validation_control_error(&control_error))?;
        Ok(json!({
            "schemaVersion": 1,
            "engine": "sqlite",
            "integrity": evidence.integrity_ok(),
            "foreignKeys": evidence.foreign_keys_ok(),
            "sourcePartCount": pending.materialized.parts().len(),
        }))
    }

    fn validate_postgresql(
        &self,
        claim: &ClaimedWork,
        pending: &PendingRestore,
    ) -> Result<serde_json::Value, NativeRunError> {
        let credentials = self
            .credentials(&pending.profile)
            .map_err(NativeRunError::terminal)?;
        let psql = self.resolver.trusted_tool("psql").map_err(read_error)?;
        let list_sql = "SELECT n.nspname, c.relname FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE c.relkind IN ('r','p') AND n.nspname NOT IN ('pg_catalog','information_schema') AND n.nspname !~ '^pg_toast' ORDER BY n.nspname,c.relname";
        let output = self.run_pg_query(
            claim,
            &psql,
            &pending.profile,
            &credentials,
            &pending.target_name,
            list_sql,
            "restore_oracle:pg_schema",
            pending.deadline,
        )?;
        let actual = parse_two_column_set(&output)?;
        let oracle_tables = exact_oracle_tables(pending.scope_tables.as_ref(), actual)?;
        let mut content_observations = Vec::with_capacity(oracle_tables.len());
        for (schema, table) in &oracle_tables {
            let sql = format!(
                "SELECT count(*)::text, \
                        COALESCE(bit_xor(hashtextextended(row_to_json(t)::text, 0)), 0)::text, \
                        COALESCE(sum(hashtextextended(row_to_json(t)::text, 0)::numeric), 0)::text \
                 FROM {}.{} AS t",
                pg_ident(schema),
                pg_ident(table)
            );
            let observation = self.run_pg_query(
                claim,
                &psql,
                &pending.profile,
                &credentials,
                &pending.target_name,
                &sql,
                "restore_oracle:pg_content",
                pending.deadline,
            )?;
            content_observations.push(format!(
                "{schema}\t{table}\t{}",
                parse_pg_content_observation(&observation)?
            ));
        }
        let schema_lines = oracle_tables
            .iter()
            .map(|(schema, table)| format!("{schema}\t{table}"))
            .collect::<Vec<_>>();
        let (schema_hash, table_count) = normalized_sha256(schema_lines);
        let (content_hash, content_table_count) = normalized_sha256(content_observations);
        Ok(json!({
            "schemaVersion": 1,
            "engine": "postgresql",
            "nativeValidation": "psql",
            "schemaHashSha256": schema_hash,
            "tableCount": table_count,
            "contentOracleHashSha256": content_hash,
            "contentOracleTableCount": content_table_count,
            "contentOracleAlgorithm": "pg-row-json-hashtextextended-xor-sum-v1",
            "selectedScopeExact": pending.scope_tables.is_some(),
        }))
    }

    fn validate_mysql(
        &self,
        claim: &ClaimedWork,
        pending: &PendingRestore,
        client_kind: ClientKind,
    ) -> Result<serde_json::Value, NativeRunError> {
        let credentials = self
            .credentials(&pending.profile)
            .map_err(NativeRunError::terminal)?;
        let tool_name = if client_kind == ClientKind::MariaDb {
            "mariadb"
        } else {
            "mysql"
        };
        let client = self.resolver.trusted_tool(tool_name).map_err(read_error)?;
        let output = self.run_mysql_query(
            claim,
            &client,
            &pending.profile,
            &credentials,
            Some(&pending.target_name),
            "SELECT DATABASE(), table_name FROM information_schema.tables WHERE table_schema=DATABASE() AND table_type='BASE TABLE' ORDER BY table_name",
            client_kind,
            "restore_oracle:mysql_schema",
            pending.deadline,
        )?;
        let actual = parse_two_column_set(&output)?;
        let expected = pending.scope_tables.as_ref().map(|tables| {
            tables
                .iter()
                .map(|(_, table)| (pending.target_name.clone(), table.clone()))
                .collect::<BTreeSet<_>>()
        });
        let oracle_tables = exact_oracle_tables(expected.as_ref(), actual)?;
        let mut content_observations = Vec::with_capacity(oracle_tables.len());
        for (_, table) in &oracle_tables {
            let sql = format!("SELECT count(*) FROM {}", mysql_ident(table));
            let count = self.run_mysql_query(
                claim,
                &client,
                &pending.profile,
                &credentials,
                Some(&pending.target_name),
                &sql,
                client_kind,
                "restore_oracle:mysql_content",
                pending.deadline,
            )?;
            let checksum_sql = format!(
                "CHECKSUM TABLE {}.{}",
                mysql_ident(&pending.target_name),
                mysql_ident(table)
            );
            let checksum = self.run_mysql_query(
                claim,
                &client,
                &pending.profile,
                &credentials,
                Some(&pending.target_name),
                &checksum_sql,
                client_kind,
                "restore_oracle:mysql_checksum",
                pending.deadline,
            )?;
            content_observations.push(format!(
                "{}\t{table}\t{}\t{}",
                pending.target_name,
                parse_count(&count)?,
                parse_mysql_checksum(&checksum)?
            ));
        }
        let schema_lines = oracle_tables
            .iter()
            .map(|(schema, table)| format!("{schema}\t{table}"))
            .collect::<Vec<_>>();
        let (schema_hash, table_count) = normalized_sha256(schema_lines);
        let (content_hash, content_table_count) = normalized_sha256(content_observations);
        Ok(json!({
            "schemaVersion": 1,
            "engine": engine_name(pending.engine),
            "nativeValidation": tool_name,
            "schemaHashSha256": schema_hash,
            "tableCount": table_count,
            "contentOracleHashSha256": content_hash,
            "contentOracleTableCount": content_table_count,
            "contentOracleAlgorithm": "mysql-native-table-checksum-plus-count-v1",
            "selectedScopeExact": pending.scope_tables.is_some(),
        }))
    }
}

struct ServiceVerificationExecutor {
    core: Arc<ServiceRestoreExecutor>,
    prepared: Mutex<HashMap<String, PreparedVerification>>,
}

impl ServiceVerificationExecutor {
    fn new(core: Arc<ServiceRestoreExecutor>) -> Self {
        Self {
            core,
            prepared: Mutex::new(HashMap::new()),
        }
    }

    fn admit_capacity(
        &self,
        claim: &ClaimedWork,
        verified: &crate::recovery_point::VerifiedRecoveryPoint,
        stage_key: &str,
    ) -> Result<VerificationAdmission, VerificationExecutionError> {
        std::fs::create_dir_all(&self.core.spool_root)
            .map_err(|_| verification_error_code("VERIFY_SPOOL_UNAVAILABLE", true))?;
        let free_bytes = fs2::available_space(&self.core.spool_root)
            .map_err(|_| verification_error_code("VERIFY_SPOOL_UNAVAILABLE", true))?;
        let free_bytes = i64::try_from(free_bytes).unwrap_or(i64::MAX);
        let reserved_bytes = verified
            .required_staging_bytes()
            .map_err(verification_read_error)?;
        let now_ms = self.core.clock.now_ms();
        if now_ms < 0 {
            return Err(verification_error_code("VERIFY_CLOCK_INVALID", true));
        }
        let runtime = self.core.store.runtime();
        let global_pool = runtime
            .upsert_resource_pool(ResourcePoolSpec::new(
                stable_capacity_id("restore", "global"),
                "restore",
                "global",
                RESTORE_GLOBAL_CONCURRENCY,
                now_ms,
            ))
            .map_err(|_| verification_error_code("VERIFY_ADMISSION_UNAVAILABLE", true))?;
        let source_key = verified.destination_fingerprint().to_owned();
        let source_pool = runtime
            .upsert_resource_pool(ResourcePoolSpec::new(
                stable_capacity_id("restore-source", &source_key),
                "destination",
                format!("restore:{source_key}"),
                RESTORE_RESOURCE_CONCURRENCY,
                now_ms,
            ))
            .map_err(|_| verification_error_code("VERIFY_ADMISSION_UNAVAILABLE", true))?;
        let volume_key = normalized_spool_key(&self.core.spool_root)
            .map_err(|_| verification_error_code("VERIFY_SPOOL_UNAVAILABLE", true))?;
        let volume_id = runtime
            .upsert_spool_volume(SpoolVolumeSpec::ready(
                stable_capacity_id("spool", &volume_key),
                volume_key,
                self.core.spool_root.to_string_lossy(),
                free_bytes.max(1),
                free_bytes.max(1),
                RESTORE_SPOOL_RESERVE_FLOOR_BYTES,
                free_bytes.max(0),
                now_ms,
            ))
            .map_err(|_| verification_error_code("VERIFY_ADMISSION_UNAVAILABLE", true))?;
        let spool_path = verified
            .staging_path(stage_key)
            .map_err(verification_read_error)?;
        let outcome = runtime
            .admit_claim(AdmissionRequest::new(
                claim.clone(),
                vec![
                    ResourceRequirement::new(global_pool, 1),
                    ResourceRequirement::new(source_pool, 1),
                ],
                Some(SpoolRequirement::new(
                    volume_id,
                    reserved_bytes,
                    spool_path.to_string_lossy(),
                )),
                now_ms,
            ))
            .map_err(|_| verification_error_code("VERIFY_ADMISSION_UNAVAILABLE", true))?;
        if outcome.admitted() {
            Ok(VerificationAdmission::Admitted)
        } else {
            VerificationAdmission::deferred(
                outcome
                    .wait_reason()
                    .unwrap_or("verification_admission_deferred"),
            )
            .map_err(|_| verification_error_code("VERIFY_ADMISSION_UNAVAILABLE", true))
        }
    }
}

#[async_trait]
impl VerificationExecutor for ServiceVerificationExecutor {
    async fn admit(
        &self,
        claim: &ClaimedWork,
        context: &VerificationClaimContext,
    ) -> Result<VerificationAdmission, VerificationExecutionError> {
        if !matches!(context.requested_level(), 1 | 2) {
            return Err(verification_error_code("VERIFY_LEVEL_UNSUPPORTED", false));
        }
        if let Some(code) =
            verification_level_preflight_error(context.source_engine(), context.requested_level())
        {
            return Err(verification_error_code(code, false));
        }
        let source_replica_id = context
            .source_replica_id()
            .ok_or_else(|| verification_error_code("VERIFY_SOURCE_REPLICA_MISSING", false))?;
        let deadline = Instant::now()
            .checked_add(NATIVE_EXECUTION_TIMEOUT)
            .ok_or_else(|| verification_error_code("VERIFY_EXECUTION_TIMEOUT", false))?;
        let verified = await_claim_operation(
            self.core.resolver.resolve(
                context.recovery_point_id(),
                source_replica_id,
                self.core.clock.now_ms(),
            ),
            &self.core.store,
            self.core.clock.as_ref(),
            claim,
            &self.core.stopping,
            deadline,
        )
        .await
        .map_err(verification_read_error)?;
        let engine = verified.signed().manifest.engine;
        if engine_name(engine) != context.source_engine() {
            return Err(verification_error_code(
                "VERIFY_SOURCE_ENGINE_MISMATCH",
                false,
            ));
        }
        if context.requested_level() == 2 {
            match engine {
                Engine::Mysql | Engine::Mariadb => {
                    return Err(verification_error_code(
                        "VERIFY_L2_NATIVE_UNSUPPORTED",
                        false,
                    ));
                }
                Engine::Postgresql => {
                    let part = verified
                        .signed()
                        .manifest
                        .parts
                        .iter()
                        .find(|part| part.role == restoreplane_core::ArtifactPartRole::DatabaseDump)
                        .ok_or_else(|| {
                            verification_error_code("VERIFY_DATABASE_PART_INVALID", false)
                        })?;
                    let tool = self
                        .core
                        .resolver
                        .trusted_tool("pg_restore")
                        .map_err(verification_read_error)?;
                    validate_tool_compatibility(
                        engine,
                        &part.dump_format,
                        &verified.signed().manifest.tool.version,
                        tool.version(),
                    )
                    .map_err(|error| verification_error_code(error.code, error.retryable))?;
                }
                Engine::Sqlite => {}
                Engine::Sqlserver => {
                    return Err(verification_error_code(
                        "VERIFY_L2_NATIVE_UNSUPPORTED",
                        false,
                    ));
                }
            }
        }
        validate_signed_verification_contract(
            verified.signed(),
            verified.scope_tables(),
            context.requested_level(),
        )
        .map_err(|code| verification_error_code(code, false))?;
        let stage_key = format!(
            "verify-{}-{}-{}",
            claim.work_item_id(),
            claim.attempt_no(),
            claim.fencing_token()
        );
        let admission = self.admit_capacity(claim, &verified, &stage_key)?;
        if admission != VerificationAdmission::Admitted {
            return Ok(admission);
        }
        let mut prepared = self
            .prepared
            .lock()
            .map_err(|_| verification_error_code("VERIFY_INTERNAL_STATE", false))?;
        if prepared
            .insert(
                context.verification_run_id().to_string(),
                PreparedVerification {
                    verified,
                    deadline,
                    stage_key,
                },
            )
            .is_some()
        {
            return Err(verification_error_code("VERIFY_INTERNAL_STATE", false));
        }
        Ok(VerificationAdmission::Admitted)
    }

    async fn execute(
        &self,
        claim: &ClaimedWork,
        context: &VerificationClaimContext,
    ) -> Result<VerificationExecutionEvidence, VerificationExecutionError> {
        let PreparedVerification {
            verified,
            deadline,
            stage_key,
        } = self
            .prepared
            .lock()
            .map_err(|_| verification_error_code("VERIFY_INTERNAL_STATE", false))?
            .remove(context.verification_run_id().as_str())
            .ok_or_else(|| verification_error_code("VERIFY_ADMISSION_REQUIRED", true))?;
        let engine = verified.signed().manifest.engine;
        let control = self.core.materialization_control(claim, deadline);
        let materialized = await_claim_operation(
            verified.materialize(&control, &stage_key),
            &self.core.store,
            self.core.clock.as_ref(),
            claim,
            &self.core.stopping,
            deadline,
        )
        .await
        .map_err(verification_read_error)?;
        let mut evidence = materialized.evidence();
        if context.requested_level() == 2 {
            let native = match engine {
                Engine::Postgresql => {
                    let part = materialized
                        .database_part()
                        .map_err(verification_read_error)?;
                    let tool = self
                        .core
                        .resolver
                        .trusted_tool("pg_restore")
                        .map_err(verification_read_error)?;
                    let args = vec![OsString::from("--list"), part.path().as_os_str().to_owned()];
                    let working_directory = tool
                        .path()
                        .parent()
                        .ok_or_else(|| verification_error_code("VERIFY_TOOL_INVALID", false))?;
                    self.core
                        .run_native(
                            claim,
                            &tool,
                            working_directory,
                            args,
                            Vec::new(),
                            &[],
                            "verification:l2_pg_restore_list",
                            deadline,
                        )
                        .and_then(require_success)
                        .map(|_| {
                            json!({
                                "validator": "pg_restore --list",
                                "toolVersion": tool.version(),
                                "toolSha256": hex_sha256(tool.sha256()),
                                "result": "passed",
                            })
                        })
                }
                Engine::Sqlite => {
                    let part = materialized
                        .database_part()
                        .map_err(verification_read_error)?;
                    let (_cancellation, control, control_error) =
                        self.core.sqlite_claim_control(claim, deadline);
                    validate_database_observed(part.path(), None, move || control())
                        .map(|validation| {
                            json!({
                                "validator": "sqlite-embedded",
                                "result": "passed",
                                "evidence": validation,
                            })
                        })
                        .map_err(|_| sqlite_verification_control_error(&control_error))
                }
                Engine::Mysql | Engine::Mariadb | Engine::Sqlserver => {
                    Err(NativeRunError::terminal("VERIFY_L2_UNSUPPORTED"))
                }
            }
            .map_err(|error| verification_error_code(error.code, error.retryable))?;
            evidence["nativeStructure"] = native;
        }
        evidence["schemaVersion"] = json!(1);
        evidence["requestedLevel"] = json!(context.requested_level());
        evidence["engine"] = json!(engine_name(engine));
        evidence["readerHashDecryptRoundTrip"] = json!("passed");
        VerificationExecutionEvidence::new(evidence.to_string())
            .map_err(|_| verification_error_code("VERIFY_EVIDENCE_INVALID", false))
    }
}

pub struct ServiceRecoveryExecutionRuntimeHandle {
    stopping: Arc<AtomicBool>,
    stop: mpsc::SyncSender<()>,
    stopped: Mutex<Option<mpsc::Receiver<()>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
    stop_wait_timeout: Duration,
}

impl ServiceRecoveryExecutionRuntimeHandle {
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        store: OperationalStore,
        catalog_path: PathBuf,
        secrets: Arc<ServiceSecretStore>,
        trusted_signing_keys: std::collections::BTreeMap<String, ed25519_dalek::VerifyingKey>,
        tool_root: PathBuf,
        staging_root: PathBuf,
        health: ServiceHealthProbe,
    ) -> Result<Self, RecoveryExecutionRuntimeStartError> {
        let spool_root = staging_root
            .parent()
            .filter(|path| path.is_absolute())
            .ok_or(RecoveryExecutionRuntimeStartError::InputInvalid)?
            .to_path_buf();
        let resolver = Arc::new(
            RecoveryPointResolver::new(
                store.clone(),
                catalog_path,
                secrets.clone(),
                trusted_signing_keys,
                tool_root,
                staging_root,
            )
            .map_err(|_| RecoveryExecutionRuntimeStartError::InputInvalid)?,
        );
        let stopping = Arc::new(AtomicBool::new(false));
        let clock: Arc<dyn RuntimeClock> = Arc::new(SystemRuntimeClock);
        let executor: Arc<dyn RestoreExecutor> = Arc::new(ServiceRestoreExecutor::new(
            store.clone(),
            secrets,
            resolver,
            spool_root,
            Arc::clone(&stopping),
            Arc::clone(&clock),
        ));
        let worker = RestoreWorker::new(
            store.runtime(),
            executor,
            Arc::clone(&clock),
            RestoreWorkerConfig::new(
                format!("restore-worker-{}", uuid::Uuid::new_v4()),
                uuid::Uuid::new_v4().to_string(),
                LEASE_DURATION_MS,
                RETRY_BACKOFF_MS,
            )
            .map_err(|_| RecoveryExecutionRuntimeStartError::WorkerInit)?,
        );
        let async_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(RecoveryExecutionRuntimeStartError::AsyncRuntime)?;
        let (stop, stop_receiver) = mpsc::sync_channel(1);
        let (stopped_sender, stopped_receiver) = mpsc::sync_channel(1);
        let thread_stopping = Arc::clone(&stopping);
        let thread = thread::Builder::new()
            .name("restoreplane-recovery-execution-runtime".to_owned())
            .spawn(move || {
                while !thread_stopping.load(Ordering::Acquire) {
                    if health.snapshot().claims_enabled() {
                        if let Err(error) = async_runtime.block_on(worker.run_once()) {
                            tracing::error!(
                                error_code = error.code(),
                                "restore runtime tick failed closed"
                            );
                        }
                    }
                    match stop_receiver.recv_timeout(DEFAULT_POLL_INTERVAL) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                }
                thread_stopping.store(true, Ordering::Release);
                let _ = stopped_sender.send(());
            })
            .map_err(RecoveryExecutionRuntimeStartError::ThreadStart)?;
        Ok(Self {
            stopping,
            stop,
            stopped: Mutex::new(Some(stopped_receiver)),
            thread: Mutex::new(Some(thread)),
            stop_wait_timeout: STOP_WAIT_TIMEOUT,
        })
    }

    pub fn request_stop(&self) {
        self.stopping.store(true, Ordering::Release);
        let _ = self.stop.try_send(());
    }

    pub fn wait_stopped(&self) -> Result<(), HostError> {
        wait_for_runtime_thread(&self.stopped, &self.thread, self.stop_wait_timeout)
    }
}

impl ClaimRuntime for ServiceRecoveryExecutionRuntimeHandle {
    fn request_stop(&self) {
        Self::request_stop(self);
    }

    fn wait_stopped(&self) -> Result<(), HostError> {
        Self::wait_stopped(self)
    }
}

impl Drop for ServiceRecoveryExecutionRuntimeHandle {
    fn drop(&mut self) {
        self.request_stop();
        let _ = self.wait_stopped();
    }
}

pub struct ServiceVerificationRuntimeHandle {
    stopping: Arc<AtomicBool>,
    stop: mpsc::SyncSender<()>,
    stopped: Mutex<Option<mpsc::Receiver<()>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
    stop_wait_timeout: Duration,
}

impl ServiceVerificationRuntimeHandle {
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        store: OperationalStore,
        catalog_path: PathBuf,
        secrets: Arc<ServiceSecretStore>,
        trusted_signing_keys: std::collections::BTreeMap<String, ed25519_dalek::VerifyingKey>,
        tool_root: PathBuf,
        staging_root: PathBuf,
        health: ServiceHealthProbe,
    ) -> Result<Self, RecoveryExecutionRuntimeStartError> {
        let spool_root = staging_root
            .parent()
            .filter(|path| path.is_absolute())
            .ok_or(RecoveryExecutionRuntimeStartError::InputInvalid)?
            .to_path_buf();
        let resolver = Arc::new(
            RecoveryPointResolver::new(
                store.clone(),
                catalog_path,
                secrets.clone(),
                trusted_signing_keys,
                tool_root,
                staging_root,
            )
            .map_err(|_| RecoveryExecutionRuntimeStartError::InputInvalid)?,
        );
        let stopping = Arc::new(AtomicBool::new(false));
        let clock: Arc<dyn RuntimeClock> = Arc::new(SystemRuntimeClock);
        let core = Arc::new(ServiceRestoreExecutor::new(
            store.clone(),
            secrets,
            resolver,
            spool_root,
            Arc::clone(&stopping),
            Arc::clone(&clock),
        ));
        let executor: Arc<dyn VerificationExecutor> =
            Arc::new(ServiceVerificationExecutor::new(core));
        let worker = VerificationWorker::new(
            store.runtime(),
            executor,
            Arc::clone(&clock),
            VerificationWorkerConfig::new(
                format!("verification-worker-{}", uuid::Uuid::new_v4()),
                uuid::Uuid::new_v4().to_string(),
                LEASE_DURATION_MS,
                RETRY_BACKOFF_MS,
            )
            .map_err(|_| RecoveryExecutionRuntimeStartError::WorkerInit)?,
        );
        let async_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(RecoveryExecutionRuntimeStartError::AsyncRuntime)?;
        let (stop, stop_receiver) = mpsc::sync_channel(1);
        let (stopped_sender, stopped_receiver) = mpsc::sync_channel(1);
        let thread_stopping = Arc::clone(&stopping);
        let thread = thread::Builder::new()
            .name("restoreplane-verification-runtime".to_owned())
            .spawn(move || {
                while !thread_stopping.load(Ordering::Acquire) {
                    if health.snapshot().claims_enabled() {
                        if let Err(error) = async_runtime.block_on(worker.run_once()) {
                            tracing::error!(
                                error_code = error.code(),
                                "verification runtime tick failed closed"
                            );
                        }
                    }
                    match stop_receiver.recv_timeout(DEFAULT_POLL_INTERVAL) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                }
                thread_stopping.store(true, Ordering::Release);
                let _ = stopped_sender.send(());
            })
            .map_err(RecoveryExecutionRuntimeStartError::ThreadStart)?;
        Ok(Self {
            stopping,
            stop,
            stopped: Mutex::new(Some(stopped_receiver)),
            thread: Mutex::new(Some(thread)),
            stop_wait_timeout: STOP_WAIT_TIMEOUT,
        })
    }

    pub fn request_stop(&self) {
        self.stopping.store(true, Ordering::Release);
        let _ = self.stop.try_send(());
    }

    pub fn wait_stopped(&self) -> Result<(), HostError> {
        wait_for_runtime_thread(&self.stopped, &self.thread, self.stop_wait_timeout)
    }
}

impl ClaimRuntime for ServiceVerificationRuntimeHandle {
    fn request_stop(&self) {
        Self::request_stop(self);
    }

    fn wait_stopped(&self) -> Result<(), HostError> {
        Self::wait_stopped(self)
    }
}

impl Drop for ServiceVerificationRuntimeHandle {
    fn drop(&mut self) {
        self.request_stop();
        let _ = self.wait_stopped();
    }
}

fn wait_for_runtime_thread(
    stopped: &Mutex<Option<mpsc::Receiver<()>>>,
    thread_handle: &Mutex<Option<JoinHandle<()>>>,
    timeout: Duration,
) -> Result<(), HostError> {
    let wait_started = Instant::now();
    let receiver = stopped.lock().map_err(|_| HostError::InternalState)?.take();
    if let Some(receiver) = receiver {
        match receiver.recv_timeout(timeout) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {
                thread_handle
                    .lock()
                    .map_err(|_| HostError::InternalState)?
                    .take();
                return Err(HostError::RuntimeFailed);
            }
        }
    }
    let thread_handle = thread_handle
        .lock()
        .map_err(|_| HostError::InternalState)?
        .take();
    if let Some(thread_handle) = thread_handle {
        while !thread_handle.is_finished() {
            let remaining = timeout.saturating_sub(wait_started.elapsed());
            if remaining.is_zero() {
                return Err(HostError::RuntimeFailed);
            }
            thread::sleep(remaining.min(Duration::from_millis(5)));
        }
        thread_handle.join().map_err(|_| HostError::RuntimeFailed)?;
    }
    Ok(())
}

fn ensure_execution_window(stopping: &AtomicBool, deadline: Instant) -> Result<(), NativeRunError> {
    if stopping.load(Ordering::Acquire) {
        return Err(NativeRunError::retryable("RESTORE_SERVICE_STOPPING"));
    }
    if Instant::now() >= deadline {
        return Err(NativeRunError::terminal("RESTORE_EXECUTION_TIMEOUT"));
    }
    Ok(())
}

fn check_claim_control_parts(
    store: &OperationalStore,
    clock: &dyn RuntimeClock,
    stopping: &AtomicBool,
    claim: &ClaimedWork,
    deadline: Instant,
) -> Result<(), NativeRunError> {
    ensure_execution_window(stopping, deadline)?;
    let heartbeat_at_ms = clock.now_ms();
    let lease_expires_at_ms = heartbeat_at_ms
        .checked_add(LEASE_DURATION_MS)
        .ok_or_else(|| NativeRunError::terminal("RESTORE_EXECUTION_TIMEOUT"))?;
    if heartbeat_at_ms < 0 {
        return Err(NativeRunError::retryable("RESTORE_CLOCK_INVALID"));
    }
    let runtime = store.runtime();
    if runtime
        .claim_cancel_requested(claim, heartbeat_at_ms)
        .map_err(|_| NativeRunError::retryable("RESTORE_LEASE_LOST"))?
    {
        return Err(NativeRunError::retryable("RECOVERY_CANCEL_REQUESTED"));
    }
    runtime
        .heartbeat_claim(claim, heartbeat_at_ms, lease_expires_at_ms)
        .map_err(|_| NativeRunError::retryable("RESTORE_LEASE_LOST"))
}

fn sqlite_control_error(control_error: &Mutex<Option<NativeRunError>>) -> NativeRunError {
    control_error
        .lock()
        .ok()
        .and_then(|stored| *stored)
        .unwrap_or_else(|| NativeRunError::terminal("RESTORE_NATIVE_FAILED"))
}

fn sqlite_validation_control_error(
    control_error: &Mutex<Option<NativeRunError>>,
) -> NativeRunError {
    control_error
        .lock()
        .ok()
        .and_then(|stored| *stored)
        .unwrap_or_else(|| NativeRunError::terminal("RESTORE_VALIDATION_FAILED"))
        .after_target_created()
}

fn sqlite_verification_control_error(
    control_error: &Mutex<Option<NativeRunError>>,
) -> NativeRunError {
    control_error
        .lock()
        .ok()
        .and_then(|stored| *stored)
        .unwrap_or_else(|| NativeRunError::terminal("VERIFY_L2_NATIVE_FAILED"))
}

#[derive(Clone, Copy, Debug)]
struct NativeRunError {
    code: &'static str,
    retryable: bool,
    target_created: bool,
}

async fn await_claim_operation<F, T>(
    operation: F,
    store: &OperationalStore,
    clock: &dyn RuntimeClock,
    claim: &ClaimedWork,
    stopping: &AtomicBool,
    deadline: Instant,
) -> Result<T, RecoveryPointReadError>
where
    F: std::future::Future<Output = Result<T, RecoveryPointReadError>>,
{
    tokio::pin!(operation);
    let mut last_heartbeat = Instant::now();
    loop {
        if stopping.load(Ordering::Acquire) {
            return Err(RecoveryPointReadError::retryable(
                "RECOVERY_SERVICE_STOPPING",
            ));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(RecoveryPointReadError::terminal(
                "RECOVERY_EXECUTION_TIMEOUT",
            ));
        }
        if last_heartbeat.elapsed() >= NATIVE_HEARTBEAT_INTERVAL {
            let now_ms = clock.now_ms();
            if store
                .runtime()
                .claim_cancel_requested(claim, now_ms)
                .map_err(|_| RecoveryPointReadError::retryable("RECOVERY_LEASE_LOST"))?
            {
                return Err(RecoveryPointReadError::retryable(
                    "RECOVERY_CANCEL_REQUESTED",
                ));
            }
            let lease_expires_at_ms = now_ms.saturating_add(LEASE_DURATION_MS);
            store
                .runtime()
                .heartbeat_claim(claim, now_ms, lease_expires_at_ms)
                .map_err(|_| RecoveryPointReadError::retryable("RECOVERY_LEASE_LOST"))?;
            last_heartbeat = Instant::now();
        }
        tokio::select! {
            result = &mut operation => return result,
            () = tokio::time::sleep(remaining.min(Duration::from_millis(100))) => {}
        }
    }
}

impl NativeRunError {
    const fn terminal(code: &'static str) -> Self {
        Self {
            code,
            retryable: false,
            target_created: false,
        }
    }

    const fn retryable(code: &'static str) -> Self {
        Self {
            code,
            retryable: true,
            target_created: false,
        }
    }

    const fn after_target_created(mut self) -> Self {
        self.retryable = false;
        self.target_created = true;
        self
    }
}

fn read_error(error: RecoveryPointReadError) -> NativeRunError {
    NativeRunError {
        code: error.code(),
        retryable: error.is_retryable(),
        target_created: false,
    }
}

fn restore_native_error(error: NativeRunError) -> RestoreExecutionError {
    if error.retryable && !error.target_created {
        return restore_error(error.code, true, None);
    }
    RestoreExecutionError::terminal(
        error.code,
        error.target_created,
        Some(1),
        json!({
            "schemaVersion": 1,
            "errorCode": error.code,
            "targetCreated": error.target_created,
        }),
    )
    .expect("static native restore error is valid")
}

fn restore_read_error(error: RecoveryPointReadError) -> RestoreExecutionError {
    restore_error(error.code(), error.is_retryable(), None)
}

fn verification_read_error(error: RecoveryPointReadError) -> VerificationExecutionError {
    verification_error_code(error.code(), error.is_retryable())
}

fn verification_error_code(code: impl Into<String>, retryable: bool) -> VerificationExecutionError {
    let code = code.into();
    let result = if retryable {
        VerificationExecutionError::retryable(code)
    } else {
        VerificationExecutionError::terminal(code)
    };
    result.unwrap_or_else(|_| {
        VerificationExecutionError::terminal("VERIFY_INTERNAL_ERROR_CODE")
            .expect("static verification error code is valid")
    })
}

fn restore_error(
    code: &'static str,
    retryable: bool,
    native_exit_code: Option<i32>,
) -> RestoreExecutionError {
    if retryable {
        RestoreExecutionError::retryable(
            code,
            json!({
                "schemaVersion": 1,
                "errorCode": code,
            }),
        )
        .expect("static restore retry error is valid")
    } else {
        RestoreExecutionError::terminal(
            code,
            false,
            native_exit_code,
            json!({
                "schemaVersion": 1,
                "errorCode": code,
            }),
        )
        .expect("static restore terminal error is valid")
    }
}

fn validation_error(code: &'static str) -> RestoreValidationError {
    RestoreValidationError::failed(
        code,
        json!({
            "schemaVersion": 1,
            "errorCode": code,
        }),
    )
    .expect("static validation error is valid")
}

fn require_success(exit: ProcessExit) -> Result<ProcessExit, NativeRunError> {
    if exit.exit_code() == 0 {
        Ok(exit)
    } else {
        Err(NativeRunError::terminal("RESTORE_NATIVE_FAILED"))
    }
}

fn output_text(exit: ProcessExit) -> Result<String, NativeRunError> {
    if exit.stdout().truncated() {
        return Err(NativeRunError::terminal("RESTORE_ORACLE_OUTPUT_TRUNCATED"));
    }
    std::str::from_utf8(exit.stdout().tail())
        .map(str::to_owned)
        .map_err(|_| NativeRunError::terminal("RESTORE_ORACLE_OUTPUT_INVALID"))
}

fn parse_two_column_set(output: &str) -> Result<BTreeSet<(String, String)>, NativeRunError> {
    let mut values = BTreeSet::new();
    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        let mut fields = line.split('\t');
        let first = fields.next().unwrap_or_default().trim();
        let second = fields.next().unwrap_or_default().trim();
        if first.is_empty()
            || second.is_empty()
            || fields.next().is_some()
            || first.len() > 256
            || second.len() > 256
            || !values.insert((first.to_owned(), second.to_owned()))
        {
            return Err(NativeRunError::terminal("RESTORE_ORACLE_OUTPUT_INVALID"));
        }
    }
    if values.len() > MAX_ORACLE_TABLES {
        return Err(NativeRunError::terminal("RESTORE_ORACLE_SCOPE_TOO_LARGE"));
    }
    Ok(values)
}

fn exact_oracle_tables(
    expected: Option<&BTreeSet<(String, String)>>,
    actual: BTreeSet<(String, String)>,
) -> Result<BTreeSet<(String, String)>, NativeRunError> {
    if let Some(expected) = expected {
        if expected.is_empty() || expected != &actual {
            return Err(NativeRunError::terminal("RESTORE_SELECTED_SCOPE_MISMATCH"));
        }
        return Ok(expected.clone());
    }
    if actual.len() > MAX_ORACLE_TABLES {
        return Err(NativeRunError::terminal("RESTORE_ORACLE_SCOPE_TOO_LARGE"));
    }
    Ok(actual)
}

fn validate_tool_compatibility(
    engine: Engine,
    dump_format: &str,
    source_version: &str,
    restore_version: &str,
) -> Result<(), NativeRunError> {
    let source_major = engine_version_major(engine, source_version)
        .ok_or_else(|| NativeRunError::terminal("RESTORE_TOOL_COMPATIBILITY_UNKNOWN"))?;
    let restore_major = engine_version_major(engine, restore_version)
        .ok_or_else(|| NativeRunError::terminal("RESTORE_TOOL_COMPATIBILITY_UNKNOWN"))?;
    let compatible = match engine {
        Engine::Postgresql => {
            dump_format == "custom"
                && (14..=18).contains(&source_major)
                && restore_major >= source_major
                && restore_major <= 18
        }
        Engine::Mysql => {
            matches!(dump_format, "plain" | "sql") && source_major == 8 && restore_major == 8
        }
        Engine::Mariadb => {
            matches!(dump_format, "plain" | "sql")
                && (10..=12).contains(&source_major)
                && restore_major >= source_major
                && restore_major <= 12
        }
        Engine::Sqlserver | Engine::Sqlite => false,
    };
    if compatible {
        Ok(())
    } else {
        Err(NativeRunError::terminal(
            "RESTORE_TOOL_COMPATIBILITY_UNSUPPORTED",
        ))
    }
}

fn validate_signed_restore_contract(
    signed: &restoreplane_pipeline::SignedSetManifestV2,
    scope_tables: Option<&BTreeSet<(String, String)>>,
) -> Result<(), &'static str> {
    let database_parts = signed
        .manifest
        .parts
        .iter()
        .filter(|part| part.role == restoreplane_core::ArtifactPartRole::DatabaseDump)
        .collect::<Vec<_>>();
    if database_parts.len() != 1 {
        return Err("RESTORE_DATABASE_PART_INVALID");
    }
    let format_valid = restore_dump_format_supported(
        signed.manifest.engine,
        database_parts[0].dump_format.as_str(),
    );
    if !format_valid {
        return Err("RESTORE_DUMP_FORMAT_UNSUPPORTED");
    }
    if signed
        .manifest
        .options
        .get("backupScopeMode")
        .is_some_and(|mode| mode == "explicit_tables")
        && scope_tables.is_none_or(BTreeSet::is_empty)
    {
        return Err("RESTORE_SCOPE_EVIDENCE_INVALID");
    }
    Ok(())
}

fn restore_dump_format_supported(engine: Engine, dump_format: &str) -> bool {
    match engine {
        Engine::Postgresql => dump_format == "custom",
        Engine::Mysql | Engine::Mariadb => matches!(dump_format, "plain" | "sql"),
        Engine::Sqlite => matches!(dump_format, "sqlite_database" | "sqlite-database"),
        Engine::Sqlserver => false,
    }
}

fn reject_unbound_network_overwrite(context: &RestoreClaimContext) -> Result<(), NativeRunError> {
    reject_unbound_network_overwrite_policy(context.conflict_policy())
}

fn reject_unbound_network_overwrite_policy(conflict_policy: &str) -> Result<(), NativeRunError> {
    if conflict_policy == "overwrite" {
        return Err(NativeRunError::terminal(
            "RESTORE_OVERWRITE_IDENTITY_UNAVAILABLE",
        ));
    }
    Ok(())
}

fn validate_signed_verification_contract(
    signed: &restoreplane_pipeline::SignedSetManifestV2,
    scope_tables: Option<&BTreeSet<(String, String)>>,
    requested_level: i64,
) -> Result<(), &'static str> {
    if requested_level == 2 {
        return validate_signed_restore_contract(signed, scope_tables)
            .map_err(|_| "VERIFY_NATIVE_FORMAT_UNSUPPORTED");
    }
    if requested_level != 1
        || signed
            .manifest
            .parts
            .iter()
            .filter(|part| part.role == restoreplane_core::ArtifactPartRole::DatabaseDump)
            .count()
            != 1
    {
        return Err("VERIFY_DATABASE_PART_INVALID");
    }
    if signed
        .manifest
        .options
        .get("backupScopeMode")
        .is_some_and(|mode| mode == "explicit_tables")
        && scope_tables.is_none_or(BTreeSet::is_empty)
    {
        return Err("VERIFY_SCOPE_EVIDENCE_INVALID");
    }
    Ok(())
}

fn verification_level_preflight_error(engine: &str, requested_level: i64) -> Option<&'static str> {
    (requested_level == 2 && matches!(engine, "mysql" | "mariadb" | "sqlserver"))
        .then_some("VERIFY_L2_NATIVE_UNSUPPORTED")
}

fn first_version_major(value: &str) -> Option<u32> {
    value
        .split(|character: char| !character.is_ascii_digit())
        .find(|part| !part.is_empty())
        .and_then(|part| part.parse().ok())
}

fn engine_version_major(engine: Engine, value: &str) -> Option<u32> {
    if matches!(engine, Engine::Mysql | Engine::Mariadb) {
        let tokens = value.split_whitespace().collect::<Vec<_>>();
        if let Some(distribution) = tokens.windows(2).find_map(|window| {
            window[0]
                .trim_matches(|character: char| !character.is_ascii_alphabetic())
                .eq_ignore_ascii_case("distrib")
                .then_some(window[1])
        }) {
            if let Some(major) = first_version_major(distribution) {
                return Some(major);
            }
        }
    }
    first_version_major(value)
}

fn parse_count(value: &str) -> Result<u64, NativeRunError> {
    let value = value.trim();
    value
        .parse::<u64>()
        .ok()
        .filter(|parsed| parsed.to_string() == value)
        .ok_or_else(|| NativeRunError::terminal("RESTORE_CONTENT_ORACLE_INVALID"))
}

fn parse_pg_content_observation(value: &str) -> Result<String, NativeRunError> {
    let mut fields = value.trim().split('\t');
    let count = fields.next().unwrap_or_default();
    let xor = fields.next().unwrap_or_default();
    let sum = fields.next().unwrap_or_default();
    if fields.next().is_some()
        || parse_count(count).is_err()
        || !canonical_signed_decimal(xor)
        || !canonical_signed_decimal(sum)
    {
        return Err(NativeRunError::terminal("RESTORE_CONTENT_ORACLE_INVALID"));
    }
    Ok(format!("{count}\t{xor}\t{sum}"))
}

fn parse_mysql_checksum(value: &str) -> Result<u64, NativeRunError> {
    let mut fields = value.trim().split('\t');
    let table = fields.next().unwrap_or_default();
    let checksum = fields.next().unwrap_or_default();
    if table.trim().is_empty() || fields.next().is_some() {
        return Err(NativeRunError::terminal("RESTORE_CONTENT_ORACLE_INVALID"));
    }
    parse_count(checksum)
}

fn canonical_signed_decimal(value: &str) -> bool {
    let digits = value.strip_prefix('-').unwrap_or(value);
    !digits.is_empty()
        && digits.len() <= 128
        && digits.bytes().all(|byte| byte.is_ascii_digit())
        && (digits == "0" || !digits.starts_with('0'))
        && value != "-0"
}

fn parse_pg_bool(value: &str) -> Result<bool, NativeRunError> {
    match value.trim() {
        "t" => Ok(true),
        "f" => Ok(false),
        _ => Err(NativeRunError::terminal("RESTORE_TARGET_PROBE_INVALID")),
    }
}

fn parse_mysql_bool(value: &str) -> Result<bool, NativeRunError> {
    match value.trim() {
        "1" => Ok(true),
        "0" => Ok(false),
        _ => Err(NativeRunError::terminal("RESTORE_TARGET_PROBE_INVALID")),
    }
}

fn parse_pg_target_probe(value: &str) -> Result<(String, String, bool), NativeRunError> {
    let line = value.trim();
    if line.is_empty() || line.contains(['\r', '\n']) {
        return Err(NativeRunError::terminal("RESTORE_TARGET_PROBE_INVALID"));
    }
    let mut fields = line.split('\t');
    let version = fields.next().unwrap_or_default().trim();
    let raw_identity = fields.next().unwrap_or_default().trim();
    let exists = fields.next().unwrap_or_default();
    if version.is_empty() || fields.next().is_some() {
        return Err(NativeRunError::terminal("RESTORE_TARGET_PROBE_INVALID"));
    }
    let parsed_identity = raw_identity
        .parse::<u64>()
        .ok()
        .filter(|parsed| *parsed != 0 && parsed.to_string() == raw_identity)
        .ok_or_else(|| NativeRunError::terminal("RESTORE_TARGET_IDENTITY_INVALID"))?;
    Ok((
        version.to_owned(),
        format!("postgresql-system-id:{parsed_identity}"),
        parse_pg_bool(exists)?,
    ))
}

fn parse_mysql_target_probe(
    client_kind: ClientKind,
    value: &str,
) -> Result<(String, String, bool), NativeRunError> {
    let line = value.trim();
    if line.is_empty() || line.contains(['\r', '\n']) {
        return Err(NativeRunError::terminal("RESTORE_TARGET_PROBE_INVALID"));
    }
    let mut fields = line.split('\t');
    let version = fields.next().unwrap_or_default().trim();
    let raw_identity = fields.next().unwrap_or_default().trim();
    let exists = fields.next().unwrap_or_default();
    if version.is_empty() || fields.next().is_some() {
        return Err(NativeRunError::terminal("RESTORE_TARGET_PROBE_INVALID"));
    }
    let identity = match client_kind {
        ClientKind::OracleMySql => {
            let parsed = uuid::Uuid::parse_str(raw_identity)
                .ok()
                .filter(|parsed| !parsed.is_nil())
                .ok_or_else(|| NativeRunError::terminal("RESTORE_TARGET_IDENTITY_INVALID"))?;
            format!("mysql-server-uuid:{}", parsed.hyphenated())
        }
        ClientKind::MariaDb => {
            if !(20..=64).contains(&raw_identity.len())
                || !raw_identity
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
            {
                return Err(NativeRunError::terminal("RESTORE_TARGET_IDENTITY_INVALID"));
            }
            format!("mariadb-server-uid:{raw_identity}")
        }
    };
    Ok((version.to_owned(), identity, parse_mysql_bool(exists)?))
}

fn guarded_pg_create_database_sql(
    target: &str,
    server_identity: &str,
) -> Result<String, NativeRunError> {
    let raw_identity = server_identity
        .strip_prefix("postgresql-system-id:")
        .and_then(|value| {
            value
                .parse::<u64>()
                .ok()
                .filter(|parsed| *parsed != 0 && parsed.to_string() == value)
        })
        .ok_or_else(|| NativeRunError::terminal("RESTORE_TARGET_IDENTITY_INVALID"))?;
    let create = format!("CREATE DATABASE {}", pg_ident(target));
    Ok(format!(
        "SELECT CASE WHEN ((SELECT system_identifier::text FROM pg_control_system()) = '{}' AND NOT EXISTS(SELECT 1 FROM pg_database WHERE datname = '{}')) THEN '{}' ELSE 'SELECT FROM RESTOREPLANE_RESTORE_GUARD_REJECTED' END\n\\gexec\n",
        raw_identity,
        sql_literal(target),
        sql_literal(&create)
    ))
}

fn guarded_mysql_create_database_sql(
    target: &str,
    server_identity: &str,
    client_kind: ClientKind,
) -> Result<String, NativeRunError> {
    let (prefix, identity_expression) = match client_kind {
        ClientKind::OracleMySql => ("mysql-server-uuid:", "@@GLOBAL.server_uuid"),
        ClientKind::MariaDb => ("mariadb-server-uid:", "@@GLOBAL.server_uid"),
    };
    let raw_identity = server_identity
        .strip_prefix(prefix)
        .filter(|identity| !identity.is_empty())
        .ok_or_else(|| NativeRunError::terminal("RESTORE_TARGET_IDENTITY_INVALID"))?;
    let create = format!(
        "CREATE DATABASE {} CHARACTER SET utf8mb4",
        mysql_ident(target)
    );
    Ok(format!(
        "SET @restoreplane_restore_sql = IF({identity_expression} = '{}' AND NOT EXISTS(SELECT 1 FROM information_schema.schemata WHERE schema_name = '{}'), '{}', 'SELECT FROM RESTOREPLANE_RESTORE_GUARD_REJECTED'); PREPARE restoreplane_restore_stmt FROM @restoreplane_restore_sql; EXECUTE restoreplane_restore_stmt; DEALLOCATE PREPARE restoreplane_restore_stmt",
        sql_literal(raw_identity),
        sql_literal(target),
        sql_literal(&create)
    ))
}

fn pg_connection_args(
    profile: &ConnectionExecutionBinding,
    credentials: &ResolvedCredentials,
    database: &str,
) -> Result<Vec<OsString>, NativeRunError> {
    let host = profile
        .host()
        .filter(|host| !host.trim().is_empty())
        .ok_or_else(|| NativeRunError::terminal("RESTORE_TARGET_BINDING_INVALID"))?;
    let port = profile
        .port()
        .ok_or_else(|| NativeRunError::terminal("RESTORE_TARGET_BINDING_INVALID"))?;
    Ok(vec![
        OsString::from(format!("--host={host}")),
        OsString::from(format!("--port={port}")),
        OsString::from(format!("--username={}", credentials.username.as_str())),
        OsString::from(format!("--dbname={database}")),
        OsString::from("--no-password"),
    ])
}

fn pg_environment(
    profile: &ConnectionExecutionBinding,
    credentials: &ResolvedCredentials,
) -> Result<Vec<(OsString, OsString)>, NativeRunError> {
    let ssl_mode = match profile.tls_mode() {
        "disabled" => "disable",
        "preferred" => "prefer",
        "required" => "require",
        "verify_ca" => "verify-ca",
        "verify_full" => "verify-full",
        _ => return Err(NativeRunError::terminal("RESTORE_TLS_MODE_UNSUPPORTED")),
    };
    Ok(vec![
        (
            OsString::from("PGPASSWORD"),
            OsString::from(credentials.password.as_str()),
        ),
        (OsString::from("PGSSLMODE"), OsString::from(ssl_mode)),
    ])
}

fn mysql_connection_args(
    profile: &ConnectionExecutionBinding,
    credentials: &ResolvedCredentials,
    database: Option<&str>,
    client_kind: ClientKind,
) -> Result<Vec<OsString>, NativeRunError> {
    let host = profile
        .host()
        .filter(|host| !host.trim().is_empty())
        .ok_or_else(|| NativeRunError::terminal("RESTORE_TARGET_BINDING_INVALID"))?;
    let port = profile
        .port()
        .ok_or_else(|| NativeRunError::terminal("RESTORE_TARGET_BINDING_INVALID"))?;
    let mut args = vec![
        OsString::from(format!("--host={host}")),
        OsString::from(format!("--port={port}")),
        OsString::from(format!("--user={}", credentials.username.as_str())),
    ];
    args.extend(
        tls_args(client_kind, profile.tls_mode())
            .map_err(|_| NativeRunError::terminal("RESTORE_TLS_MODE_UNSUPPORTED"))?
            .into_iter()
            .map(OsString::from),
    );
    if let Some(database) = database {
        args.push(OsString::from(format!("--database={database}")));
    }
    Ok(args)
}

fn mysql_environment(credentials: &ResolvedCredentials) -> Vec<(OsString, OsString)> {
    vec![(
        OsString::from("MYSQL_PWD"),
        OsString::from(credentials.password.as_str()),
    )]
}

fn valid_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    value.len() <= 128
        && (first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$'))
}

fn pg_ident(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn mysql_ident(value: &str) -> String {
    format!("`{}`", value.replace('`', "``"))
}

fn sql_literal(value: &str) -> String {
    value.replace('\'', "''")
}

fn windows_time_to_unix_ms(value_100ns: u64) -> Option<i64> {
    const WINDOWS_TO_UNIX_EPOCH_100NS: u64 = 116_444_736_000_000_000;
    value_100ns
        .checked_sub(WINDOWS_TO_UNIX_EPOCH_100NS)
        .and_then(|value| value.checked_div(10_000))
        .and_then(|value| i64::try_from(value).ok())
}

fn hex_sha256(value: &[u8; 32]) -> String {
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}

const fn engine_name(engine: Engine) -> &'static str {
    match engine {
        Engine::Postgresql => "postgresql",
        Engine::Mysql => "mysql",
        Engine::Mariadb => "mariadb",
        Engine::Sqlserver => "sqlserver",
        Engine::Sqlite => "sqlite",
    }
}

fn stable_capacity_id(kind: &str, key: &str) -> String {
    let digest = blake3::hash(format!("restoreplane-capacity\0{kind}\0{key}").as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest.as_bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}

fn normalized_spool_key(root: &Path) -> Result<String, RestoreExecutionError> {
    let canonical = std::fs::canonicalize(root)
        .map_err(|_| restore_error("RESTORE_SPOOL_UNAVAILABLE", true, None))?;
    let normalized = canonical.to_string_lossy().replace('\\', "/");
    if normalized.is_empty() || normalized.len() > 4_096 {
        return Err(restore_error(
            "RESTORE_CAPACITY_POLICY_INVALID",
            false,
            None,
        ));
    }
    Ok(if cfg!(windows) {
        normalized.to_lowercase()
    } else {
        normalized
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_stop_join_shares_one_explicit_budget() {
        let (_stop_sender, stop_receiver) = mpsc::sync_channel::<()>(1);
        let (stopped_sender, stopped_receiver) = mpsc::sync_channel(1);
        let thread = thread::spawn(move || {
            let _ = stopped_sender.send(());
            thread::sleep(Duration::from_millis(200));
            drop(stop_receiver);
        });
        let stopped = Mutex::new(Some(stopped_receiver));
        let thread = Mutex::new(Some(thread));

        let started = Instant::now();
        let error = wait_for_runtime_thread(&stopped, &thread, Duration::from_millis(20))
            .expect_err("premature stop signal cannot permit an unbounded join");

        assert_eq!(error.code(), "SERVICE_RUNTIME_FAILED");
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn invalid_artifact_format_is_rejected_by_pre_destructive_contract() {
        assert!(!restore_dump_format_supported(Engine::Postgresql, "plain"));
        assert!(!restore_dump_format_supported(Engine::Mysql, "custom"));
        assert!(!restore_dump_format_supported(Engine::Mariadb, "custom"));
        assert!(!restore_dump_format_supported(Engine::Sqlite, "plain"));
        assert!(!restore_dump_format_supported(Engine::Sqlserver, "native"));
        for invalid in [
            "SQLITE_DATABASE",
            "sqlite__database",
            "sqlite database",
            "sqlite_database ",
            "sqlite",
            "database",
        ] {
            assert!(!restore_dump_format_supported(Engine::Sqlite, invalid));
        }
        for engine in [
            Engine::Postgresql,
            Engine::Mysql,
            Engine::Mariadb,
            Engine::Sqlserver,
        ] {
            assert!(!restore_dump_format_supported(engine, "sqlite_database"));
            assert!(!restore_dump_format_supported(engine, "sqlite-database"));
        }
    }

    #[test]
    fn native_backup_canonical_database_formats_and_exact_sqlite_legacy_alias_are_supported() {
        // NativeBackupExecutor signs normalize_format(plan.dump_format): the
        // SQLite provider's sqlite-database becomes sqlite_database. Network
        // database parts already produce custom/plain, so those stay exact.
        assert!(restore_dump_format_supported(
            Engine::Sqlite,
            "sqlite_database"
        ));
        assert!(restore_dump_format_supported(
            Engine::Sqlite,
            "sqlite-database"
        ));
        assert!(restore_dump_format_supported(Engine::Postgresql, "custom"));
        for engine in [Engine::Mysql, Engine::Mariadb] {
            assert!(restore_dump_format_supported(engine, "plain"));
            assert!(restore_dump_format_supported(engine, "sql"));
        }
    }

    #[test]
    fn current_pg_restore_compatibility_accepts_supported_historical_dump() {
        validate_tool_compatibility(Engine::Postgresql, "custom", "14.11", "18.2")
            .expect("current bundle restores a supported historical pg dump");

        let incompatible =
            validate_tool_compatibility(Engine::Postgresql, "custom", "18.1", "17.6")
                .expect_err("older restore tool must fail closed");
        assert_eq!(incompatible.code, "RESTORE_TOOL_COMPATIBILITY_UNSUPPORTED");
    }

    #[test]
    fn mysql_family_compatibility_uses_distribution_not_client_protocol_version() {
        validate_tool_compatibility(
            Engine::Mariadb,
            "plain",
            "mariadb-dump Ver 10.19 Distrib 11.8.3-MariaDB",
            "mariadb.exe Ver 15.1 Distrib 11.8.3-MariaDB",
        )
        .expect("MariaDB distribution major is the compatibility axis");
        validate_tool_compatibility(
            Engine::Mysql,
            "plain",
            "mysqldump Ver 10.13 Distrib 8.4.11",
            "mysql.exe Ver 8.0 Distrib 8.4.11",
        )
        .expect("Oracle MySQL distribution major is the compatibility axis");
    }

    #[test]
    fn retryable_native_failure_after_destructive_fence_becomes_terminal_cleanup() {
        let mapped = restore_native_error(NativeRunError {
            code: "RESTORE_PROCESS_TIMEOUT",
            retryable: true,
            target_created: true,
        });

        assert!(!mapped.is_retryable());
        assert!(mapped.target_created());
        assert_eq!(mapped.code(), "RESTORE_PROCESS_TIMEOUT");
    }

    #[test]
    fn network_overwrite_fails_closed_without_a_pinned_live_server_identity() {
        let error = reject_unbound_network_overwrite_policy("overwrite")
            .expect_err("unbound network overwrite must never reach DDL");
        assert_eq!(error.code, "RESTORE_OVERWRITE_IDENTITY_UNAVAILABLE");
        reject_unbound_network_overwrite_policy("create_new")
            .expect("create-new remains protected by the live absence probe");
    }

    #[test]
    fn target_existence_probe_rejects_ambiguous_native_output() {
        assert!(parse_pg_bool("t\n").expect("canonical PostgreSQL true"));
        assert!(!parse_mysql_bool("0\r\n").expect("canonical MySQL false"));
        assert_eq!(
            parse_pg_bool("permission denied")
                .expect_err("ambiguous PostgreSQL output")
                .code,
            "RESTORE_TARGET_PROBE_INVALID"
        );
        assert_eq!(
            parse_mysql_bool("").expect_err("missing MySQL output").code,
            "RESTORE_TARGET_PROBE_INVALID"
        );
    }

    #[test]
    fn native_target_probe_requires_canonical_strong_server_identity() {
        assert_eq!(
            parse_pg_target_probe("PostgreSQL 17.4\t7541424833265539027\tf\n")
                .expect("canonical PostgreSQL target probe"),
            (
                "PostgreSQL 17.4".to_owned(),
                "postgresql-system-id:7541424833265539027".to_owned(),
                false
            )
        );
        assert_eq!(
            parse_mysql_target_probe(
                ClientKind::OracleMySql,
                "8.4.5\t550e8400-e29b-41d4-a716-446655440000\t0\n"
            )
            .expect("canonical MySQL target probe")
            .1,
            "mysql-server-uuid:550e8400-e29b-41d4-a716-446655440000"
        );
        assert!(parse_pg_target_probe("PostgreSQL 17.4\tserver-a\tf").is_err());
        assert!(parse_mysql_target_probe(ClientKind::MariaDb, "11.8\tweak host\t0").is_err());
    }

    #[test]
    fn guarded_create_sql_rechecks_identity_and_absence_in_the_ddl_session() {
        let pg = guarded_pg_create_database_sql(
            "orders_restore",
            "postgresql-system-id:7541424833265539027",
        )
        .expect("PostgreSQL guard SQL");
        assert!(pg.contains("pg_control_system"));
        assert!(pg.contains("7541424833265539027"));
        assert!(pg.contains("NOT EXISTS"));
        assert!(pg.contains("CREATE DATABASE \"orders_restore\""));

        let mysql = guarded_mysql_create_database_sql(
            "orders_restore",
            "mysql-server-uuid:550e8400-e29b-41d4-a716-446655440000",
            ClientKind::OracleMySql,
        )
        .expect("MySQL guard SQL");
        assert!(mysql.contains("@@GLOBAL.server_uuid"));
        assert!(mysql.contains("NOT EXISTS"));
        assert!(mysql.contains("PREPARE restoreplane_restore_stmt"));
        assert!(mysql.contains("CREATE DATABASE `orders_restore`"));
    }

    #[test]
    fn content_observation_requires_data_dependent_native_checksums() {
        assert_eq!(
            parse_pg_content_observation("3\t-42\t18446744073709551574\n")
                .expect("canonical PostgreSQL observation"),
            "3\t-42\t18446744073709551574"
        );
        assert_eq!(
            parse_mysql_checksum("orders.items\t3918507938\r\n").expect("canonical MySQL checksum"),
            3_918_507_938
        );
        assert_eq!(
            parse_mysql_checksum("orders.items\tNULL")
                .expect_err("unsupported native checksums fail closed")
                .code,
            "RESTORE_CONTENT_ORACLE_INVALID"
        );
        assert_eq!(
            parse_pg_content_observation("3\t-0\t0")
                .expect_err("non-canonical content evidence")
                .code,
            "RESTORE_CONTENT_ORACLE_INVALID"
        );
    }

    #[test]
    fn absolute_restore_window_stops_before_blocking_provider_work() {
        let stopping = AtomicBool::new(false);
        let timeout = ensure_execution_window(
            &stopping,
            Instant::now()
                .checked_sub(Duration::from_millis(1))
                .expect("past deadline"),
        )
        .expect_err("expired claim must not enter provider work");
        assert_eq!(timeout.code, "RESTORE_EXECUTION_TIMEOUT");

        stopping.store(true, Ordering::Release);
        let stopped = ensure_execution_window(&stopping, Instant::now() + Duration::from_secs(1))
            .expect_err("service stop must not enter provider work");
        assert_eq!(stopped.code, "RESTORE_SERVICE_STOPPING");
    }

    #[test]
    fn legacy_mysql_family_l2_is_terminal_before_reader_resolution() {
        assert_eq!(
            verification_level_preflight_error("mysql", 2),
            Some("VERIFY_L2_NATIVE_UNSUPPORTED")
        );
        assert_eq!(
            verification_level_preflight_error("mariadb", 2),
            Some("VERIFY_L2_NATIVE_UNSUPPORTED")
        );
        assert_eq!(verification_level_preflight_error("postgresql", 2), None);
        assert_eq!(verification_level_preflight_error("mysql", 1), None);
    }
}
#[cfg(all(test, windows))]
mod workbench_restore_chain;
