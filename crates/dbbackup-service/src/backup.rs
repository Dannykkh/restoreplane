use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dbbackup_catalog::{
    AdmissionOutcome, AdmissionRequest, BackupClaimContext, BeginCommitDisposition,
    ClaimCompletionSpec, ClaimRequest, ClaimTerminalOutcome, CompleteRecoveryPointSpec,
    OperationalStore, ProcessExitSpec, ProcessStartSpec, RepositoryError, ResourcePoolSpec,
    ResourceRequirement, RetryDisposition, RuntimeRepository, SpoolRequirement, SpoolVolumeSpec,
};
use dbbackup_core::{
    ArtifactPartRole, BackupScopeSelection, ConsistencyGrade, Engine, PartSpec, SelectedTable,
};
use dbbackup_domain::{DestinationRequirement, RecoveryPointId, WorkItemId, WorkItemKind};
use dbbackup_pipeline::{
    preflight_disk_guard, publish_set_manifest_v2, run_artifact_with_cancellation, BackupSource,
    DumpArtifact, EncodedPartSourceV2, EncodingEvidenceV2, ManifestPartV2, NativeToolIdentityV2,
    PipelineCancellationToken, PipelineCfg, ReaderStatusV2, RecoveryOriginV2, ReplicaCommitmentV2,
    ReplicaEndpointV2, ReplicaRequirementV2, SetManifestV2, SignedSetManifestV2,
    SourceManifestEvidence, SpoolCommit,
};
use dbbackup_secrets::{wrap_dek_with_kek, Dek, WrapSet};
use dbbackup_source_mysql::binary::{
    bin_name_for, resolve_bundled_metadata_client, tool_name_for, ClientKind, MysqldumpBinaryMatrix,
};
use dbbackup_source_mysql::dump::MySqlConnCfg;
use dbbackup_source_mysql::meta::MysqlCliMetaConn;
use dbbackup_source_mysql::version::ServerKind;
use dbbackup_source_mysql::{MySqlSource, MySqlSourceError};
use dbbackup_source_pg::version::{bin_name as pg_bin_name, BundledToolMatrix};
use dbbackup_source_pg::{PgConnCfg, PostgresSource};
use dbbackup_source_sqlite::{BackupCancellationToken, SqliteSource};
use ed25519_dalek::SigningKey;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::host::{ClaimRuntime, HostError, ServiceHealthProbe};
use crate::operations::destination::resolve_backup_replica;
use crate::secrets::{SecretPurpose, ServiceSecretStore};
use crate::trusted_tool::verify_trusted_bundled_tool;

const LEASE_DURATION_MS: i64 = 30_000;
const RETRY_DELAY_MS: i64 = 1_000;
const BUSY_DELAY_MS: i64 = 500;
const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(250);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const DEFAULT_WORKER_THREADS: usize = 4;
const DEK_WRAP_GENERATION: u32 = 1;
const DEFAULT_EXECUTION_TIMEOUT_MS: i64 = 6 * 60 * 60 * 1_000;
const DEFAULT_MIN_FREE_DISK_BYTES: u64 = 2 * 1_024 * 1_024 * 1_024;
const DEFAULT_SPOOL_RESERVATION_BYTES: i64 = 512 * 1_024 * 1_024;
const DEFAULT_DESTINATION_CONCURRENCY: i64 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeBackupLimits {
    execution_timeout_ms: i64,
    min_free_disk_bytes: u64,
    spool_reservation_bytes: i64,
    global_concurrency: i64,
    destination_concurrency: i64,
}

impl NativeBackupLimits {
    pub const fn new(
        execution_timeout_ms: i64,
        min_free_disk_bytes: u64,
        spool_reservation_bytes: i64,
        global_concurrency: i64,
        destination_concurrency: i64,
    ) -> Self {
        Self {
            execution_timeout_ms,
            min_free_disk_bytes,
            spool_reservation_bytes,
            global_concurrency,
            destination_concurrency,
        }
    }

    fn validate(self) -> Result<Self, BackupWorkerError> {
        if self.execution_timeout_ms < 1_000
            || self.min_free_disk_bytes > i64::MAX as u64
            || self.spool_reservation_bytes <= 0
            || self.global_concurrency <= 0
            || self.destination_concurrency <= 0
        {
            return Err(BackupWorkerError::ConfigInvalid);
        }
        Ok(self)
    }
}

impl Default for NativeBackupLimits {
    fn default() -> Self {
        Self::new(
            DEFAULT_EXECUTION_TIMEOUT_MS,
            DEFAULT_MIN_FREE_DISK_BYTES,
            DEFAULT_SPOOL_RESERVATION_BYTES,
            DEFAULT_WORKER_THREADS as i64,
            DEFAULT_DESTINATION_CONCURRENCY,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackupWorkerTick {
    Idle,
    Deferred(WorkItemId),
    AwaitingReconciliation(WorkItemId),
    Completed(WorkItemId),
    PartiallyCompleted(WorkItemId),
    Retried(WorkItemId),
    Failed(WorkItemId),
    Cancelled(WorkItemId),
}

#[derive(Debug, Error)]
pub enum BackupWorkerError {
    #[error("BACKUP_WORKER_REPOSITORY")]
    Repository(#[from] RepositoryError),
    #[error("BACKUP_WORKER_SECRET_STORE")]
    SecretStore(#[from] crate::secrets::SecretStoreError),
    #[error("BACKUP_WORKER_KEY_INVALID")]
    KeyInvalid,
    #[error("BACKUP_WORKER_CONFIG_INVALID")]
    ConfigInvalid,
    #[error("BACKUP_WORKER_SPOOL_IO")]
    SpoolIo(#[source] std::io::Error),
}

impl BackupWorkerError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Repository(error) => error.code(),
            Self::SecretStore(error) => error.code(),
            Self::KeyInvalid => "BACKUP_WORKER_KEY_INVALID",
            Self::ConfigInvalid => "BACKUP_WORKER_CONFIG_INVALID",
            Self::SpoolIo(_) => "BACKUP_WORKER_SPOOL_IO",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpoolScavengeReport {
    removed_attempts: usize,
    preserved_active: usize,
    skipped_unmanaged: usize,
}

impl SpoolScavengeReport {
    pub const fn removed_attempts(&self) -> usize {
        self.removed_attempts
    }

    pub const fn preserved_active(&self) -> usize {
        self.preserved_active
    }

    pub const fn skipped_unmanaged(&self) -> usize {
        self.skipped_unmanaged
    }
}

#[derive(Debug, Error)]
pub enum BackupWorkerStartError {
    #[error("BACKUP_WORKER_INITIALIZATION_FAILED")]
    Initialization(#[from] BackupWorkerError),
    #[error("BACKUP_WORKER_THREAD_SPAWN_FAILED")]
    Spawn(#[source] std::io::Error),
}

impl BackupWorkerStartError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Initialization(error) => error.code(),
            Self::Spawn(_) => "BACKUP_WORKER_THREAD_SPAWN_FAILED",
        }
    }
}

pub struct NativeBackupExecutor {
    store: OperationalStore,
    secrets: Arc<ServiceSecretStore>,
    install_root: PathBuf,
    spool_root: PathBuf,
    instance_id: String,
    signing_key_id: String,
    encryption_key_id: String,
    signing_key: SigningKey,
    encryption_key: Dek,
    limits: NativeBackupLimits,
    clock: fn() -> i64,
}

impl NativeBackupExecutor {
    pub fn new(
        store: OperationalStore,
        secrets: Arc<ServiceSecretStore>,
        install_root: PathBuf,
        spool_root: PathBuf,
        instance_id: impl Into<String>,
    ) -> Result<Self, BackupWorkerError> {
        Self::with_clock(
            store,
            secrets,
            install_root,
            spool_root,
            instance_id,
            unix_time_ms,
        )
    }

    pub fn with_clock(
        store: OperationalStore,
        secrets: Arc<ServiceSecretStore>,
        install_root: PathBuf,
        spool_root: PathBuf,
        instance_id: impl Into<String>,
        clock: fn() -> i64,
    ) -> Result<Self, BackupWorkerError> {
        Self::with_clock_and_limits(
            store,
            secrets,
            install_root,
            spool_root,
            instance_id,
            clock,
            NativeBackupLimits::default(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_clock_and_limits(
        store: OperationalStore,
        secrets: Arc<ServiceSecretStore>,
        install_root: PathBuf,
        spool_root: PathBuf,
        instance_id: impl Into<String>,
        clock: fn() -> i64,
        limits: NativeBackupLimits,
    ) -> Result<Self, BackupWorkerError> {
        let instance_id = instance_id.into();
        let now_ms = clock();
        if instance_id.trim().is_empty() || now_ms < 0 {
            return Err(BackupWorkerError::ConfigInvalid);
        }
        let limits = limits.validate()?;
        std::fs::create_dir_all(&spool_root).map_err(|_| BackupWorkerError::ConfigInvalid)?;
        let (signing_ref, signing_seed) =
            secrets.load_or_create_service_key(SecretPurpose::SigningKey, now_ms)?;
        let (encryption_ref, encryption_key) =
            secrets.load_or_create_service_key(SecretPurpose::EncryptionKey, now_ms)?;
        let signing_seed: [u8; 32] = signing_seed
            .expose()
            .try_into()
            .map_err(|_| BackupWorkerError::KeyInvalid)?;
        let encryption_key: [u8; 32] = encryption_key
            .expose()
            .try_into()
            .map_err(|_| BackupWorkerError::KeyInvalid)?;
        let executor = Self {
            store,
            secrets,
            install_root,
            spool_root,
            instance_id,
            signing_key_id: signing_ref.id().to_string(),
            encryption_key_id: encryption_ref.id().to_string(),
            signing_key: SigningKey::from_bytes(&signing_seed),
            encryption_key: Dek::from_bytes(encryption_key),
            limits,
            clock,
        };
        executor.observe_base_capacity(now_ms)?;
        Ok(executor)
    }

    pub fn signing_key_id(&self) -> &str {
        &self.signing_key_id
    }

    pub fn verifying_key(&self) -> ed25519_dalek::VerifyingKey {
        self.signing_key.verifying_key()
    }

    fn observe_base_capacity(&self, observed_at_ms: i64) -> Result<(), BackupWorkerError> {
        let runtime = self.store.runtime();
        runtime.upsert_resource_pool(ResourcePoolSpec::new(
            stable_capacity_id("global-job", "backup-service"),
            "global_job",
            "backup-service",
            self.limits.global_concurrency,
            observed_at_ms,
        ))?;
        let free_bytes =
            fs2::available_space(&self.spool_root).map_err(BackupWorkerError::SpoolIo)?;
        let free_bytes = i64::try_from(free_bytes).unwrap_or(i64::MAX);
        let volume_key =
            normalized_spool_key(&self.spool_root).map_err(|_| BackupWorkerError::ConfigInvalid)?;
        runtime.upsert_spool_volume(SpoolVolumeSpec::ready(
            stable_capacity_id("spool", &volume_key),
            volume_key,
            self.spool_root.to_string_lossy(),
            free_bytes.max(1),
            free_bytes.max(1),
            i64::try_from(self.limits.min_free_disk_bytes)
                .map_err(|_| BackupWorkerError::ConfigInvalid)?,
            free_bytes.max(0),
            observed_at_ms,
        ))?;
        Ok(())
    }

    pub fn scavenge_orphaned_spool(&self) -> Result<SpoolScavengeReport, BackupWorkerError> {
        const ACTIVE_PATH_LIMIT: usize = 10_000;
        let paths = self.store.runtime().active_spool_paths(ACTIVE_PATH_LIMIT)?;
        if paths.len() == ACTIVE_PATH_LIMIT {
            return Err(BackupWorkerError::ConfigInvalid);
        }
        let active = paths
            .into_iter()
            .filter_map(|path| std::fs::canonicalize(path).ok())
            .collect();
        scavenge_managed_spool(&self.spool_root, &active).map_err(BackupWorkerError::SpoolIo)
    }

    fn work_spool(&self, claim: &dbbackup_catalog::ClaimedWork) -> PathBuf {
        self.spool_root
            .join(claim.work_item_id().as_str())
            .join(format!(
                "attempt-{}-{}",
                claim.attempt_no(),
                claim.fencing_token()
            ))
    }

    fn admit_claim(
        &self,
        claim: &dbbackup_catalog::ClaimedWork,
        context: &BackupClaimContext,
        now_ms: i64,
    ) -> Result<AdmissionOutcome, BackupExecutionFailure> {
        let free_bytes = fs2::available_space(&self.spool_root)
            .map_err(|_| BackupExecutionFailure::retryable("BACKUP_SPOOL_PROBE_FAILED"))?;
        let free_bytes_i64 = i64::try_from(free_bytes).unwrap_or(i64::MAX);
        let hard_limit_bytes = free_bytes_i64.max(1);
        let soft_limit_bytes = hard_limit_bytes;
        let reserve_floor_bytes = i64::try_from(self.limits.min_free_disk_bytes)
            .map_err(|_| BackupExecutionFailure::terminal("BACKUP_CAPACITY_POLICY_INVALID"))?;
        let runtime = self.store.runtime();

        let global_pool_id = runtime
            .upsert_resource_pool(ResourcePoolSpec::new(
                stable_capacity_id("global-job", "backup-service"),
                "global_job",
                "backup-service",
                self.limits.global_concurrency,
                now_ms,
            ))
            .map_err(|_| BackupExecutionFailure::retryable("BACKUP_ADMISSION_UNAVAILABLE"))?;
        let source_key = context.connection_profile_id().to_string();
        let source_pool_id = runtime
            .upsert_resource_pool(ResourcePoolSpec::new(
                stable_capacity_id("source", &source_key),
                "source",
                source_key.clone(),
                1,
                now_ms,
            ))
            .map_err(|_| BackupExecutionFailure::retryable("BACKUP_ADMISSION_UNAVAILABLE"))?;
        let mut resources = vec![
            ResourceRequirement::new(global_pool_id, 1),
            ResourceRequirement::new(source_pool_id, 1),
        ];
        let destination_bindings = self
            .store
            .configuration()
            .backup_destination_bindings(context.plan_revision_id())
            .map_err(|_| BackupExecutionFailure::terminal("BACKUP_DESTINATION_INVALID"))?;
        for destination in destination_bindings {
            let destination_key = destination.destination_id().to_string();
            let pool_id = runtime
                .upsert_resource_pool(ResourcePoolSpec::new(
                    stable_capacity_id("destination", &destination_key),
                    "destination",
                    destination_key,
                    self.limits.destination_concurrency,
                    now_ms,
                ))
                .map_err(|_| BackupExecutionFailure::retryable("BACKUP_ADMISSION_UNAVAILABLE"))?;
            resources.push(ResourceRequirement::new(pool_id, 1));
        }
        let volume_key = normalized_spool_key(&self.spool_root)?;
        let spool_volume_id = runtime
            .upsert_spool_volume(SpoolVolumeSpec::ready(
                stable_capacity_id("spool", &volume_key),
                volume_key,
                self.spool_root.to_string_lossy(),
                hard_limit_bytes,
                soft_limit_bytes,
                reserve_floor_bytes,
                free_bytes_i64,
                now_ms,
            ))
            .map_err(|_| BackupExecutionFailure::retryable("BACKUP_ADMISSION_UNAVAILABLE"))?;
        runtime
            .admit_claim(AdmissionRequest::new(
                claim.clone(),
                resources,
                Some(SpoolRequirement::new(
                    spool_volume_id,
                    self.limits.spool_reservation_bytes,
                    self.work_spool(claim).to_string_lossy(),
                )),
                now_ms,
            ))
            .map_err(|_| BackupExecutionFailure::retryable("BACKUP_ADMISSION_UNAVAILABLE"))
    }

    fn execute(
        &self,
        claim: &dbbackup_catalog::ClaimedWork,
        context: &BackupClaimContext,
        cancellation: ServiceBackupCancellation,
        deadline_ms: i64,
    ) -> Result<BackupExecution, BackupExecutionFailure> {
        let profile = self
            .store
            .configuration()
            .connection_execution_binding(context.connection_profile_id())
            .map_err(|_| BackupExecutionFailure::terminal("BACKUP_CONNECTION_INVALID"))?;
        let tls_mode = profile.tls_mode().to_owned();
        let source = self.build_source(context, &profile, cancellation.provider.clone())?;
        let destination_bindings = self
            .store
            .configuration()
            .backup_destination_bindings(context.plan_revision_id())
            .map_err(|_| BackupExecutionFailure::terminal("BACKUP_DESTINATION_INVALID"))?;
        let mut destinations = Vec::with_capacity(destination_bindings.len());
        for binding in destination_bindings {
            if binding.state() == "disabled" {
                return Err(BackupExecutionFailure::terminal(
                    "BACKUP_DESTINATION_DISABLED",
                ));
            }
            let (endpoint, immutability) =
                resolve_backup_replica(binding.destination(), Some(&self.secrets)).map_err(
                    |error| BackupExecutionFailure {
                        code: normalize_error_code(error.code()),
                        retryable: error.is_retryable(),
                        commit_started: false,
                        cancelled: false,
                    },
                )?;
            destinations.push(PreparedDestination {
                fingerprint: binding.fingerprint(),
                kind: binding.kind().to_owned(),
                requirement: map_requirement(binding.requirement()),
                immutability: immutability.to_owned(),
                endpoint,
            });
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| BackupExecutionFailure::retryable("BACKUP_RUNTIME_UNAVAILABLE"))?;
        runtime.block_on(self.execute_async(
            claim,
            context,
            source,
            destinations,
            tls_mode,
            cancellation,
            deadline_ms,
        ))
    }

    fn build_source(
        &self,
        context: &BackupClaimContext,
        profile: &dbbackup_catalog::ConnectionExecutionBinding,
        cancellation: BackupCancellationToken,
    ) -> Result<Box<dyn BackupSource>, BackupExecutionFailure> {
        let selection = parse_scope_selection(
            context.scope_snapshot_json(),
            context.scope_mode(),
            context.resolved_object_count(),
            context.scope_snapshot_hash_blake3(),
        )?;
        match context.engine() {
            "sqlite" => {
                if !selection.is_whole_database() {
                    return Err(BackupExecutionFailure::terminal("BACKUP_SCOPE_UNSUPPORTED"));
                }
                let path = profile
                    .file_path()
                    .ok_or_else(|| BackupExecutionFailure::terminal("BACKUP_ENDPOINT_INVALID"))?;
                Ok(Box::new(
                    SqliteSource::new(
                        context.backup_run_id().as_str(),
                        context.target_display_name(),
                        path,
                    )
                    .with_cancellation_token(cancellation),
                ))
            }
            "postgresql" => {
                let credentials = self.database_credentials(profile)?;
                let host = profile
                    .host()
                    .ok_or_else(|| BackupExecutionFailure::terminal("BACKUP_ENDPOINT_INVALID"))?;
                let port = profile
                    .port()
                    .ok_or_else(|| BackupExecutionFailure::terminal("BACKUP_ENDPOINT_INVALID"))?;
                let pg_bundle = self.install_root.join("tools").join("pg").join("pg18");
                let mut trusted_pg_tools = Vec::with_capacity(2);
                for tool_name in ["pg_dump", "pg_dumpall"] {
                    let verified = verify_trusted_bundled_tool(
                        &self.store,
                        &self.install_root,
                        tool_name,
                        &pg_bundle.join(pg_bin_name(tool_name)),
                    )
                    .map_err(|_| BackupExecutionFailure::terminal("BACKUP_TOOL_UNTRUSTED"))?;
                    trusted_pg_tools.push((
                        verified.canonical_path().to_path_buf(),
                        *verified.sha256(),
                        verified.dependencies().to_vec(),
                    ));
                }
                Ok(Box::new(
                    PostgresSource {
                        job: context.backup_run_id().to_string(),
                        alias: context.scope_display_name().to_owned(),
                        engine_alias: "postgresql".to_owned(),
                        conn: PgConnCfg {
                            host: host.to_owned(),
                            port,
                            user: credentials.username,
                            password: credentials.password,
                            admin_database: "postgres".to_owned(),
                            tls_mode: profile.tls_mode().to_owned(),
                        },
                        registered_databases: vec![context.target_display_name().to_owned()],
                        matrix: BundledToolMatrix::from_bundle_dir(
                            &self.install_root.join("tools").join("pg"),
                        )
                        .with_trusted_launch_identities(trusted_pg_tools),
                    }
                    .with_selection(selection),
                ))
            }
            "mysql" | "mariadb" => {
                if !matches!(profile.tls_mode(), "disabled" | "preferred" | "required") {
                    return Err(BackupExecutionFailure::terminal(
                        "BACKUP_TLS_MODE_UNSUPPORTED",
                    ));
                }
                let credentials = self.database_credentials(profile)?;
                let host = profile
                    .host()
                    .ok_or_else(|| BackupExecutionFailure::terminal("BACKUP_ENDPOINT_INVALID"))?;
                let port = profile
                    .port()
                    .ok_or_else(|| BackupExecutionFailure::terminal("BACKUP_ENDPOINT_INVALID"))?;
                let client_kind = if context.engine() == "mariadb" {
                    ClientKind::MariaDb
                } else {
                    ClientKind::OracleMySql
                };
                let mysql_bundle = self.install_root.join("tools").join("mysql");
                let dump_bundle = mysql_bundle.join(if client_kind == ClientKind::MariaDb {
                    "mariadb11"
                } else {
                    "mysql8"
                });
                let verified_dump_tool = verify_trusted_bundled_tool(
                    &self.store,
                    &self.install_root,
                    tool_name_for(client_kind),
                    &dump_bundle.join(bin_name_for(client_kind)),
                )
                .map_err(|_| BackupExecutionFailure::terminal("BACKUP_TOOL_UNTRUSTED"))?;
                let metadata_client =
                    resolve_mysql_metadata_client(&self.install_root, client_kind)?;
                let verified_metadata_client = verify_trusted_bundled_tool(
                    &self.store,
                    &self.install_root,
                    if client_kind == ClientKind::MariaDb {
                        "mariadb"
                    } else {
                        "mysql"
                    },
                    &metadata_client,
                )
                .map_err(|_| BackupExecutionFailure::terminal("BACKUP_TOOL_UNTRUSTED"))?;
                let meta = MysqlCliMetaConn {
                    client: verified_metadata_client.canonical_path().to_path_buf(),
                    expected_sha256: Some(*verified_metadata_client.sha256()),
                    trusted_dependencies: verified_metadata_client.dependencies().to_vec(),
                    host: host.to_owned(),
                    port,
                    user: credentials.username.clone(),
                    password: credentials.password.clone(),
                    client_kind,
                    tls_mode: profile.tls_mode().to_owned(),
                };
                Ok(Box::new(
                    MySqlSource::new(
                        context.backup_run_id().to_string(),
                        context.scope_display_name().to_owned(),
                        Box::new(meta),
                        MySqlConnCfg {
                            host: host.to_owned(),
                            port,
                            user: credentials.username,
                            password: credentials.password,
                            database: context.target_display_name().to_owned(),
                            tls_mode: profile.tls_mode().to_owned(),
                        },
                        MysqldumpBinaryMatrix::from_bundle_dir(
                            &self.install_root.join("tools").join("mysql"),
                        )
                        .with_trusted_launch_identities([(
                            verified_dump_tool.canonical_path().to_path_buf(),
                            *verified_dump_tool.sha256(),
                            verified_dump_tool.dependencies().to_vec(),
                        )]),
                        true,
                    )
                    .with_expected_server_kind(if client_kind == ClientKind::MariaDb {
                        ServerKind::MariaDb
                    } else {
                        ServerKind::OracleMySql
                    })
                    .with_selection(selection),
                ))
            }
            _ => Err(BackupExecutionFailure::terminal(
                "BACKUP_ENGINE_UNSUPPORTED",
            )),
        }
    }

    fn database_credentials(
        &self,
        profile: &dbbackup_catalog::ConnectionExecutionBinding,
    ) -> Result<DatabaseCredentials, BackupExecutionFailure> {
        let id = profile
            .database_auth_secret_ref_id()
            .ok_or_else(|| BackupExecutionFailure::terminal("BACKUP_AUTH_REQUIRED"))?;
        let secret = self
            .secrets
            .resolve(id, SecretPurpose::DatabaseAuth)
            .map_err(|_| BackupExecutionFailure::terminal("BACKUP_AUTH_INVALID"))?;
        let stored: StoredDatabaseCredentials = serde_json::from_slice(secret.expose())
            .map_err(|_| BackupExecutionFailure::terminal("BACKUP_AUTH_INVALID"))?;
        if stored.username.trim().is_empty() || stored.password.is_empty() {
            return Err(BackupExecutionFailure::terminal("BACKUP_AUTH_INVALID"));
        }
        Ok(DatabaseCredentials {
            username: stored.username,
            password: Zeroizing::new(stored.password),
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_async(
        &self,
        claim: &dbbackup_catalog::ClaimedWork,
        context: &BackupClaimContext,
        source: Box<dyn BackupSource>,
        destinations: Vec<PreparedDestination>,
        tls_mode: String,
        cancellation: ServiceBackupCancellation,
        deadline_ms: i64,
    ) -> Result<BackupExecution, BackupExecutionFailure> {
        let remaining_ms = deadline_ms.saturating_sub((self.clock)());
        if remaining_ms <= 0 {
            cancellation.cancel_with(CancellationCause::ExecutionTimeout);
            return Err(BackupExecutionFailure::retryable(
                "BACKUP_EXECUTION_TIMEOUT",
            ));
        }
        self.execute_async_inner(
            claim,
            context,
            source,
            destinations,
            tls_mode,
            cancellation,
            deadline_ms,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_async_inner(
        &self,
        claim: &dbbackup_catalog::ClaimedWork,
        context: &BackupClaimContext,
        source: Box<dyn BackupSource>,
        destinations: Vec<PreparedDestination>,
        tls_mode: String,
        cancellation: ServiceBackupCancellation,
        deadline_ms: i64,
    ) -> Result<BackupExecution, BackupExecutionFailure> {
        self.store
            .runtime()
            .mark_in_process_claim_running(claim, (self.clock)())
            .map_err(|_| BackupExecutionFailure::retryable("BACKUP_PROCESS_RECORD_FAILED"))?;
        let plan_timeout = execution_time_remaining(deadline_ms, self.clock, &cancellation)?;
        let plan = plan_source_with_timeout(source.as_ref(), &cancellation, plan_timeout).await?;
        if plan.db_parts.is_empty() {
            return Err(BackupExecutionFailure::terminal(
                "BACKUP_DATABASE_NOT_FOUND",
            ));
        }
        let mut planned_parts = Vec::with_capacity(plan.db_parts.len() + 1);
        if let Some(globals) = plan.globals_part {
            planned_parts.push(globals);
        }
        planned_parts.extend(plan.db_parts);

        let work_spool = self.work_spool(claim);
        std::fs::create_dir_all(&work_spool)
            .map_err(|_| BackupExecutionFailure::retryable("BACKUP_SPOOL_CREATE_FAILED"))?;
        let mut cfg = PipelineCfg {
            min_free_disk: self.limits.min_free_disk_bytes,
            ..PipelineCfg::default()
        };
        if matches!(context.engine(), "mysql" | "mariadb") {
            cfg.fatal_patterns.extend(MySqlSource::fatal_patterns());
        }
        let mut prepared = Vec::with_capacity(planned_parts.len());
        for mut part in planned_parts {
            if cancellation.is_cancelled() {
                cleanup_prepared(&prepared, &work_spool);
                return Ok(BackupExecution::Cancelled);
            }
            if preflight_disk_guard(&work_spool, cfg.min_free_disk).is_err() {
                cleanup_prepared(&prepared, &work_spool);
                return Err(BackupExecutionFailure::retryable("BACKUP_SPOOL_DISK_LOW"));
            }
            let dek = Dek::generate();
            let wrap_set = wrap_dek_with_kek(&self.encryption_key, &dek, DEK_WRAP_GENERATION)
                .map_err(|_| BackupExecutionFailure::terminal("BACKUP_DEK_WRAP_FAILED"))?;
            let spool_path = work_spool.join(format!("{}.enc", part.backup_id));
            let tool_sha256 = if matches!(context.engine(), "postgresql" | "mysql" | "mariadb") {
                let verified = verify_trusted_bundled_tool(
                    &self.store,
                    &self.install_root,
                    part.dump_tool,
                    &part.dump_tool_path,
                )
                .map_err(|_| BackupExecutionFailure::terminal("BACKUP_TOOL_UNTRUSTED"))?;
                part.dump_tool_path = verified.canonical_path().to_path_buf();
                part.dump_tool_sha256 = Some(*verified.sha256());
                part.dump_tool_dependencies = verified.dependencies().to_vec();
                verified.sha256_hex()
            } else {
                sha256_for_tool(&part.dump_tool_path)
                    .map_err(|_| BackupExecutionFailure::terminal("BACKUP_TOOL_IDENTITY_FAILED"))?
            };
            let mut artifact = source.spawn(&part).map_err(|_| {
                if cancellation.is_cancelled() {
                    BackupExecutionFailure::cancelled()
                } else {
                    BackupExecutionFailure::retryable("BACKUP_SOURCE_START_FAILED")
                }
            })?;
            if artifact
                .native_process()
                .is_some_and(|native| hex_sha256(native.executable_sha256()) != tool_sha256)
            {
                drop(artifact);
                cleanup_prepared(&prepared, &work_spool);
                return Err(BackupExecutionFailure::terminal("BACKUP_TOOL_UNTRUSTED"));
            }
            self.register_artifact_process(claim, &part, &mut artifact)?;
            let artifact_timeout =
                match execution_time_remaining(deadline_ms, self.clock, &cancellation) {
                    Ok(timeout) => timeout,
                    Err(error) => {
                        cleanup_prepared(&prepared, &work_spool);
                        return Err(error);
                    }
                };
            let commit = run_artifact_until_deadline(
                artifact,
                spool_path.clone(),
                dek,
                &cfg,
                cancellation.pipeline.clone(),
                cancellation.clone(),
                artifact_timeout,
            )
            .await
            .map_err(|_| {
                if cancellation.is_cancelled() {
                    BackupExecutionFailure::cancelled()
                } else {
                    BackupExecutionFailure::retryable("BACKUP_PIPELINE_FAILED")
                }
            });
            let commit = match commit {
                Ok(commit) => commit,
                Err(error) => {
                    let _ = std::fs::remove_file(&spool_path);
                    cleanup_prepared(&prepared, &work_spool);
                    if error.cancelled {
                        return Ok(BackupExecution::Cancelled);
                    }
                    return Err(error);
                }
            };
            prepared.push(PreparedPart {
                plan: part,
                commit,
                wrap_set,
                tool_sha256,
            });
        }
        if cancellation.is_cancelled() {
            cleanup_prepared(&prepared, &work_spool);
            return Ok(BackupExecution::Cancelled);
        }
        if (self.clock)() >= deadline_ms {
            cancellation.cancel_with(CancellationCause::ExecutionTimeout);
            cleanup_prepared(&prepared, &work_spool);
            return Err(BackupExecutionFailure::retryable(
                "BACKUP_EXECUTION_TIMEOUT",
            ));
        }
        let source_evidence = source.take_manifest_evidence().map_err(|_| {
            cleanup_prepared(&prepared, &work_spool);
            BackupExecutionFailure::retryable("BACKUP_SOURCE_EVIDENCE_FAILED")
        })?;
        let result = self
            .publish(
                claim,
                context,
                PreparedPublish {
                    parts: prepared,
                    destinations,
                    work_spool: work_spool.clone(),
                    pipeline_config: cfg,
                    tls_mode,
                    source_evidence,
                    cancellation,
                },
                deadline_ms,
            )
            .await;
        if matches!(&result, Err(error) if !error.commit_started) {
            cleanup_spool_directory(&work_spool);
        }
        result
    }

    fn register_artifact_process(
        &self,
        claim: &dbbackup_catalog::ClaimedWork,
        part: &dbbackup_pipeline::DumpPlanPart,
        artifact: &mut DumpArtifact,
    ) -> Result<(), BackupExecutionFailure> {
        register_backup_artifact_process(&self.store.runtime(), claim, part, artifact, self.clock)
    }

    async fn publish(
        &self,
        claim: &dbbackup_catalog::ClaimedWork,
        context: &BackupClaimContext,
        prepared_publish: PreparedPublish,
        deadline_ms: i64,
    ) -> Result<BackupExecution, BackupExecutionFailure> {
        let PreparedPublish {
            parts: prepared,
            destinations,
            work_spool,
            pipeline_config: cfg,
            tls_mode,
            source_evidence,
            cancellation,
        } = prepared_publish;
        let committed_at_ms = (self.clock)();
        let recovery_point_id = RecoveryPointId::new();
        let prefix = format!(
            "v2/{}/{}/{}",
            context.plan_occurrence_id(),
            context.backup_scope_id(),
            recovery_point_id
        );
        let manifest_key = format!("{prefix}/set-manifest.json");
        let mut options = BTreeMap::new();
        let workspace = self
            .store
            .workspace()
            .for_work_item(claim.work_item_id())
            .map_err(|_| BackupExecutionFailure::terminal("BACKUP_WORKSPACE_OWNERSHIP_MISSING"))?;
        options.insert("workspaceId".to_owned(), workspace.id);
        options.insert("workspaceName".to_owned(), workspace.display_name);
        options.insert(
            "backupScopeId".to_owned(),
            context.backup_scope_id().to_string(),
        );
        options.insert(
            "backupScopeMode".to_owned(),
            context.scope_mode().to_owned(),
        );
        options.insert(
            "backupScopeCoverage".to_owned(),
            if context.scope_mode() == "whole_database" {
                "whole_database"
            } else {
                "partial_tables"
            }
            .to_owned(),
        );
        options.insert(
            "backupScopeDependencyPolicy".to_owned(),
            scope_dependency_policy(context.engine(), context.scope_mode()).to_owned(),
        );
        options.insert(
            "backupScopeObjectCount".to_owned(),
            context.resolved_object_count().to_string(),
        );
        options.insert(
            "backupScopeSnapshotHashBlake3".to_owned(),
            context.scope_snapshot_hash_blake3().to_owned(),
        );
        options.insert(
            "backupScopeSnapshot".to_owned(),
            manifest_json_option(context.scope_snapshot_json())?,
        );
        options.insert(
            "databaseTargetId".to_owned(),
            context.database_target_id().to_string(),
        );
        options.insert(
            "databaseSourceFingerprint".to_owned(),
            context.database_source_fingerprint().to_owned(),
        );
        options.insert(
            "encryptionKeyRef".to_owned(),
            self.encryption_key_id.clone(),
        );
        options.insert(
            "targetOptions".to_owned(),
            manifest_json_option(context.target_options_json())?,
        );
        options.insert(
            "executionPolicy".to_owned(),
            manifest_json_option(context.execution_policy_json())?,
        );
        options.insert("tlsMode".to_owned(), tls_mode);
        let consistency_grade = merge_source_manifest_evidence(
            &mut options,
            source_evidence,
            consistency_for_engine(context.engine()),
        )?;
        let mut manifest_parts = Vec::with_capacity(prepared.len());
        let mut sources = Vec::with_capacity(prepared.len());
        for part in &prepared {
            options.insert(
                format!("dumpFlags.{}", part.plan.backup_id),
                manifest_text_option(&part.plan.dump_flags),
            );
            if let Some(pair_id) = part.plan.pairs_with_globals_backup_id {
                options.insert(
                    format!("pairsWithGlobals.{}", part.plan.backup_id),
                    pair_id.to_string(),
                );
            }
            if let Some(charset) = &part.plan.charset {
                options.insert(
                    format!("charset.{}", part.plan.backup_id),
                    manifest_text_option(charset),
                );
            }
            if let Some(collation) = &part.plan.collation {
                options.insert(
                    format!("collation.{}", part.plan.backup_id),
                    manifest_text_option(collation),
                );
            }
            if !part.plan.extensions.is_empty() {
                let extensions = serde_json::to_string(&part.plan.extensions)
                    .map_err(|_| "BACKUP_MANIFEST_INVALID")?;
                options.insert(
                    format!("extensions.{}", part.plan.backup_id),
                    manifest_json_option(&extensions)?,
                );
            }
            options.insert(
                format!("dekWrap.{}", part.plan.backup_id),
                serde_json::to_string(&part.wrap_set).map_err(|_| "BACKUP_MANIFEST_INVALID")?,
            );
            let role = match part.plan.kind {
                dbbackup_pipeline::DumpPartKind::Globals => ArtifactPartRole::Globals,
                dbbackup_pipeline::DumpPartKind::Database { .. } => ArtifactPartRole::DatabaseDump,
            };
            let logical_name = match &part.plan.kind {
                dbbackup_pipeline::DumpPartKind::Globals => "globals.sql".to_owned(),
                dbbackup_pipeline::DumpPartKind::Database { name } => name.clone(),
            };
            let part_spec = PartSpec::new(
                part.plan.backup_id,
                role,
                logical_name,
                true,
                normalize_format(part.plan.dump_format),
            )
            .map_err(|_| "BACKUP_MANIFEST_INVALID")?;
            let evidence = EncodingEvidenceV2::new(
                blake3::Hash::from_bytes(part.commit.plaintext_blake3)
                    .to_hex()
                    .to_string(),
                blake3::Hash::from_bytes(part.commit.ciphertext_blake3)
                    .to_hex()
                    .to_string(),
                part.commit.plaintext_bytes,
                part.commit.ciphertext_bytes,
                "zstd",
                "chacha20poly1305_stream",
                DEK_WRAP_GENERATION,
            )
            .map_err(|_| "BACKUP_MANIFEST_INVALID")?;
            let object_key = format!("{prefix}/parts/{}.enc", part.plan.backup_id);
            manifest_parts.push(
                ManifestPartV2::encoded(part_spec, evidence, object_key)
                    .map_err(|_| "BACKUP_MANIFEST_INVALID")?,
            );
            sources.push(
                EncodedPartSourceV2::new(part.plan.backup_id, part.commit.spool_path.clone())
                    .map_err(|_| "BACKUP_MANIFEST_INVALID")?,
            );
        }
        let replicas = destinations
            .iter()
            .map(|destination| {
                ReplicaCommitmentV2::new(
                    destination.fingerprint.clone(),
                    destination.kind.clone(),
                    destination.requirement,
                    ReaderStatusV2::Eligible,
                    destination.immutability.clone(),
                    manifest_key.clone(),
                )
                .map_err(|_| "BACKUP_MANIFEST_INVALID")
            })
            .collect::<Result<Vec<_>, _>>()?;
        let tool_part = prepared
            .iter()
            .find(|part| {
                matches!(
                    part.plan.kind,
                    dbbackup_pipeline::DumpPartKind::Database { .. }
                )
            })
            .ok_or("BACKUP_MANIFEST_INVALID")?;
        let tool = NativeToolIdentityV2::new(
            tool_part.plan.dump_tool,
            tool_part.plan.dump_tool_version.clone(),
            tool_part.tool_sha256.clone(),
        )
        .map_err(|_| "BACKUP_TOOL_IDENTITY_FAILED")?;
        let manifest = SetManifestV2::new(
            RecoveryOriginV2::new(
                recovery_point_id.as_uuid(),
                self.instance_id.clone(),
                context.plan_occurrence_id().as_uuid(),
                context.backup_run_id().as_uuid(),
                context.backup_scope_id().as_uuid(),
                context.source_fingerprint(),
                context.scope_display_name(),
            )
            .map_err(|_| "BACKUP_MANIFEST_INVALID")?,
            parse_engine(context.engine()).ok_or("BACKUP_ENGINE_UNSUPPORTED")?,
            consistency_grade,
            committed_at_ms,
            self.signing_key_id.clone(),
            tool,
            options,
            manifest_parts,
            replicas,
        )
        .map_err(|_| "BACKUP_MANIFEST_INVALID")?;
        let signed = SignedSetManifestV2::create(manifest, &self.signing_key)
            .map_err(|_| "BACKUP_MANIFEST_SIGN_FAILED")?;
        let endpoints = destinations
            .into_iter()
            .map(|destination| destination.endpoint)
            .collect();
        if cancellation.is_cancelled() {
            cleanup_spool_directory(&work_spool);
            return Ok(BackupExecution::Cancelled);
        }
        execution_time_remaining(deadline_ms, self.clock, &cancellation)?;
        match self
            .store
            .runtime()
            .begin_backup_claim_commit(claim, &recovery_point_id, (self.clock)())
            .map_err(|_| BackupExecutionFailure::retryable("BACKUP_COMMIT_BEGIN_FAILED"))?
        {
            BeginCommitDisposition::CancelRequested => {
                cleanup_spool_directory(&work_spool);
                return Ok(BackupExecution::Cancelled);
            }
            BeginCommitDisposition::Committing => {}
        }
        let publish_timeout =
            commit_publish_time_remaining(deadline_ms, self.clock, &cancellation)?;
        let outcomes = match tokio::time::timeout(
            publish_timeout,
            publish_set_manifest_v2(
                &signed,
                sources,
                endpoints,
                &cfg,
                &self.signing_key.verifying_key(),
            ),
        )
        .await
        {
            Ok(result) => {
                result.map_err(|_| BackupExecutionFailure::after_commit("BACKUP_PUBLISH_FAILED"))?
            }
            Err(_) => {
                cancellation.cancel_with(CancellationCause::ExecutionTimeout);
                return Err(BackupExecutionFailure::after_commit(
                    "BACKUP_EXECUTION_TIMEOUT",
                ));
            }
        };
        let (succeeded, outcome) = classify_replica_results(
            outcomes
                .iter()
                .map(|outcome| (outcome.requirement, outcome.result.is_ok())),
        )
        .map_err(BackupExecutionFailure::after_commit)?;
        let spec = CompleteRecoveryPointSpec::from_verified_publish(
            &signed,
            &outcomes,
            &self.signing_key.verifying_key(),
            committed_at_ms,
        )
        .map_err(|_| BackupExecutionFailure::after_commit("BACKUP_RECOVERY_POINT_INVALID"))?;
        Ok(BackupExecution::Committed {
            spec: Box::new(spec),
            outcome,
            result_json: json!({
                "schemaVersion": 1,
                "recoveryPointId": recovery_point_id.to_string(),
                "replicasCommitted": succeeded,
                "replicasConfigured": outcomes.len(),
            })
            .to_string(),
            work_spool,
        })
    }
}

fn register_backup_artifact_process(
    runtime: &RuntimeRepository,
    claim: &dbbackup_catalog::ClaimedWork,
    part: &dbbackup_pipeline::DumpPlanPart,
    artifact: &mut DumpArtifact,
    clock: fn() -> i64,
) -> Result<(), BackupExecutionFailure> {
    let Some(native) = artifact.native_process() else {
        return Ok(());
    };

    let process_instance_id = Uuid::new_v4().to_string();
    let observed_at_ms = clock().max(native.process_start_time_ms());
    runtime
        .record_process_start(ProcessStartSpec::new(
            process_instance_id.clone(),
            claim.clone(),
            format!("native_dump:{}", part.dump_tool),
            native.process_id(),
            native.process_start_time_ms(),
            claim.worker_boot_id(),
            native.job_object_key(),
            native.executable_path().to_string_lossy().into_owned(),
            Some(hex_sha256(native.executable_sha256())),
            observed_at_ms,
        ))
        .map_err(|_| BackupExecutionFailure::retryable("BACKUP_PROCESS_RECORD_FAILED"))?;
    runtime
        .mark_process_running(claim, &process_instance_id, observed_at_ms)
        .map_err(|_| BackupExecutionFailure::retryable("BACKUP_PROCESS_RECORD_FAILED"))?;

    let exit_repository = runtime.clone();
    let exit_claim = claim.clone();
    artifact.observe_native_exit(move |exit| {
        let exit_code = exit.code.map(i64::from).unwrap_or(-1);
        let reason = if exit.success {
            "exited"
        } else {
            "exited_nonzero"
        };
        if let Err(error) = exit_repository.record_process_exit(
            &exit_claim,
            ProcessExitSpec::new(
                process_instance_id,
                clock().max(observed_at_ms),
                exit_code,
                reason,
            ),
        ) {
            tracing::error!(
                error_code = error.code(),
                "native backup child exit could not be recorded"
            );
        }
    });
    Ok(())
}

fn resolve_mysql_metadata_client(
    install_root: &Path,
    client_kind: ClientKind,
) -> Result<PathBuf, BackupExecutionFailure> {
    let bundle_base = install_root.join("tools").join("mysql");
    resolve_bundled_metadata_client(client_kind, &bundle_base)
        .map_err(|_| BackupExecutionFailure::terminal("BACKUP_TOOL_MISSING"))
}

pub struct ServiceBackupWorker {
    store: OperationalStore,
    executor: Arc<NativeBackupExecutor>,
    worker_id: String,
    worker_boot_id: String,
    source_gate: Arc<SourceGate>,
    shutdown: Option<Arc<AtomicBool>>,
    clock: fn() -> i64,
}

impl ServiceBackupWorker {
    pub fn with_clock(
        store: OperationalStore,
        executor: Arc<NativeBackupExecutor>,
        worker_id: impl Into<String>,
        worker_boot_id: impl Into<String>,
        clock: fn() -> i64,
    ) -> Self {
        Self {
            store,
            executor,
            worker_id: worker_id.into(),
            worker_boot_id: worker_boot_id.into(),
            source_gate: Arc::new(SourceGate::default()),
            shutdown: None,
            clock,
        }
    }

    fn pooled(
        store: OperationalStore,
        executor: Arc<NativeBackupExecutor>,
        worker_id: String,
        worker_boot_id: String,
        source_gate: Arc<SourceGate>,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        Self {
            store,
            executor,
            worker_id,
            worker_boot_id,
            source_gate,
            shutdown: Some(shutdown),
            clock: unix_time_ms,
        }
    }

    pub fn run_once(&self) -> Result<BackupWorkerTick, BackupWorkerError> {
        let now_ms = (self.clock)();
        let runtime = self.store.runtime();
        let Some(claim) = runtime.claim_next(
            ClaimRequest::new(
                &self.worker_id,
                &self.worker_boot_id,
                now_ms,
                LEASE_DURATION_MS,
            )
            .for_kind(WorkItemKind::Backup),
        )?
        else {
            return Ok(BackupWorkerTick::Idle);
        };
        let work_item_id = claim.work_item_id().clone();
        let context = runtime.backup_claim_context(&claim, now_ms)?;
        if runtime.claim_cancel_requested(&claim, now_ms)? {
            runtime.complete_claim(ClaimCompletionSpec::cancelled(
                claim,
                now_ms,
                Some("CANCEL_REQUESTED".to_owned()),
                None,
            ))?;
            return Ok(BackupWorkerTick::Cancelled(work_item_id));
        }
        let admission = match self.executor.admit_claim(&claim, &context, now_ms) {
            Ok(admission) => admission,
            Err(error) => {
                return retry_claim_tick(&runtime, &claim, work_item_id, now_ms, error.code);
            }
        };
        if !admission.admitted() {
            let reason = admission
                .wait_reason()
                .unwrap_or("backup_capacity_unavailable");
            runtime.defer_claim(&claim, now_ms, now_ms.saturating_add(BUSY_DELAY_MS), reason)?;
            return Ok(BackupWorkerTick::Deferred(work_item_id));
        }
        let source_key = context.connection_profile_id().to_string();
        let Some(_source_guard) = self.source_gate.try_acquire(source_key) else {
            runtime.defer_claim(
                &claim,
                now_ms,
                now_ms.saturating_add(BUSY_DELAY_MS),
                "source_busy",
            )?;
            return Ok(BackupWorkerTick::Deferred(work_item_id));
        };
        runtime.start_claim(&claim, now_ms)?;
        let cancellation = ServiceBackupCancellation::default();
        let deadline_ms = now_ms.saturating_add(self.executor.limits.execution_timeout_ms);
        let monitor = match ClaimMonitor::start(
            self.store.clone(),
            claim.clone(),
            cancellation.clone(),
            self.shutdown.clone(),
            self.clock,
            ClaimMonitorPolicy {
                deadline_ms,
                spool_root: self.executor.spool_root.clone(),
                min_free_disk_bytes: self.executor.limits.min_free_disk_bytes,
            },
        ) {
            Ok(monitor) => monitor,
            Err(_) => {
                let disposition = runtime.retry_claim(
                    &claim,
                    now_ms,
                    now_ms.saturating_add(RETRY_DELAY_MS),
                    "BACKUP_MONITOR_UNAVAILABLE",
                    None,
                )?;
                return Ok(match disposition {
                    RetryDisposition::Requeued => BackupWorkerTick::Retried(work_item_id),
                    RetryDisposition::Exhausted => BackupWorkerTick::Failed(work_item_id),
                    RetryDisposition::Cancelled => BackupWorkerTick::Cancelled(work_item_id),
                });
            }
        };
        let execution = self
            .executor
            .execute(&claim, &context, cancellation.clone(), deadline_ms);
        monitor.stop();
        let completed_at_ms = (self.clock)().max(now_ms);
        match execution {
            Ok(BackupExecution::Cancelled) => settle_cancelled_claim(
                &runtime,
                claim,
                work_item_id,
                completed_at_ms,
                &cancellation,
            ),
            Ok(BackupExecution::Committed {
                spec,
                outcome,
                result_json,
                work_spool,
            }) => {
                runtime.commit_backup_claim(
                    &claim,
                    *spec,
                    outcome,
                    completed_at_ms,
                    Some(result_json),
                )?;
                cleanup_spool_directory(&work_spool);
                Ok(match outcome {
                    ClaimTerminalOutcome::Succeeded => BackupWorkerTick::Completed(work_item_id),
                    ClaimTerminalOutcome::PartiallySucceeded => {
                        BackupWorkerTick::PartiallyCompleted(work_item_id)
                    }
                    ClaimTerminalOutcome::Failed | ClaimTerminalOutcome::Cancelled => {
                        unreachable!()
                    }
                })
            }
            Err(error) => settle_backup_execution_failure(
                &runtime,
                claim,
                work_item_id,
                completed_at_ms,
                &cancellation,
                error,
            ),
        }
    }
}

pub struct ServiceBackupWorkerHandle {
    stop: Arc<AtomicBool>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl ServiceBackupWorkerHandle {
    pub fn spawn(
        store: OperationalStore,
        secrets: Arc<ServiceSecretStore>,
        install_root: PathBuf,
        spool_root: PathBuf,
        health: ServiceHealthProbe,
    ) -> Result<Self, BackupWorkerStartError> {
        let instance_id = health.snapshot().boot_id().to_string();
        let executor = Arc::new(NativeBackupExecutor::new(
            store.clone(),
            secrets,
            install_root,
            spool_root,
            instance_id,
        )?);
        Self::spawn_with_executor(store, executor, health)
    }

    pub fn spawn_with_executor(
        store: OperationalStore,
        executor: Arc<NativeBackupExecutor>,
        health: ServiceHealthProbe,
    ) -> Result<Self, BackupWorkerStartError> {
        let stop = Arc::new(AtomicBool::new(false));
        let source_gate = Arc::new(SourceGate::default());
        let worker_boot_id = Uuid::new_v4().to_string();
        let mut threads = Vec::with_capacity(DEFAULT_WORKER_THREADS);
        for index in 0..DEFAULT_WORKER_THREADS {
            let thread_stop = Arc::clone(&stop);
            let worker = ServiceBackupWorker::pooled(
                store.clone(),
                Arc::clone(&executor),
                format!("backup-worker-{index}-{}", Uuid::new_v4()),
                worker_boot_id.clone(),
                Arc::clone(&source_gate),
                Arc::clone(&stop),
            );
            let thread_health = health.clone();
            match thread::Builder::new()
                .name(format!("dbbackup-backup-worker-{index}"))
                .spawn(move || {
                    while !thread_stop.load(Ordering::Acquire) {
                        if !thread_health.snapshot().claims_enabled() {
                            thread::park_timeout(IDLE_POLL_INTERVAL);
                            continue;
                        }
                        match worker.run_once() {
                            Ok(BackupWorkerTick::Idle | BackupWorkerTick::Deferred(_)) => {
                                thread::park_timeout(IDLE_POLL_INTERVAL);
                            }
                            Ok(_) => {}
                            Err(error) => {
                                tracing::error!(
                                    error_code = error.code(),
                                    "Backup worker tick failed"
                                );
                                thread::park_timeout(IDLE_POLL_INTERVAL);
                            }
                        }
                    }
                }) {
                Ok(thread) => threads.push(thread),
                Err(error) => {
                    stop.store(true, Ordering::Release);
                    for thread in threads.drain(..) {
                        thread.thread().unpark();
                        let _ = thread.join();
                    }
                    return Err(BackupWorkerStartError::Spawn(error));
                }
            }
        }
        Ok(Self {
            stop,
            threads: Mutex::new(threads),
        })
    }

    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
        if let Ok(threads) = self.threads.lock() {
            for thread in threads.iter() {
                thread.thread().unpark();
            }
        }
    }

    pub fn wait_stopped(&self) -> Result<(), HostError> {
        let threads = self
            .threads
            .lock()
            .map_err(|_| HostError::InternalState)?
            .drain(..)
            .collect::<Vec<_>>();
        let mut failed = false;
        for thread in threads {
            failed |= thread.join().is_err();
        }
        if failed {
            Err(HostError::RuntimeFailed)
        } else {
            Ok(())
        }
    }
}

impl ClaimRuntime for ServiceBackupWorkerHandle {
    fn request_stop(&self) {
        Self::request_stop(self);
    }

    fn wait_stopped(&self) -> Result<(), HostError> {
        Self::wait_stopped(self)
    }
}

impl Drop for ServiceBackupWorkerHandle {
    fn drop(&mut self) {
        self.request_stop();
        let _ = self.wait_stopped();
    }
}

struct PreparedPart {
    plan: dbbackup_pipeline::DumpPlanPart,
    commit: SpoolCommit,
    wrap_set: WrapSet,
    tool_sha256: String,
}

struct PreparedDestination {
    fingerprint: String,
    kind: String,
    requirement: ReplicaRequirementV2,
    immutability: String,
    endpoint: ReplicaEndpointV2,
}

enum BackupExecution {
    Cancelled,
    Committed {
        spec: Box<CompleteRecoveryPointSpec>,
        outcome: ClaimTerminalOutcome,
        result_json: String,
        work_spool: PathBuf,
    },
}

struct PreparedPublish {
    parts: Vec<PreparedPart>,
    destinations: Vec<PreparedDestination>,
    work_spool: PathBuf,
    pipeline_config: PipelineCfg,
    tls_mode: String,
    source_evidence: SourceManifestEvidence,
    cancellation: ServiceBackupCancellation,
}

struct BackupExecutionFailure {
    code: &'static str,
    retryable: bool,
    commit_started: bool,
    cancelled: bool,
}

#[derive(Clone)]
struct ServiceBackupCancellation {
    provider: BackupCancellationToken,
    pipeline: PipelineCancellationToken,
    cause: Arc<AtomicU8>,
}

impl ServiceBackupCancellation {
    fn cancel_with(&self, cause: CancellationCause) {
        let _ = self.cause.compare_exchange(
            CancellationCause::None.as_u8(),
            cause.as_u8(),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.provider.cancel();
        self.pipeline.cancel();
    }

    fn cause(&self) -> Option<CancellationCause> {
        CancellationCause::from_u8(self.cause.load(Ordering::Acquire)).reported()
    }

    fn retry_error_code(&self) -> Option<&'static str> {
        match self.cause()? {
            CancellationCause::ExecutionTimeout => Some("BACKUP_EXECUTION_TIMEOUT"),
            CancellationCause::DiskFloor => Some("BACKUP_SPOOL_DISK_LOW"),
            CancellationCause::SpoolProbeFailed => Some("BACKUP_SPOOL_PROBE_FAILED"),
            CancellationCause::LeaseLost => Some("BACKUP_LEASE_LOST"),
            CancellationCause::None
            | CancellationCause::UserRequested
            | CancellationCause::ServiceShutdown => None,
        }
    }

    fn terminal_error_code(&self) -> &'static str {
        match self.cause() {
            Some(CancellationCause::ServiceShutdown) => "SERVICE_SHUTDOWN",
            _ => "CANCEL_REQUESTED",
        }
    }

    fn is_cancelled(&self) -> bool {
        self.provider.is_cancelled() || self.pipeline.is_cancelled()
    }
}

impl Default for ServiceBackupCancellation {
    fn default() -> Self {
        Self {
            provider: BackupCancellationToken::default(),
            pipeline: PipelineCancellationToken::default(),
            cause: Arc::new(AtomicU8::new(CancellationCause::None.as_u8())),
        }
    }
}

async fn plan_source_with_timeout(
    source: &dyn BackupSource,
    cancellation: &ServiceBackupCancellation,
    timeout: Duration,
) -> Result<dbbackup_pipeline::DumpPlan, BackupExecutionFailure> {
    if timeout.is_zero() {
        cancellation.cancel_with(CancellationCause::ExecutionTimeout);
        return Err(BackupExecutionFailure::retryable(
            "BACKUP_EXECUTION_TIMEOUT",
        ));
    }
    tokio::select! {
        result = source.plan() => result.map_err(classify_source_plan_failure),
        () = cancellation.pipeline.cancelled() => {
            if cancellation.cause() == Some(CancellationCause::ExecutionTimeout) {
                Err(BackupExecutionFailure::retryable("BACKUP_EXECUTION_TIMEOUT"))
            } else {
                Err(BackupExecutionFailure::cancelled())
            }
        }
        () = tokio::time::sleep(timeout) => {
            cancellation.cancel_with(CancellationCause::ExecutionTimeout);
            Err(BackupExecutionFailure::retryable("BACKUP_EXECUTION_TIMEOUT"))
        }
    }
}

fn classify_source_plan_failure(error: anyhow::Error) -> BackupExecutionFailure {
    if error
        .downcast_ref::<dbbackup_source_pg::PgSourceError>()
        .is_some_and(|error| {
            matches!(
                error,
                dbbackup_source_pg::PgSourceError::DumpToolUntrusted { .. }
            )
        })
        || error
            .downcast_ref::<MySqlSourceError>()
            .is_some_and(|error| matches!(error, MySqlSourceError::DumpToolUntrusted { .. }))
    {
        BackupExecutionFailure::terminal("BACKUP_TOOL_UNTRUSTED")
    } else if error
        .downcast_ref::<MySqlSourceError>()
        .is_some_and(|error| matches!(error, MySqlSourceError::ServerKindMismatch { .. }))
    {
        BackupExecutionFailure::terminal("BACKUP_ENGINE_MISMATCH")
    } else {
        BackupExecutionFailure::retryable("BACKUP_SOURCE_PLAN_FAILED")
    }
}

fn execution_time_remaining(
    deadline_ms: i64,
    clock: fn() -> i64,
    cancellation: &ServiceBackupCancellation,
) -> Result<Duration, BackupExecutionFailure> {
    if cancellation.is_cancelled() {
        return if cancellation.cause() == Some(CancellationCause::ExecutionTimeout) {
            Err(BackupExecutionFailure::retryable(
                "BACKUP_EXECUTION_TIMEOUT",
            ))
        } else {
            Err(BackupExecutionFailure::cancelled())
        };
    }
    let remaining_ms = deadline_ms.saturating_sub(clock());
    if remaining_ms <= 0 {
        cancellation.cancel_with(CancellationCause::ExecutionTimeout);
        return Err(BackupExecutionFailure::retryable(
            "BACKUP_EXECUTION_TIMEOUT",
        ));
    }
    Ok(Duration::from_millis(
        u64::try_from(remaining_ms).unwrap_or(u64::MAX),
    ))
}

fn commit_publish_time_remaining(
    deadline_ms: i64,
    clock: fn() -> i64,
    cancellation: &ServiceBackupCancellation,
) -> Result<Duration, BackupExecutionFailure> {
    let remaining_ms = deadline_ms.saturating_sub(clock());
    if remaining_ms <= 0 {
        cancellation.cancel_with(CancellationCause::ExecutionTimeout);
        return Err(BackupExecutionFailure::after_commit(
            "BACKUP_EXECUTION_TIMEOUT",
        ));
    }
    Ok(Duration::from_millis(
        u64::try_from(remaining_ms).unwrap_or(u64::MAX),
    ))
}

#[allow(clippy::too_many_arguments)]
async fn run_artifact_until_deadline(
    artifact: DumpArtifact,
    spool_path: PathBuf,
    dek: Dek,
    cfg: &PipelineCfg,
    pipeline_cancellation: PipelineCancellationToken,
    cancellation: ServiceBackupCancellation,
    timeout: Duration,
) -> anyhow::Result<SpoolCommit> {
    let run = run_artifact_with_cancellation(artifact, spool_path, dek, cfg, pipeline_cancellation);
    tokio::pin!(run);
    tokio::select! {
        result = &mut run => result,
        () = tokio::time::sleep(timeout) => {
            cancellation.cancel_with(CancellationCause::ExecutionTimeout);
            // The pipeline cancellation path aborts and joins the native child before returning,
            // which lets the exit observer settle the durable process row before claim retry.
            let _ = run.await;
            anyhow::bail!("BACKUP_EXECUTION_TIMEOUT")
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum CancellationCause {
    None = 0,
    UserRequested = 1,
    ServiceShutdown = 2,
    ExecutionTimeout = 3,
    DiskFloor = 4,
    SpoolProbeFailed = 5,
    LeaseLost = 6,
}

impl CancellationCause {
    const fn as_u8(self) -> u8 {
        self as u8
    }

    const fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::UserRequested,
            2 => Self::ServiceShutdown,
            3 => Self::ExecutionTimeout,
            4 => Self::DiskFloor,
            5 => Self::SpoolProbeFailed,
            6 => Self::LeaseLost,
            _ => Self::None,
        }
    }

    const fn reported(self) -> Option<Self> {
        match self {
            Self::None => None,
            value => Some(value),
        }
    }
}

impl BackupExecutionFailure {
    const fn terminal(code: &'static str) -> Self {
        Self {
            code,
            retryable: false,
            commit_started: false,
            cancelled: false,
        }
    }

    const fn retryable(code: &'static str) -> Self {
        Self {
            code,
            retryable: true,
            commit_started: false,
            cancelled: false,
        }
    }

    const fn cancelled() -> Self {
        Self {
            code: "CANCEL_REQUESTED",
            retryable: false,
            commit_started: false,
            cancelled: true,
        }
    }

    const fn after_commit(code: &'static str) -> Self {
        Self {
            code,
            retryable: false,
            commit_started: true,
            cancelled: false,
        }
    }
}

impl From<&'static str> for BackupExecutionFailure {
    fn from(code: &'static str) -> Self {
        Self::terminal(code)
    }
}

#[derive(Default)]
struct SourceGate {
    active: Mutex<HashSet<String>>,
}

impl SourceGate {
    fn try_acquire(self: &Arc<Self>, key: String) -> Option<SourceGuard> {
        let mut active = self.active.lock().ok()?;
        if !active.insert(key.clone()) {
            return None;
        }
        drop(active);
        Some(SourceGuard {
            gate: Arc::clone(self),
            key,
        })
    }
}

struct SourceGuard {
    gate: Arc<SourceGate>,
    key: String,
}

impl Drop for SourceGuard {
    fn drop(&mut self) {
        if let Ok(mut active) = self.gate.active.lock() {
            active.remove(&self.key);
        }
    }
}

struct ClaimMonitor {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

struct ClaimMonitorPolicy {
    deadline_ms: i64,
    spool_root: PathBuf,
    min_free_disk_bytes: u64,
}

impl ClaimMonitor {
    fn start(
        store: OperationalStore,
        claim: dbbackup_catalog::ClaimedWork,
        cancellation: ServiceBackupCancellation,
        shutdown: Option<Arc<AtomicBool>>,
        clock: fn() -> i64,
        policy: ClaimMonitorPolicy,
    ) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name(format!("dbbackup-backup-heartbeat-{}", claim.attempt_no()))
            .spawn(move || {
                while !thread_stop.load(Ordering::Acquire) {
                    thread::park_timeout(HEARTBEAT_INTERVAL);
                    if thread_stop.load(Ordering::Acquire) {
                        break;
                    }
                    if shutdown
                        .as_ref()
                        .is_some_and(|shutdown| shutdown.load(Ordering::Acquire))
                    {
                        cancellation.cancel_with(CancellationCause::ServiceShutdown);
                    }
                    let now_ms = clock();
                    let runtime = store.runtime();
                    match runtime.claim_cancel_requested(&claim, now_ms) {
                        Ok(true) => cancellation.cancel_with(CancellationCause::UserRequested),
                        Ok(false) => {}
                        Err(_) => {
                            cancellation.cancel_with(CancellationCause::LeaseLost);
                            break;
                        }
                    }
                    apply_monitor_policy(
                        &cancellation,
                        now_ms,
                        policy.deadline_ms,
                        fs2::available_space(&policy.spool_root),
                        policy.min_free_disk_bytes,
                    );
                    if runtime
                        .heartbeat_claim(&claim, now_ms, now_ms.saturating_add(LEASE_DURATION_MS))
                        .is_err()
                    {
                        cancellation.cancel_with(CancellationCause::LeaseLost);
                        break;
                    }
                }
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }

    fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

fn apply_monitor_policy(
    cancellation: &ServiceBackupCancellation,
    now_ms: i64,
    deadline_ms: i64,
    available_space: std::io::Result<u64>,
    min_free_disk_bytes: u64,
) {
    if now_ms >= deadline_ms {
        cancellation.cancel_with(CancellationCause::ExecutionTimeout);
        return;
    }
    match available_space {
        Ok(free_bytes) if free_bytes < min_free_disk_bytes => {
            cancellation.cancel_with(CancellationCause::DiskFloor);
        }
        Ok(_) => {}
        Err(_) => cancellation.cancel_with(CancellationCause::SpoolProbeFailed),
    }
}

fn retry_claim_tick(
    runtime: &RuntimeRepository,
    claim: &dbbackup_catalog::ClaimedWork,
    work_item_id: WorkItemId,
    now_ms: i64,
    error_code: &'static str,
) -> Result<BackupWorkerTick, BackupWorkerError> {
    let disposition = runtime.retry_claim(
        claim,
        now_ms,
        now_ms.saturating_add(RETRY_DELAY_MS),
        error_code,
        None,
    )?;
    Ok(match disposition {
        RetryDisposition::Requeued => BackupWorkerTick::Retried(work_item_id),
        RetryDisposition::Exhausted => BackupWorkerTick::Failed(work_item_id),
        RetryDisposition::Cancelled => BackupWorkerTick::Cancelled(work_item_id),
    })
}

fn settle_backup_execution_failure(
    runtime: &RuntimeRepository,
    claim: dbbackup_catalog::ClaimedWork,
    work_item_id: WorkItemId,
    completed_at_ms: i64,
    cancellation: &ServiceBackupCancellation,
    error: BackupExecutionFailure,
) -> Result<BackupWorkerTick, BackupWorkerError> {
    if error.commit_started {
        return Ok(BackupWorkerTick::AwaitingReconciliation(work_item_id));
    }
    if error.cancelled {
        return settle_cancelled_claim(runtime, claim, work_item_id, completed_at_ms, cancellation);
    }
    if error.retryable {
        return retry_claim_tick(runtime, &claim, work_item_id, completed_at_ms, error.code);
    }
    runtime.complete_claim(ClaimCompletionSpec::failed(
        claim,
        completed_at_ms,
        error.code,
        None,
    ))?;
    Ok(BackupWorkerTick::Failed(work_item_id))
}

fn settle_cancelled_claim(
    runtime: &RuntimeRepository,
    claim: dbbackup_catalog::ClaimedWork,
    work_item_id: WorkItemId,
    completed_at_ms: i64,
    cancellation: &ServiceBackupCancellation,
) -> Result<BackupWorkerTick, BackupWorkerError> {
    if let Some(error_code) = cancellation.retry_error_code() {
        return retry_claim_tick(runtime, &claim, work_item_id, completed_at_ms, error_code);
    }
    runtime.complete_claim(ClaimCompletionSpec::cancelled(
        claim,
        completed_at_ms,
        Some(cancellation.terminal_error_code().to_owned()),
        None,
    ))?;
    Ok(BackupWorkerTick::Cancelled(work_item_id))
}

fn stable_capacity_id(kind: &str, key: &str) -> String {
    let digest = blake3::hash(format!("dbbackup-capacity\0{kind}\0{key}").as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest.as_bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes).to_string()
}

fn normalized_spool_key(root: &Path) -> Result<String, BackupExecutionFailure> {
    let canonical = std::fs::canonicalize(root)
        .map_err(|_| BackupExecutionFailure::retryable("BACKUP_SPOOL_PROBE_FAILED"))?;
    let normalized = canonical.to_string_lossy().replace('\\', "/");
    if normalized.is_empty() || normalized.len() > 4_096 {
        return Err(BackupExecutionFailure::terminal(
            "BACKUP_CAPACITY_POLICY_INVALID",
        ));
    }
    Ok(if cfg!(windows) {
        normalized.to_lowercase()
    } else {
        normalized
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredDatabaseCredentials {
    username: String,
    password: String,
}

struct DatabaseCredentials {
    username: String,
    password: Zeroizing<String>,
}

fn cleanup_prepared(prepared: &[PreparedPart], directory: &Path) {
    for part in prepared {
        let _ = std::fs::remove_file(&part.commit.spool_path);
    }
    cleanup_spool_directory(directory);
}

fn cleanup_spool_directory(directory: &Path) {
    let Some(work_item_directory) = directory.parent() else {
        return;
    };
    let Some(spool_root) = work_item_directory.parent() else {
        return;
    };
    let _ = cleanup_managed_attempt_spool(spool_root, directory);
}

fn scavenge_managed_spool(
    root: &Path,
    active: &HashSet<PathBuf>,
) -> std::io::Result<SpoolScavengeReport> {
    const MAX_SCANNED_ENTRIES: usize = 50_000;
    let root = std::fs::canonicalize(root)?;
    let mut report = SpoolScavengeReport::default();
    let mut scanned = 0_usize;
    for work_entry in std::fs::read_dir(&root)? {
        let work_entry = work_entry?;
        scanned = scanned.saturating_add(1);
        if scanned > MAX_SCANNED_ENTRIES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "managed spool entry limit exceeded",
            ));
        }
        let file_type = work_entry.file_type()?;
        if file_type.is_symlink()
            || !file_type.is_dir()
            || Uuid::parse_str(&work_entry.file_name().to_string_lossy()).is_err()
        {
            report.skipped_unmanaged = report.skipped_unmanaged.saturating_add(1);
            continue;
        }
        let attempts =
            std::fs::read_dir(work_entry.path())?.collect::<Result<Vec<_>, std::io::Error>>()?;
        for attempt_entry in attempts {
            scanned = scanned.saturating_add(1);
            if scanned > MAX_SCANNED_ENTRIES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "managed spool entry limit exceeded",
                ));
            }
            let file_type = attempt_entry.file_type()?;
            if file_type.is_symlink()
                || !file_type.is_dir()
                || !is_attempt_directory_name(&attempt_entry.file_name().to_string_lossy())
            {
                report.skipped_unmanaged = report.skipped_unmanaged.saturating_add(1);
                continue;
            }
            let attempt_path = match std::fs::canonicalize(attempt_entry.path()) {
                Ok(path) if path.starts_with(&root) => path,
                _ => {
                    report.skipped_unmanaged = report.skipped_unmanaged.saturating_add(1);
                    continue;
                }
            };
            if active.contains(&attempt_path) {
                report.preserved_active = report.preserved_active.saturating_add(1);
                continue;
            }
            cleanup_managed_attempt_spool(&root, &attempt_path)?;
            report.removed_attempts = report.removed_attempts.saturating_add(1);
        }
    }
    Ok(report)
}

fn cleanup_managed_attempt_spool(root: &Path, directory: &Path) -> std::io::Result<()> {
    let relative = directory.strip_prefix(root).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "attempt spool is outside the configured root",
        )
    })?;
    let mut components = relative.components();
    let work_item = match components.next() {
        Some(std::path::Component::Normal(value)) => value.to_string_lossy(),
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "attempt spool has no work item component",
            ));
        }
    };
    let attempt = match components.next() {
        Some(std::path::Component::Normal(value)) => value.to_string_lossy(),
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "attempt spool has no attempt component",
            ));
        }
    };
    if components.next().is_some()
        || Uuid::parse_str(&work_item).is_err()
        || !is_attempt_directory_name(&attempt)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "attempt spool path is not service managed",
        ));
    }
    remove_tree_without_following(directory)?;
    if let Some(parent) = directory.parent() {
        match std::fs::remove_dir(parent) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn is_attempt_directory_name(value: &str) -> bool {
    let Some(suffix) = value.strip_prefix("attempt-") else {
        return false;
    };
    let Some((attempt_no, fencing_token)) = suffix.split_once('-') else {
        return false;
    };
    attempt_no.parse::<i64>().is_ok_and(|value| value > 0)
        && fencing_token.parse::<i64>().is_ok_and(|value| value > 0)
}

fn remove_tree_without_following(path: &Path) -> std::io::Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() {
        return std::fs::remove_file(path).or_else(|_| std::fs::remove_dir(path));
    }
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path)? {
            remove_tree_without_following(&entry?.path())?;
        }
        std::fs::remove_dir(path)
    } else {
        std::fs::remove_file(path)
    }
}

fn hex_sha256(digest: &[u8; 32]) -> String {
    use std::fmt::Write as _;

    let mut encoded = String::with_capacity(64);
    for byte in digest {
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn sha256_for_tool(path: &Path) -> std::io::Result<String> {
    let effective = if path.to_string_lossy().starts_with("embedded:") {
        std::env::current_exe()?
    } else {
        resolve_executable_path(path)?
    };
    let mut file = File::open(effective)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn resolve_executable_path(path: &Path) -> std::io::Result<PathBuf> {
    if path.is_file() {
        return Ok(path.to_path_buf());
    }
    if path.components().count() == 1 {
        if let Some(search_path) = std::env::var_os("PATH") {
            for directory in std::env::split_paths(&search_path) {
                let candidate = directory.join(path);
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "native backup tool was not found",
    ))
}

fn normalize_format(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_lowercase() || character.is_ascii_digit() {
                character
            } else {
                '_'
            }
        })
        .collect()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredScopeSnapshot {
    mode: String,
    objects: Vec<StoredScopeObject>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredScopeObject {
    engine_object_id: String,
    kind: String,
    schema: String,
    name: String,
    object_fingerprint_blake3: String,
}

fn parse_scope_selection(
    snapshot_json: &str,
    expected_mode: &str,
    expected_object_count: i64,
    expected_hash_blake3: &str,
) -> Result<BackupScopeSelection, BackupExecutionFailure> {
    if expected_object_count < 0
        || blake3::hash(snapshot_json.as_bytes()).to_hex().as_str() != expected_hash_blake3
    {
        return Err(BackupExecutionFailure::terminal(
            "BACKUP_SCOPE_SNAPSHOT_INVALID",
        ));
    }
    let snapshot: StoredScopeSnapshot = serde_json::from_str(snapshot_json)
        .map_err(|_| BackupExecutionFailure::terminal("BACKUP_SCOPE_SNAPSHOT_INVALID"))?;
    if snapshot.mode != expected_mode
        || i64::try_from(snapshot.objects.len()).ok() != Some(expected_object_count)
    {
        return Err(BackupExecutionFailure::terminal(
            "BACKUP_SCOPE_SNAPSHOT_INVALID",
        ));
    }
    match snapshot.mode.as_str() {
        "whole_database" if snapshot.objects.is_empty() => {
            Ok(BackupScopeSelection::whole_database())
        }
        "explicit_tables" => {
            let mut tables = Vec::with_capacity(snapshot.objects.len());
            for object in snapshot.objects {
                if object.kind != "table"
                    || object.engine_object_id.trim().is_empty()
                    || !is_lower_hex_blake3(&object.object_fingerprint_blake3)
                {
                    return Err(BackupExecutionFailure::terminal(
                        "BACKUP_SCOPE_SNAPSHOT_INVALID",
                    ));
                }
                tables.push(SelectedTable::new(object.schema, object.name).map_err(|_| {
                    BackupExecutionFailure::terminal("BACKUP_SCOPE_SNAPSHOT_INVALID")
                })?);
            }
            BackupScopeSelection::explicit_tables(tables)
                .map_err(|_| BackupExecutionFailure::terminal("BACKUP_SCOPE_SNAPSHOT_INVALID"))
        }
        _ => Err(BackupExecutionFailure::terminal(
            "BACKUP_SCOPE_SNAPSHOT_INVALID",
        )),
    }
}

fn is_lower_hex_blake3(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn scope_dependency_policy(engine: &str, mode: &str) -> &'static str {
    if mode == "whole_database" {
        return "engine_whole_database_defaults";
    }
    match engine {
        "postgresql" => "selected_tables_only;dependent_objects_not_auto_included",
        "mysql" | "mariadb" => "selected_tables_and_table_triggers;routines_and_events_excluded",
        _ => "explicit_scope_not_supported",
    }
}

fn manifest_json_option(value: &str) -> Result<String, &'static str> {
    let parsed: serde_json::Value =
        serde_json::from_str(value).map_err(|_| "BACKUP_MANIFEST_INVALID")?;
    let canonical = serde_json::to_string(&parsed).map_err(|_| "BACKUP_MANIFEST_INVALID")?;
    if canonical.len() <= 1_024 {
        return Ok(canonical);
    }
    Ok(json!({ "blake3": blake3::hash(canonical.as_bytes()).to_hex().to_string() }).to_string())
}

fn manifest_text_option(value: &str) -> String {
    if !value.is_empty() && value.len() <= 1_024 && !value.contains(['\0', '\r', '\n']) {
        return value.to_owned();
    }
    json!({ "blake3": blake3::hash(value.as_bytes()).to_hex().to_string() }).to_string()
}

fn merge_source_manifest_evidence(
    options: &mut BTreeMap<String, String>,
    evidence: SourceManifestEvidence,
    default_consistency_grade: ConsistencyGrade,
) -> Result<ConsistencyGrade, &'static str> {
    for (key, value) in evidence.manifest_options {
        if options.insert(key, manifest_text_option(&value)).is_some() {
            return Err("BACKUP_MANIFEST_EVIDENCE_CONFLICT");
        }
    }
    Ok(evidence
        .consistency_grade
        .unwrap_or(default_consistency_grade))
}

fn classify_replica_results(
    results: impl IntoIterator<Item = (ReplicaRequirementV2, bool)>,
) -> Result<(usize, ClaimTerminalOutcome), &'static str> {
    let mut succeeded = 0;
    let mut required_failed = false;
    for (requirement, success) in results {
        if success {
            succeeded += 1;
        } else if requirement == ReplicaRequirementV2::Required {
            required_failed = true;
        }
    }
    if succeeded == 0 {
        return Err("BACKUP_ALL_REPLICAS_FAILED");
    }
    let outcome = if required_failed {
        ClaimTerminalOutcome::PartiallySucceeded
    } else {
        ClaimTerminalOutcome::Succeeded
    };
    Ok((succeeded, outcome))
}

const fn map_requirement(requirement: DestinationRequirement) -> ReplicaRequirementV2 {
    match requirement {
        DestinationRequirement::Required => ReplicaRequirementV2::Required,
        DestinationRequirement::BestEffort => ReplicaRequirementV2::BestEffort,
    }
}

const fn parse_engine(engine: &str) -> Option<Engine> {
    match engine.as_bytes() {
        b"postgresql" => Some(Engine::Postgresql),
        b"mysql" => Some(Engine::Mysql),
        b"mariadb" => Some(Engine::Mariadb),
        b"sqlite" => Some(Engine::Sqlite),
        _ => None,
    }
}

fn consistency_for_engine(engine: &str) -> ConsistencyGrade {
    if engine == "sqlite" {
        ConsistencyGrade::TransactionConsistent
    } else {
        ConsistencyGrade::ApplicationConsistent
    }
}

fn normalize_error_code(value: &str) -> &'static str {
    match value {
        "DESTINATION_SECRET_INVALID" => "BACKUP_DESTINATION_SECRET_INVALID",
        "DESTINATION_ENDPOINT_INVALID" => "BACKUP_DESTINATION_INVALID",
        "DESTINATION_KIND_UNSUPPORTED" => "BACKUP_DESTINATION_UNSUPPORTED",
        _ => "BACKUP_DESTINATION_INVALID",
    }
}

fn unix_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NATIVE_REGISTRATION_TEST_MODE: &str = "DBBACKUP_NATIVE_REGISTRATION_TEST_MODE";

    #[test]
    fn backup_handle_claim_runtime_stop_joins_all_threads() {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let stopped = Arc::new(AtomicBool::new(false));
        let thread_stopped = Arc::clone(&stopped);
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                thread::park_timeout(Duration::from_millis(5));
            }
            thread_stopped.store(true, Ordering::Release);
        });
        let handle = ServiceBackupWorkerHandle {
            stop,
            threads: Mutex::new(vec![thread]),
        };

        let runtime: &dyn crate::host::ClaimRuntime = &handle;
        runtime.request_stop();
        runtime.wait_stopped().expect("backup worker must join");

        assert!(stopped.load(Ordering::Acquire));
    }

    #[test]
    fn production_metadata_client_uses_the_install_bundle_for_both_vendors() {
        let temp = tempfile::TempDir::new().expect("install root");
        let mysql = temp
            .path()
            .join("tools")
            .join("mysql")
            .join("mysql8")
            .join(if cfg!(windows) { "mysql.exe" } else { "mysql" });
        let maria = temp
            .path()
            .join("tools")
            .join("mysql")
            .join("mariadb11")
            .join(if cfg!(windows) {
                "mariadb.exe"
            } else {
                "mariadb"
            });
        for client in [&mysql, &maria] {
            std::fs::create_dir_all(client.parent().expect("client parent"))
                .expect("bundle directory");
            std::fs::write(client, b"fixture").expect("bundle client");
        }

        let resolved_mysql =
            match resolve_mysql_metadata_client(temp.path(), ClientKind::OracleMySql) {
                Ok(path) => path,
                Err(_) => panic!("bundled MySQL metadata client must resolve"),
            };
        let resolved_maria = match resolve_mysql_metadata_client(temp.path(), ClientKind::MariaDb) {
            Ok(path) => path,
            Err(_) => panic!("bundled MariaDB metadata client must resolve"),
        };
        assert_eq!(
            resolved_mysql,
            std::fs::canonicalize(mysql).expect("canonical MySQL client")
        );
        assert_eq!(
            resolved_maria,
            std::fs::canonicalize(maria).expect("canonical MariaDB client")
        );
    }

    struct NeverPlanningSource;

    #[async_trait::async_trait]
    impl BackupSource for NeverPlanningSource {
        async fn plan(&self) -> anyhow::Result<dbbackup_pipeline::DumpPlan> {
            std::future::pending().await
        }

        fn spawn(
            &self,
            _part: &dbbackup_pipeline::DumpPlanPart,
        ) -> anyhow::Result<dbbackup_pipeline::DumpArtifact> {
            unreachable!("a timed-out plan must never spawn")
        }
    }

    #[tokio::test]
    async fn never_resolving_source_plan_hits_hard_execution_timeout() {
        let cancellation = ServiceBackupCancellation::default();
        let error = match plan_source_with_timeout(
            &NeverPlanningSource,
            &cancellation,
            Duration::from_millis(20),
        )
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("plan deadline must be enforced independently of the heartbeat thread"),
        };

        assert_eq!(error.code, "BACKUP_EXECUTION_TIMEOUT");
        assert!(error.retryable);
        assert_eq!(
            cancellation.cause(),
            Some(CancellationCause::ExecutionTimeout)
        );
    }

    #[cfg(windows)]
    struct NeverCompletingDestination {
        entered: Arc<AtomicBool>,
        dropped: Arc<AtomicBool>,
    }

    #[cfg(windows)]
    struct PendingDestinationCall {
        dropped: Arc<AtomicBool>,
    }

    #[cfg(windows)]
    impl Drop for PendingDestinationCall {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }

    #[cfg(windows)]
    #[async_trait::async_trait]
    impl dbbackup_pipeline::Destination for NeverCompletingDestination {
        fn name(&self) -> &str {
            "never-completing"
        }

        fn capabilities(&self) -> dbbackup_pipeline::DestCaps {
            dbbackup_pipeline::DestCaps {
                immutability: dbbackup_pipeline::ImmutabilityKind::None,
                checksum_required: true,
                resumable: true,
            }
        }

        async fn writer(
            &self,
            _key: &str,
        ) -> Result<dbbackup_pipeline::DestWriter, dbbackup_pipeline::DestError> {
            self.entered.store(true, Ordering::Release);
            let pending_call = PendingDestinationCall {
                dropped: Arc::clone(&self.dropped),
            };
            let result = std::future::pending().await;
            drop(pending_call);
            result
        }
    }

    #[cfg(windows)]
    #[async_trait::async_trait]
    impl dbbackup_pipeline::DestinationReaderV2 for NeverCompletingDestination {
        async fn probe_object(
            &self,
            _key: &str,
        ) -> Result<dbbackup_pipeline::ReaderObjectProbeV2, dbbackup_pipeline::DestError> {
            std::future::pending().await
        }

        async fn read_bytes_bounded(
            &self,
            _key: &str,
            _max_bytes: u64,
        ) -> Result<Vec<u8>, dbbackup_pipeline::DestError> {
            std::future::pending().await
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn never_completing_publish_returns_bounded_and_preserves_committing_recovery() {
        use std::process::Command;

        use dbbackup_catalog::{
            BackupWorkSpec, ConnectionProfileSpec, DatabaseTargetSpec, DestinationSpec,
            OccurrenceSpec, PlanDraftSpec,
        };
        use dbbackup_domain::{
            BackupPlanId, BackupRunId, ConnectionProfileId, DatabaseTargetId, DestinationId,
            DestinationRequirement, OccurrenceIdentity, OccurrenceOrigin, PlanRevisionId,
        };

        use crate::ipc::WindowsSid;
        use crate::secrets::AclPolicy;

        let temp = tempfile::TempDir::new().expect("temporary backup fixture");
        let store = OperationalStore::open(temp.path().join("operational.db"))
            .expect("open operational store");
        let whoami = Command::new("whoami")
            .args(["/user", "/fo", "csv", "/nh"])
            .output()
            .expect("query current user SID");
        assert!(whoami.status.success());
        let whoami = String::from_utf8(whoami.stdout).expect("whoami UTF-8");
        let owner_sid = whoami
            .trim()
            .trim_matches('"')
            .rsplit("\",\"")
            .next()
            .and_then(|value| WindowsSid::parse(value.trim_matches('"')).ok())
            .expect("current user SID");
        let acl = AclPolicy::new(
            WindowsSid::parse("S-1-5-80-1001").expect("service SID"),
            owner_sid,
        )
        .expect("secret ACL");
        let secrets = Arc::new(
            ServiceSecretStore::open(temp.path().join("secrets"), store.secret_refs(), acl)
                .expect("open secret store"),
        );
        let spool_root = temp.path().join("spool");
        let install_root = temp.path().join("install");
        std::fs::create_dir_all(&spool_root).expect("spool root");
        std::fs::create_dir_all(&install_root).expect("install root");

        let connection_id = ConnectionProfileId::new();
        let target_id = DatabaseTargetId::new();
        let destination_id = DestinationId::new();
        let plan_id = BackupPlanId::new();
        let revision_id = PlanRevisionId::new();
        let occurrence_id = OccurrenceIdentity::scheduled(
            revision_id.clone(),
            OccurrenceOrigin::Scheduled,
            unix_time_ms(),
        )
        .expect("occurrence identity")
        .deterministic_id(Uuid::NAMESPACE_URL);
        let backup_run_id = BackupRunId::new();
        let work_item_id = WorkItemId::new();
        let configured_at_ms = unix_time_ms();
        let configuration = store.configuration();
        configuration
            .insert_connection(ConnectionProfileSpec::file(
                connection_id.clone(),
                "deadline-sqlite",
                "sqlite",
                temp.path().join("source.sqlite").to_string_lossy(),
                configured_at_ms,
            ))
            .expect("insert connection");
        configuration
            .insert_target(DatabaseTargetSpec::new(
                target_id.clone(),
                connection_id,
                "file:deadline-sqlite",
                "source.sqlite",
                "source.sqlite",
                "source-fingerprint",
                configured_at_ms + 1,
            ))
            .expect("insert target");
        configuration
            .insert_destination(DestinationSpec::local(
                destination_id.clone(),
                "never-completing",
                serde_json::json!({ "path": temp.path().join("destination") }).to_string(),
                configured_at_ms + 2,
            ))
            .expect("insert destination");
        configuration
            .insert_plan_draft(PlanDraftSpec::manual(
                plan_id,
                revision_id.clone(),
                "deadline-plan",
                1,
                "a".repeat(64),
                configured_at_ms + 3,
            ))
            .expect("insert plan");
        configuration
            .add_revision_target(&revision_id, &target_id, configured_at_ms + 4)
            .expect("add target");
        configuration
            .add_revision_destination(
                &revision_id,
                &destination_id,
                DestinationRequirement::Required,
                configured_at_ms + 5,
            )
            .expect("add destination");
        configuration
            .publish_revision(&revision_id, configured_at_ms + 6)
            .expect("publish revision");

        let runtime = store.runtime();
        runtime
            .upsert_occurrence(OccurrenceSpec::scheduled(
                occurrence_id.clone(),
                revision_id.clone(),
                OccurrenceOrigin::Scheduled,
                configured_at_ms,
                configured_at_ms,
            ))
            .expect("insert occurrence");
        runtime
            .enqueue_backup_work(BackupWorkSpec::new(
                backup_run_id,
                work_item_id.clone(),
                occurrence_id,
                revision_id,
                target_id,
                "sqlite",
                "source-fingerprint",
                configured_at_ms,
            ))
            .expect("enqueue backup");
        let claim = runtime
            .claim_next(ClaimRequest::new(
                "deadline-worker",
                "deadline-boot",
                configured_at_ms + 10,
                LEASE_DURATION_MS,
            ))
            .expect("claim backup")
            .expect("backup claim");
        let context = runtime
            .backup_claim_context(&claim, configured_at_ms + 11)
            .expect("backup context");
        let executor = NativeBackupExecutor::new(
            store.clone(),
            secrets,
            install_root,
            spool_root,
            "deadline-instance",
        )
        .expect("native executor");
        let admitted_at_ms = unix_time_ms();
        let admission = match executor.admit_claim(&claim, &context, admitted_at_ms) {
            Ok(admission) => admission,
            Err(error) => panic!("admit backup: {}", error.code),
        };
        assert!(admission.admitted());
        runtime
            .start_claim(&claim, admitted_at_ms + 1)
            .expect("start backup");
        runtime
            .mark_in_process_claim_running(&claim, admitted_at_ms + 2)
            .expect("mark running");
        let work_spool = executor.work_spool(&claim);
        std::fs::create_dir_all(&work_spool).expect("work spool");
        let spool_path = work_spool.join("part.enc");
        std::fs::write(&spool_path, b"encrypted fixture").expect("spool payload");
        let tool_path = temp.path().join("sqlite3.exe");
        std::fs::write(&tool_path, b"fixture tool").expect("fixture tool");
        let part_id = Uuid::new_v4();
        let prepared_part = PreparedPart {
            plan: dbbackup_pipeline::DumpPlanPart {
                backup_id: part_id,
                job: "deadline-job".to_owned(),
                source_alias: "sqlite".to_owned(),
                kind: dbbackup_pipeline::DumpPartKind::Database {
                    name: "source.sqlite".to_owned(),
                },
                pairs_with_globals_backup_id: None,
                dump_tool: "sqlite3",
                dump_tool_path: tool_path.clone(),
                dump_tool_sha256: None,
                dump_tool_dependencies: Vec::new(),
                dump_tool_version: "test".to_owned(),
                dump_format: "sql",
                dump_flags: "sqlite backup".to_owned(),
                extensions: Vec::new(),
                charset: Some("UTF-8".to_owned()),
                collation: None,
            },
            commit: SpoolCommit {
                spool_path,
                plaintext_blake3: [1; 32],
                ciphertext_blake3: [2; 32],
                plaintext_bytes: 17,
                ciphertext_bytes: 17,
                stderr_tail: String::new(),
            },
            wrap_set: wrap_dek_with_kek(&executor.encryption_key, &Dek::generate(), 1)
                .expect("fixture wrap set"),
            tool_sha256: sha256_for_tool(&tool_path).expect("fixture tool identity"),
        };
        let entered = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        let never = Arc::new(NeverCompletingDestination {
            entered: Arc::clone(&entered),
            dropped: Arc::clone(&dropped),
        });
        let fingerprint = "never-completing-fingerprint".to_owned();
        let endpoint = ReplicaEndpointV2::new(fingerprint.clone(), never.clone(), never)
            .expect("never-completing endpoint");
        let cancellation = ServiceBackupCancellation::default();
        let deadline_ms = unix_time_ms() + 200;
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            executor.publish(
                &claim,
                &context,
                PreparedPublish {
                    parts: vec![prepared_part],
                    destinations: vec![PreparedDestination {
                        fingerprint,
                        kind: "local".to_owned(),
                        requirement: ReplicaRequirementV2::Required,
                        immutability: "none".to_owned(),
                        endpoint,
                    }],
                    work_spool: work_spool.clone(),
                    pipeline_config: PipelineCfg {
                        upload_retries: 0,
                        max_parallel_destinations: 1,
                        ..PipelineCfg::default()
                    },
                    tls_mode: "disabled".to_owned(),
                    source_evidence: SourceManifestEvidence::default(),
                    cancellation: cancellation.clone(),
                },
                deadline_ms,
            ),
        )
        .await
        .expect("publication must return within the outer safety bound");
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("never-completing publication must time out"),
        };
        assert_eq!(error.code, "BACKUP_EXECUTION_TIMEOUT");
        assert!(error.commit_started);
        assert!(entered.load(Ordering::Acquire));
        assert!(dropped.load(Ordering::Acquire));
        assert!(
            work_spool.exists(),
            "unknown commit outcome must retain spool"
        );

        let tick = settle_backup_execution_failure(
            &runtime,
            claim.clone(),
            work_item_id.clone(),
            unix_time_ms(),
            &cancellation,
            error,
        )
        .expect("preserve committing claim");
        assert_eq!(
            tick,
            BackupWorkerTick::AwaitingReconciliation(work_item_id.clone())
        );
        let expired = runtime
            .expired_claims(configured_at_ms + LEASE_DURATION_MS + 20, 10)
            .expect("expired committing claim")
            .into_iter()
            .find(|candidate| candidate.claim().work_item_id() == &work_item_id)
            .expect("committing recovery candidate");
        assert_eq!(expired.work_state(), "committing");
        assert_eq!(expired.attempt_state(), "committing");
        assert!(runtime
            .active_spool_paths(10)
            .expect("active spool reservations")
            .iter()
            .any(|path| Path::new(path) == work_spool));
        assert!(runtime
            .backup_commit_recovery_context(&claim)
            .expect("durable recovery intent")
            .recovery_point_id()
            .is_some());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn native_registration_persists_exact_identity_running_and_exit() {
        use std::process::Stdio;

        use dbbackup_catalog::{
            ConnectionProfileSpec, DatabaseTargetSpec, DestinationSpec,
            OccurrenceMaterializationSpec, PlanDraftSpec,
        };
        use dbbackup_domain::{
            BackupPlanId, ConnectionProfileId, DatabaseTargetId, DestinationId, OccurrenceOrigin,
            PlanRevisionId,
        };
        use dbbackup_pipeline::{
            DumpExit, DumpPartKind, DumpPlanPart, DumpStream, NativeChildProcess, NativeDumpStream,
        };

        let temp = tempfile::TempDir::new().expect("temporary catalog");
        let store =
            OperationalStore::open(temp.path().join("operational.db")).expect("operational store");
        let configuration = store.configuration();
        let connection_id = ConnectionProfileId::new();
        let target_id = DatabaseTargetId::new();
        let destination_id = DestinationId::new();
        let revision_id = PlanRevisionId::new();
        let now_ms = unix_time_ms();
        configuration
            .insert_connection(ConnectionProfileSpec::tcp(
                connection_id.clone(),
                "postgres",
                "postgresql",
                "db.internal",
                5432,
                now_ms - 100,
            ))
            .expect("connection");
        configuration
            .insert_target(DatabaseTargetSpec::new(
                target_id.clone(),
                connection_id,
                "database:app",
                "app",
                "app",
                "source-fingerprint",
                now_ms - 99,
            ))
            .expect("target");
        configuration
            .insert_destination(DestinationSpec::local(
                destination_id.clone(),
                "local",
                r#"{"path":"D:\\backup"}"#,
                now_ms - 98,
            ))
            .expect("destination");
        configuration
            .insert_plan_draft(PlanDraftSpec::manual(
                BackupPlanId::new(),
                revision_id.clone(),
                "native-registration",
                1,
                "a".repeat(64),
                now_ms - 97,
            ))
            .expect("plan");
        configuration
            .add_revision_target(&revision_id, &target_id, now_ms - 96)
            .expect("target membership");
        configuration
            .add_revision_destination(
                &revision_id,
                &destination_id,
                DestinationRequirement::Required,
                now_ms - 95,
            )
            .expect("destination membership");
        configuration
            .publish_revision(&revision_id, now_ms - 94)
            .expect("publish");
        let runtime = store.runtime();
        runtime
            .materialize_occurrence(OccurrenceMaterializationSpec::due(
                revision_id,
                OccurrenceOrigin::Scheduled,
                now_ms,
                now_ms,
            ))
            .expect("materialize");
        let claim = runtime
            .claim_next(ClaimRequest::new(
                "backup-worker",
                "service-boot",
                now_ms,
                LEASE_DURATION_MS,
            ))
            .expect("claim")
            .expect("backup claim");
        runtime.start_claim(&claim, now_ms).expect("start claim");

        let executable = std::env::current_exe().expect("test executable");
        let mut command = tokio::process::Command::new(&executable);
        command
            .args([
                "--exact",
                "backup::tests::native_registration_helper",
                "--nocapture",
            ])
            .env(NATIVE_REGISTRATION_TEST_MODE, "wait")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().expect("spawn native child");
        let child_pid = i64::from(child.id().expect("child PID"));
        let snapshot = dbbackup_runtime::snapshot_spawned_process(child_pid)
            .expect("snapshot native child identity");
        let process = NativeChildProcess::supervised(
            child_pid,
            snapshot.process_start_time_ms(),
            format!(r"Local\DbBackup.Job.{}", Uuid::new_v4()),
            snapshot.executable_path().to_path_buf(),
            dbbackup_runtime::hash_executable(snapshot.executable_path())
                .expect("native child digest"),
        )
        .expect("native child identity");
        let stdout = child.stdout.take().expect("child stdout");
        let (abort_sender, mut abort_receiver) = tokio::sync::oneshot::channel();
        let wait = Box::pin(async move {
            let status = tokio::select! {
                status = child.wait() => status?,
                _ = &mut abort_receiver => {
                    let _ = child.start_kill();
                    child.wait().await?
                }
            };
            Ok(DumpExit {
                code: status.code(),
                success: status.success(),
                stderr_tail: String::new(),
            })
        });
        let mut artifact = DumpArtifact::NativeStream(NativeDumpStream {
            process,
            stream: DumpStream {
                reader: Box::new(stdout),
                wait,
                abort: Some(Box::new(move || {
                    let _ = abort_sender.send(());
                })),
            },
        });
        let part = DumpPlanPart {
            backup_id: Uuid::new_v4(),
            job: "native-registration".to_owned(),
            source_alias: "postgres".to_owned(),
            kind: DumpPartKind::Database {
                name: "app".to_owned(),
            },
            pairs_with_globals_backup_id: None,
            dump_tool: "pg_dump",
            dump_tool_path: executable,
            dump_tool_sha256: None,
            dump_tool_dependencies: Vec::new(),
            dump_tool_version: "test".to_owned(),
            dump_format: "custom",
            dump_flags: "pg_dump -Fc -f - app".to_owned(),
            extensions: Vec::new(),
            charset: Some("UTF8".to_owned()),
            collation: None,
        };

        if register_backup_artifact_process(&runtime, &claim, &part, &mut artifact, unix_time_ms)
            .is_err()
        {
            panic!("record exact native identity");
        }
        let running = runtime
            .expired_claims(now_ms + LEASE_DURATION_MS + 1, 1)
            .expect("inspect running process")
            .pop()
            .expect("expired candidate");
        let recorded = running.process().expect("recorded native process");
        assert_eq!(recorded.state(), "running");
        assert_eq!(recorded.service_boot_id(), "service-boot");
        assert!(Path::new(recorded.executable_path()).is_absolute());
        assert_eq!(recorded.executable_sha256().map(str::len), Some(64));
        assert!(recorded
            .job_object_key()
            .starts_with(r"Local\DbBackup.Job."));
        assert!(!recorded.job_object_key().contains(".Unowned."));

        let cancellation = ServiceBackupCancellation::default();
        let deadline_result = run_artifact_until_deadline(
            artifact,
            temp.path().join("native-timeout.enc"),
            Dek::generate(),
            &PipelineCfg::default(),
            cancellation.pipeline.clone(),
            cancellation.clone(),
            Duration::from_millis(20),
        )
        .await;
        assert!(deadline_result.is_err());
        assert_eq!(
            cancellation.retry_error_code(),
            Some("BACKUP_EXECUTION_TIMEOUT")
        );
        let exited = runtime
            .expired_claims(now_ms + LEASE_DURATION_MS + 1, 1)
            .expect("inspect exited process")
            .pop()
            .expect("expired candidate");
        assert_eq!(exited.process().expect("process row").state(), "exited");
    }

    #[test]
    fn native_registration_helper() {
        if std::env::var(NATIVE_REGISTRATION_TEST_MODE).as_deref() == Ok("wait") {
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }

    #[test]
    fn source_gate_serializes_each_connection_without_blocking_other_sources() {
        assert_eq!(DEFAULT_WORKER_THREADS, 4);
        let gate = Arc::new(SourceGate::default());
        let first = gate
            .try_acquire("connection-a".to_owned())
            .expect("first claim acquires its source");
        assert!(gate.try_acquire("connection-a".to_owned()).is_none());
        assert!(gate.try_acquire("connection-b".to_owned()).is_some());
        drop(first);
        assert!(gate.try_acquire("connection-a".to_owned()).is_some());
    }

    #[test]
    fn cancellation_cause_preserves_first_safety_signal_and_retry_class() {
        let cancellation = ServiceBackupCancellation::default();
        cancellation.cancel_with(CancellationCause::ExecutionTimeout);
        cancellation.cancel_with(CancellationCause::UserRequested);

        assert_eq!(
            cancellation.cause(),
            Some(CancellationCause::ExecutionTimeout)
        );
        assert_eq!(
            cancellation.retry_error_code(),
            Some("BACKUP_EXECUTION_TIMEOUT")
        );
        assert!(cancellation.is_cancelled());
    }

    #[test]
    fn monitor_policy_detects_deadline_and_disk_floor_without_waiting() {
        let cancellation = ServiceBackupCancellation::default();
        apply_monitor_policy(&cancellation, 10_001, 10_000, Ok(8_000), 4_000);
        assert_eq!(
            cancellation.cause(),
            Some(CancellationCause::ExecutionTimeout)
        );

        let cancellation = ServiceBackupCancellation::default();
        apply_monitor_policy(&cancellation, 9_999, 10_000, Ok(3_999), 4_000);
        assert_eq!(cancellation.cause(), Some(CancellationCause::DiskFloor));
        assert_eq!(
            cancellation.retry_error_code(),
            Some("BACKUP_SPOOL_DISK_LOW")
        );
    }

    #[test]
    fn managed_spool_cleanup_removes_nested_attempt_content_only() {
        let temp = tempfile::TempDir::new().expect("temp spool root");
        let root = temp.path().join("spool");
        let attempt = root.join(Uuid::new_v4().to_string()).join("attempt-1-1");
        let nested = attempt.join("nested");
        std::fs::create_dir_all(&nested).expect("nested attempt spool");
        std::fs::write(nested.join("partial.enc"), b"partial").expect("partial spool");
        let unrelated = root.join("operator-note.txt");
        std::fs::write(&unrelated, b"preserve").expect("unrelated file");

        cleanup_managed_attempt_spool(&root, &attempt).expect("cleanup managed attempt");

        assert!(!attempt.exists());
        assert!(unrelated.exists());
        assert!(cleanup_managed_attempt_spool(&root, &root).is_err());
    }

    #[test]
    fn startup_spool_scavenger_preserves_active_and_unmanaged_paths() {
        let temp = tempfile::TempDir::new().expect("temp spool root");
        let root = temp.path().join("spool");
        let active = root.join(Uuid::new_v4().to_string()).join("attempt-1-1");
        let orphan = root.join(Uuid::new_v4().to_string()).join("attempt-2-3");
        let unmanaged = root.join("operator-owned").join("attempt-1-1");
        for path in [&active, &orphan, &unmanaged] {
            std::fs::create_dir_all(path).expect("spool fixture");
            std::fs::write(path.join("part.enc"), b"fixture").expect("spool part");
        }
        let active = std::fs::canonicalize(&active).expect("active canonical path");

        let report = scavenge_managed_spool(&root, &HashSet::from([active.clone()]))
            .expect("bounded spool scavenge");

        assert_eq!(report.removed_attempts(), 1);
        assert_eq!(report.preserved_active(), 1);
        assert_eq!(report.skipped_unmanaged(), 1);
        assert!(active.exists());
        assert!(!orphan.exists());
        assert!(unmanaged.exists());
    }

    #[test]
    fn manifest_options_preserve_small_json_and_bound_large_policy_evidence() {
        assert_eq!(
            manifest_json_option("{\"mode\":\"all\"}").unwrap(),
            "{\"mode\":\"all\"}"
        );
        let large = serde_json::to_string(&json!({ "value": "x".repeat(2_000) })).unwrap();
        let bounded = manifest_json_option(&large).expect("large policy is represented by digest");
        assert!(bounded.len() <= 1_024);
        assert!(bounded.contains("blake3"));
        assert!(!bounded.contains(&"x".repeat(1_100)));
    }

    #[test]
    fn source_evidence_is_bounded_and_can_downgrade_consistency() {
        let mut options = BTreeMap::new();
        let evidence = dbbackup_pipeline::SourceManifestEvidence {
            manifest_options: BTreeMap::from([
                ("mysql.serverVersion".to_owned(), "8.4.5".to_owned()),
                ("mysql.nonTransactionalTables".to_owned(), "x".repeat(2_000)),
            ]),
            consistency_grade: Some(ConsistencyGrade::CrashConsistent),
        };

        let grade = merge_source_manifest_evidence(
            &mut options,
            evidence,
            ConsistencyGrade::ApplicationConsistent,
        )
        .expect("source evidence");

        assert_eq!(grade, ConsistencyGrade::CrashConsistent);
        assert_eq!(options["mysql.serverVersion"], "8.4.5");
        assert!(options["mysql.nonTransactionalTables"].len() <= 1_024);
        assert!(options["mysql.nonTransactionalTables"].contains("blake3"));
    }

    #[test]
    fn stored_scope_snapshot_is_verified_and_converted_to_provider_selection() {
        let snapshot = concat!(
            "{\"mode\":\"explicit_tables\",\"objects\":[",
            "{\"engineObjectId\":\"oid-2\",\"kind\":\"table\",\"schema\":\"sales\",",
            "\"name\":\"orders\",\"objectFingerprintBlake3\":",
            "\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"},",
            "{\"engineObjectId\":\"oid-1\",\"kind\":\"table\",\"schema\":\"public\",",
            "\"name\":\"accounts\",\"objectFingerprintBlake3\":",
            "\"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\"}]}"
        );
        let hash = blake3::hash(snapshot.as_bytes()).to_hex().to_string();

        let selection = match parse_scope_selection(snapshot, "explicit_tables", 2, &hash) {
            Ok(selection) => selection,
            Err(_) => panic!("valid persisted scope"),
        };

        assert_eq!(selection.tables().len(), 2);
        assert_eq!(selection.tables()[0].schema_name(), "public");
        assert_eq!(selection.tables()[0].table_name(), "accounts");
        assert_eq!(selection.tables()[1].schema_name(), "sales");
        assert_eq!(selection.tables()[1].table_name(), "orders");
    }

    #[test]
    fn stored_scope_snapshot_fails_closed_when_evidence_does_not_match() {
        let snapshot = "{\"mode\":\"whole_database\",\"objects\":[]}";
        let error = parse_scope_selection(snapshot, "whole_database", 0, &"0".repeat(64))
            .expect_err("snapshot digest mismatch must fail closed");

        assert_eq!(error.code, "BACKUP_SCOPE_SNAPSHOT_INVALID");
    }

    #[test]
    fn partial_scope_manifest_policy_states_engine_dependency_limits() {
        assert_eq!(
            scope_dependency_policy("postgresql", "explicit_tables"),
            "selected_tables_only;dependent_objects_not_auto_included"
        );
        assert_eq!(
            scope_dependency_policy("mysql", "explicit_tables"),
            "selected_tables_and_table_triggers;routines_and_events_excluded"
        );
        assert_eq!(
            scope_dependency_policy("mariadb", "whole_database"),
            "engine_whole_database_defaults"
        );
    }

    #[test]
    fn replica_outcome_only_downgrades_when_required_destination_fails() {
        assert_eq!(
            classify_replica_results([
                (ReplicaRequirementV2::Required, true),
                (ReplicaRequirementV2::BestEffort, false),
            ]),
            Ok((1, ClaimTerminalOutcome::Succeeded))
        );
        assert_eq!(
            classify_replica_results([
                (ReplicaRequirementV2::Required, false),
                (ReplicaRequirementV2::BestEffort, true),
            ]),
            Ok((1, ClaimTerminalOutcome::PartiallySucceeded))
        );
        assert_eq!(
            classify_replica_results([
                (ReplicaRequirementV2::Required, false),
                (ReplicaRequirementV2::BestEffort, false),
            ]),
            Err("BACKUP_ALL_REPLICAS_FAILED")
        );
    }
}
