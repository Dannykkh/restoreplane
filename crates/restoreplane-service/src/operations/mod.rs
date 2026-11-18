use std::collections::BTreeSet;
use std::io::Read;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use restoreplane_catalog::{
    ActivityEventSpec, BeginCommitDisposition, ClaimCompletionSpec, ClaimRequest,
    ConnectionExecutionBinding, ConnectionProbeSpec, DatabaseObjectDiscoverySpec,
    DestinationProbeSpec, DestinationSecretRoleV2, DiscoveredDatabaseObjectSpec,
    DiscoveredDatabaseSpec, DiscoverySnapshotSpec, OperationalStore, RepositoryError,
    RetryDisposition, SelectedDatabaseTarget,
};
use restoreplane_domain::{ConnectionProfileId, DatabaseTargetId, DestinationId, WorkItemId};
use serde::Deserialize;
use serde_json::{json, Value};
use thiserror::Error;
use zeroize::Zeroizing;

use crate::host::{ClaimRuntime, HostError};
use crate::secrets::{SecretPurpose, ServiceSecretStore};
use crate::support_bundle::{SupportBundleBuilder, SupportBundleInput, SupportBundleReport};
use crate::telemetry_archive::TelemetryArchiveService;

pub(crate) mod destination;
mod native;

use destination::{inspect_destination, DestinationInspection};
pub use native::NativeConnectionInspector;

const LEASE_DURATION_MS: i64 = 30_000;
const RETRY_DELAY_MS: i64 = 1_000;
const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(250);
const TELEMETRY_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1_000;
const TELEMETRY_ARCHIVE_INTERVAL_MS: i64 = 60 * 60 * 1_000;
const TELEMETRY_ARCHIVE_BATCH_ROWS: usize = 5_000;
const TELEMETRY_ARCHIVE_MAX_BATCHES_PER_TICK: usize = 10;
const SUPPORTED_OPERATION_KINDS: [&str; 4] = [
    "connection_probe",
    "connection_discovery",
    "destination_probe",
    "support_bundle",
];

pub trait ConnectionInspector: Send + Sync + 'static {
    fn inspect(
        &self,
        profile: &ConnectionExecutionBinding,
        credentials: Option<&DatabaseCredentials>,
        discover: bool,
    ) -> Result<ConnectionInspection, ConnectionOperationError>;

    fn inspect_database_objects(
        &self,
        _profile: &ConnectionExecutionBinding,
        _credentials: Option<&DatabaseCredentials>,
        _target: &SelectedDatabaseTarget,
    ) -> Result<DatabaseObjectInspection, ConnectionOperationError> {
        Err(ConnectionOperationError::terminal(
            "BACKUP_SCOPE_DISCOVERY_UNSUPPORTED",
            "unsupported_version",
            "Choose PostgreSQL, MySQL, or MariaDB for table-level backup scope discovery.",
        ))
    }

    fn inspect_selected_structure(
        &self,
        _profile: &ConnectionExecutionBinding,
        _credentials: Option<&DatabaseCredentials>,
        _target: &SelectedDatabaseTarget,
        _items: &[restoreplane_domain::workbench_protection::WorkbenchSelectionItem],
    ) -> Result<SelectedStructureInspection, ConnectionOperationError> {
        Err(ConnectionOperationError::terminal(
            "WORKBENCH_STRUCTURE_UNSUPPORTED",
            "unsupported_version",
            "이 엔진의 선택 구조 검사를 지원하지 않습니다.",
        ))
    }
}

pub struct SelectedStructureInspection {
    pub items: Vec<restoreplane_domain::workbench_protection::WorkbenchSelectionItem>,
    pub server_identity: String,
}

pub struct DatabaseCredentials {
    username: Zeroizing<String>,
    password: Zeroizing<String>,
}

impl DatabaseCredentials {
    pub fn username(&self) -> &str {
        self.username.as_str()
    }

    pub fn password(&self) -> &str {
        self.password.as_str()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ConnectionInspection {
    server_version: String,
    capabilities: Value,
    databases: Vec<DiscoveredDatabase>,
}

impl ConnectionInspection {
    pub fn new(
        server_version: impl Into<String>,
        capabilities: Value,
        databases: Vec<DiscoveredDatabase>,
    ) -> Self {
        Self {
            server_version: server_version.into(),
            capabilities,
            databases,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredDatabase {
    engine_object_id: String,
    database_name: String,
    normalized_name: String,
    eligible: bool,
    exclusion_reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseObjectInspection {
    objects: Vec<DiscoveredDatabaseObject>,
    workbench_selection: Option<restoreplane_domain::workbench_protection::WorkbenchSelectionProof>,
}

impl DatabaseObjectInspection {
    pub fn new(mut objects: Vec<DiscoveredDatabaseObject>) -> Self {
        objects.sort_by(|left, right| {
            (&left.schema_name, &left.object_name, &left.engine_object_id).cmp(&(
                &right.schema_name,
                &right.object_name,
                &right.engine_object_id,
            ))
        });
        Self {
            objects,
            workbench_selection: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredDatabaseObject {
    engine_object_id: String,
    schema_name: String,
    object_name: String,
    object_type: String,
    eligible: bool,
    exclusion_reason: Option<String>,
}

impl DiscoveredDatabaseObject {
    pub fn eligible_table(
        engine_object_id: impl Into<String>,
        schema_name: impl Into<String>,
        object_name: impl Into<String>,
    ) -> Self {
        Self {
            engine_object_id: engine_object_id.into(),
            schema_name: schema_name.into(),
            object_name: object_name.into(),
            object_type: "table".to_owned(),
            eligible: true,
            exclusion_reason: None,
        }
    }

    pub fn excluded(
        engine_object_id: impl Into<String>,
        schema_name: impl Into<String>,
        object_name: impl Into<String>,
        object_type: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            engine_object_id: engine_object_id.into(),
            schema_name: schema_name.into(),
            object_name: object_name.into(),
            object_type: object_type.into(),
            eligible: false,
            exclusion_reason: Some(reason.into()),
        }
    }
}

impl DiscoveredDatabase {
    pub fn eligible(engine_object_id: impl Into<String>, database_name: impl Into<String>) -> Self {
        let database_name = database_name.into();
        Self {
            engine_object_id: engine_object_id.into(),
            normalized_name: database_name.to_lowercase(),
            database_name,
            eligible: true,
            exclusion_reason: None,
        }
    }

    pub fn excluded(
        engine_object_id: impl Into<String>,
        database_name: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        let database_name = database_name.into();
        Self {
            engine_object_id: engine_object_id.into(),
            normalized_name: database_name.to_lowercase(),
            database_name,
            eligible: false,
            exclusion_reason: Some(reason.into()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionOperationError {
    code: String,
    failure_class: &'static str,
    retryable: bool,
    operator_action: String,
}

impl ConnectionOperationError {
    pub fn terminal(
        code: impl Into<String>,
        failure_class: &'static str,
        operator_action: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            failure_class,
            retryable: false,
            operator_action: operator_action.into(),
        }
    }

    pub fn retryable(
        code: impl Into<String>,
        failure_class: &'static str,
        operator_action: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            failure_class,
            retryable: true,
            operator_action: operator_action.into(),
        }
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    pub const fn is_retryable(&self) -> bool {
        self.retryable
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkerTick {
    Idle,
    Completed(WorkItemId),
    Failed(WorkItemId),
    Retried(WorkItemId),
    Cancelled(WorkItemId),
}

enum OperationInspection {
    Connection(ConnectionInspection),
    DatabaseObjects(DatabaseObjectInspection),
    Destination(DestinationInspection),
    SupportBundle(SupportBundleReport),
}

struct PersistedOperation {
    result_json: String,
    terminal_error_code: Option<&'static str>,
}

impl PersistedOperation {
    fn succeeded(result_json: String) -> Self {
        Self {
            result_json,
            terminal_error_code: None,
        }
    }

    fn failed(result_json: String, error_code: &'static str) -> Self {
        Self {
            result_json,
            terminal_error_code: Some(error_code),
        }
    }
}

pub struct ServiceOperationWorkerHandle {
    stop: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl ServiceOperationWorkerHandle {
    pub fn spawn(
        store: OperationalStore,
        secrets: Arc<ServiceSecretStore>,
    ) -> Result<Self, OperationWorkerStartError> {
        Self::spawn_with_support_bundle_root(
            store,
            secrets,
            std::env::temp_dir().join("restoreplane-support-bundles"),
        )
    }

    pub fn spawn_with_support_bundle_root(
        store: OperationalStore,
        secrets: Arc<ServiceSecretStore>,
        support_bundle_root: PathBuf,
    ) -> Result<Self, OperationWorkerStartError> {
        Self::spawn_with_maintenance_roots(store, secrets, support_bundle_root, None)
    }

    pub fn spawn_with_maintenance_roots(
        store: OperationalStore,
        secrets: Arc<ServiceSecretStore>,
        support_bundle_root: PathBuf,
        telemetry_archive_root: Option<PathBuf>,
    ) -> Result<Self, OperationWorkerStartError> {
        Self::spawn_with_runtime_roots(
            store,
            secrets,
            support_bundle_root,
            telemetry_archive_root,
            None,
        )
    }

    pub fn spawn_with_runtime_roots(
        store: OperationalStore,
        secrets: Arc<ServiceSecretStore>,
        support_bundle_root: PathBuf,
        telemetry_archive_root: Option<PathBuf>,
        native_tool_root: Option<PathBuf>,
    ) -> Result<Self, OperationWorkerStartError> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let worker_id = format!("connection-worker-{}", uuid::Uuid::new_v4());
        let worker_boot_id = uuid::Uuid::new_v4().to_string();
        let thread = thread::Builder::new()
            .name("restoreplane-connection-worker".to_owned())
            .spawn(move || {
                let inspector = match native_tool_root {
                    Some(root) => {
                        NativeConnectionInspector::with_trusted_tool_root(root, store.clone())
                    }
                    None => NativeConnectionInspector::default(),
                };
                let worker = ServiceOperationWorker::new(
                    store,
                    Some(secrets),
                    Arc::new(inspector),
                    worker_id,
                    worker_boot_id,
                )
                .with_support_bundle_root(support_bundle_root);
                let telemetry_archive = telemetry_archive_root.and_then(|root| {
                    match TelemetryArchiveService::new(worker.store.clone(), root) {
                        Ok(service) => Some(service),
                        Err(error) => {
                            tracing::warn!(
                                error_code = error.code(),
                                "telemetry archive maintenance is unavailable"
                            );
                            None
                        }
                    }
                });
                let mut next_telemetry_archive_at_ms = 0_i64;
                while !thread_stop.load(Ordering::Acquire) {
                    let now_ms = unix_time_ms();
                    if now_ms >= next_telemetry_archive_at_ms {
                        if let Err(error) =
                            run_telemetry_maintenance(telemetry_archive.as_ref(), now_ms)
                        {
                            tracing::warn!(
                                error_code = error,
                                "bounded telemetry archive maintenance failed"
                            );
                        }
                        next_telemetry_archive_at_ms =
                            now_ms.saturating_add(TELEMETRY_ARCHIVE_INTERVAL_MS);
                    }
                    match worker.run_once() {
                        Ok(WorkerTick::Idle) => thread::park_timeout(IDLE_POLL_INTERVAL),
                        Ok(_) => {}
                        Err(error) => {
                            tracing::error!(
                                error_code = error.code(),
                                "connection operation worker tick failed"
                            );
                            thread::park_timeout(IDLE_POLL_INTERVAL);
                        }
                    }
                }
            })
            .map_err(OperationWorkerStartError::Spawn)?;
        Ok(Self {
            stop,
            thread: Mutex::new(Some(thread)),
        })
    }

    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
        if let Ok(thread) = self.thread.lock() {
            if let Some(thread) = thread.as_ref() {
                thread.thread().unpark();
            }
        }
    }

    pub fn wait_stopped(&self) -> Result<(), HostError> {
        let thread = self
            .thread
            .lock()
            .map_err(|_| HostError::InternalState)?
            .take();
        if let Some(thread) = thread {
            thread.join().map_err(|_| HostError::RuntimeFailed)?;
        }
        Ok(())
    }
}

impl ClaimRuntime for ServiceOperationWorkerHandle {
    fn request_stop(&self) {
        Self::request_stop(self);
    }

    fn wait_stopped(&self) -> Result<(), HostError> {
        Self::wait_stopped(self)
    }
}

fn run_telemetry_maintenance(
    service: Option<&TelemetryArchiveService>,
    now_ms: i64,
) -> Result<(), &'static str> {
    let Some(service) = service else {
        return Ok(());
    };
    if now_ms < TELEMETRY_RETENTION_MS {
        return Err("TELEMETRY_ARCHIVE_CLOCK_INVALID");
    }
    let cutoff_ms = now_ms.saturating_sub(TELEMETRY_RETENTION_MS);
    for _ in 0..TELEMETRY_ARCHIVE_MAX_BATCHES_PER_TICK {
        match service.archive_before(cutoff_ms, TELEMETRY_ARCHIVE_BATCH_ROWS, now_ms) {
            Ok(Some(_)) => {}
            Ok(None) => return Ok(()),
            Err(error) => return Err(error.code()),
        }
    }
    Ok(())
}

impl Drop for ServiceOperationWorkerHandle {
    fn drop(&mut self) {
        self.request_stop();
        let _ = self.wait_stopped();
    }
}

pub struct ServiceOperationWorker<I: ConnectionInspector> {
    store: OperationalStore,
    secrets: Option<Arc<ServiceSecretStore>>,
    inspector: Arc<I>,
    worker_id: String,
    worker_boot_id: String,
    support_bundle_root: PathBuf,
    clock: fn() -> i64,
}

impl<I: ConnectionInspector> ServiceOperationWorker<I> {
    pub fn new(
        store: OperationalStore,
        secrets: Option<Arc<ServiceSecretStore>>,
        inspector: Arc<I>,
        worker_id: impl Into<String>,
        worker_boot_id: impl Into<String>,
    ) -> Self {
        Self::with_clock(
            store,
            secrets,
            inspector,
            worker_id,
            worker_boot_id,
            unix_time_ms,
        )
    }

    pub fn with_clock(
        store: OperationalStore,
        secrets: Option<Arc<ServiceSecretStore>>,
        inspector: Arc<I>,
        worker_id: impl Into<String>,
        worker_boot_id: impl Into<String>,
        clock: fn() -> i64,
    ) -> Self {
        Self {
            store,
            secrets,
            inspector,
            worker_id: worker_id.into(),
            worker_boot_id: worker_boot_id.into(),
            support_bundle_root: std::env::temp_dir().join("restoreplane-support-bundles"),
            clock,
        }
    }

    pub fn with_support_bundle_root(mut self, support_bundle_root: PathBuf) -> Self {
        self.support_bundle_root = support_bundle_root;
        self
    }

    pub fn run_once(&self) -> Result<WorkerTick, OperationWorkerError> {
        let started_at_ms = self.now()?;
        let runtime = self.store.runtime();
        let Some(claim) = runtime.claim_next(
            ClaimRequest::new(
                &self.worker_id,
                &self.worker_boot_id,
                started_at_ms,
                LEASE_DURATION_MS,
            )
            .for_operation_kinds(SUPPORTED_OPERATION_KINDS),
        )?
        else {
            return Ok(WorkerTick::Idle);
        };
        let work_item_id = claim.work_item_id().clone();
        let context = runtime.operation_claim_context(&claim, started_at_ms)?;
        if runtime.claim_cancel_requested(&claim, started_at_ms)? {
            runtime.complete_claim(ClaimCompletionSpec::cancelled(
                claim,
                started_at_ms,
                Some("CANCEL_REQUESTED".to_owned()),
                None,
            ))?;
            self.append_event(
                started_at_ms,
                "CONNECTION_OPERATION_CANCELLED",
                &work_item_id,
                "The connection operation was cancelled before it started.",
                false,
            )?;
            return Ok(WorkerTick::Cancelled(work_item_id));
        }

        runtime.start_claim(&claim, started_at_ms)?;
        runtime.mark_in_process_claim_running(&claim, started_at_ms)?;
        let execution = self.execute_operation(&context);
        let completed_at_ms = self.now()?.max(started_at_ms);
        match execution {
            Ok(result) => {
                if runtime.claim_cancel_requested(&claim, completed_at_ms)? {
                    runtime.complete_claim(ClaimCompletionSpec::cancelled(
                        claim,
                        completed_at_ms,
                        Some("CANCEL_REQUESTED".to_owned()),
                        None,
                    ))?;
                    self.append_event(
                        completed_at_ms,
                        "CONNECTION_OPERATION_CANCELLED",
                        &work_item_id,
                        "The connection operation was cancelled before evidence was committed.",
                        false,
                    )?;
                    return Ok(WorkerTick::Cancelled(work_item_id));
                }
                let persisted =
                    self.persist_operation(&context, &result, started_at_ms, completed_at_ms)?;
                match runtime.begin_claim_commit(&claim, completed_at_ms)? {
                    BeginCommitDisposition::Committing => {
                        if let Some(error_code) = persisted.terminal_error_code {
                            runtime.complete_claim(ClaimCompletionSpec::failed(
                                claim,
                                completed_at_ms,
                                error_code,
                                Some(persisted.result_json),
                            ))?;
                            self.append_event(
                                completed_at_ms,
                                "DESTINATION_PROBE_FAILED",
                                &work_item_id,
                                "The service committed failed Destination role evidence.",
                                true,
                            )?;
                            Ok(WorkerTick::Failed(work_item_id))
                        } else {
                            runtime.complete_claim(ClaimCompletionSpec::succeeded(
                                claim,
                                completed_at_ms,
                                Some(persisted.result_json),
                            ))?;
                            let destination = context.operation_kind() == "destination_probe";
                            let support_bundle = context.operation_kind() == "support_bundle";
                            self.append_event(
                                completed_at_ms,
                                if destination {
                                    "DESTINATION_PROBE_SUCCEEDED"
                                } else if support_bundle {
                                    "SUPPORT_BUNDLE_SUCCEEDED"
                                } else {
                                    "CONNECTION_OPERATION_SUCCEEDED"
                                },
                                &work_item_id,
                                if destination {
                                    "The service committed independent Destination role evidence."
                                } else if support_bundle {
                                    "The service created a bounded redacted support bundle."
                                } else {
                                    "The service committed connection evidence."
                                },
                                false,
                            )?;
                            Ok(WorkerTick::Completed(work_item_id))
                        }
                    }
                    BeginCommitDisposition::CancelRequested => {
                        runtime.complete_claim(ClaimCompletionSpec::cancelled(
                            claim,
                            completed_at_ms,
                            Some("CANCEL_REQUESTED".to_owned()),
                            None,
                        ))?;
                        Ok(WorkerTick::Cancelled(work_item_id))
                    }
                }
            }
            Err(error) if error.retryable => {
                let terminal_result = serde_json::to_string(&json!({
                    "schemaVersion": 1,
                    "operatorAction": error.operator_action,
                }))
                .map_err(|_| OperationWorkerError::ResultEncoding)?;
                let disposition = runtime.retry_claim(
                    &claim,
                    completed_at_ms,
                    completed_at_ms.saturating_add(RETRY_DELAY_MS),
                    error.code,
                    Some(terminal_result),
                )?;
                match disposition {
                    RetryDisposition::Requeued => Ok(WorkerTick::Retried(work_item_id)),
                    RetryDisposition::Exhausted => Ok(WorkerTick::Failed(work_item_id)),
                    RetryDisposition::Cancelled => Ok(WorkerTick::Cancelled(work_item_id)),
                }
            }
            Err(error) => {
                if context.operation_kind() == "connection_probe" {
                    self.store.configuration().record_connection_probe(
                        ConnectionProbeSpec::failed(
                            context.operation_run_id().as_str(),
                            ConnectionProfileId::from_str(context.subject_id())
                                .map_err(|_| OperationWorkerError::RequestInvalid)?,
                            error.failure_class,
                            "{}",
                            &error.operator_action,
                            started_at_ms,
                            completed_at_ms,
                        ),
                    )?;
                } else if context.operation_kind() == "destination_probe" {
                    self.persist_destination_failure(
                        &context,
                        &error,
                        started_at_ms,
                        completed_at_ms,
                    )?;
                }
                let result_json = serde_json::to_string(&json!({
                    "schemaVersion": 1,
                    "operatorAction": error.operator_action,
                }))
                .map_err(|_| OperationWorkerError::ResultEncoding)?;
                runtime.complete_claim(ClaimCompletionSpec::failed(
                    claim,
                    completed_at_ms,
                    error.code,
                    Some(result_json),
                ))?;
                self.append_event(
                    completed_at_ms,
                    if context.operation_kind() == "destination_probe" {
                        "DESTINATION_PROBE_FAILED"
                    } else if context.operation_kind() == "support_bundle" {
                        "SUPPORT_BUNDLE_FAILED"
                    } else {
                        "CONNECTION_OPERATION_FAILED"
                    },
                    &work_item_id,
                    if context.operation_kind() == "destination_probe" {
                        "The Destination probe failed. Review the stable error code and retry action."
                    } else if context.operation_kind() == "support_bundle" {
                        "The support bundle failed closed. Review diagnostics and the stable error code."
                    } else {
                        "The connection operation failed. Review the stable error code and retry action."
                    },
                    true,
                )?;
                Ok(WorkerTick::Failed(work_item_id))
            }
        }
    }

    fn execute_operation(
        &self,
        context: &restoreplane_catalog::OperationClaimContext,
    ) -> Result<OperationInspection, ConnectionOperationError> {
        match context.operation_kind() {
            "connection_discovery" if context.subject_type() == "database_target" => self
                .execute_database_object_operation(context)
                .map(OperationInspection::DatabaseObjects),
            "connection_probe" | "connection_discovery" => self
                .execute_connection_operation(context)
                .map(OperationInspection::Connection),
            "destination_probe" => self
                .execute_destination_operation(context)
                .map(OperationInspection::Destination),
            "support_bundle" => self
                .execute_support_bundle(context)
                .map(OperationInspection::SupportBundle),
            _ => Err(ConnectionOperationError::terminal(
                "OPERATION_KIND_UNSUPPORTED",
                "unknown",
                "Create a new operation from the management console.",
            )),
        }
    }

    fn execute_connection_operation(
        &self,
        context: &restoreplane_catalog::OperationClaimContext,
    ) -> Result<ConnectionInspection, ConnectionOperationError> {
        if context.subject_type() != "connection_profile" {
            return Err(ConnectionOperationError::terminal(
                "CONNECTION_REQUEST_INVALID",
                "unknown",
                "Create a new connection operation from the Connections page.",
            ));
        }
        let request: ConnectionOperationRequest = serde_json::from_str(context.request_json())
            .map_err(|_| {
                ConnectionOperationError::terminal(
                    "CONNECTION_REQUEST_INVALID",
                    "unknown",
                    "Create a new connection operation from the Connections page.",
                )
            })?;
        if request.schema_version != 1 || request.connection_id != context.subject_id() {
            return Err(ConnectionOperationError::terminal(
                "CONNECTION_REQUEST_INVALID",
                "unknown",
                "Create a new connection operation from the Connections page.",
            ));
        }
        let connection_id =
            ConnectionProfileId::from_str(&request.connection_id).map_err(|_| {
                ConnectionOperationError::terminal(
                    "CONNECTION_REQUEST_INVALID",
                    "unknown",
                    "Create a new connection operation from the Connections page.",
                )
            })?;
        let profile = self
            .store
            .configuration()
            .connection_execution_binding(&connection_id)
            .map_err(|_| {
                ConnectionOperationError::terminal(
                    "CONNECTION_PROFILE_NOT_FOUND",
                    "unknown",
                    "Refresh Connections and retry with an active profile.",
                )
            })?;
        let credentials = self.resolve_credentials(&profile)?;
        self.inspector.inspect(
            &profile,
            credentials.as_ref(),
            context.operation_kind() == "connection_discovery",
        )
    }

    fn execute_database_object_operation(
        &self,
        context: &restoreplane_catalog::OperationClaimContext,
    ) -> Result<DatabaseObjectInspection, ConnectionOperationError> {
        let request: DatabaseObjectOperationRequest =
            serde_json::from_str(context.request_json()).map_err(|_| invalid_scope_request())?;
        if request.schema_version != 2
            || request.kind != "backup_scope_object_discovery"
            || request.database_target_id != context.subject_id()
        {
            return Err(invalid_scope_request());
        }
        let target_id = DatabaseTargetId::from_str(&request.database_target_id)
            .map_err(|_| invalid_scope_request())?;
        let target = self
            .store
            .configuration()
            .selected_database_target(&target_id)
            .map_err(|_| {
                ConnectionOperationError::terminal(
                    "BACKUP_SCOPE_TARGET_NOT_FOUND",
                    "unknown",
                    "Refresh Databases and retry with an active database target.",
                )
            })?;
        let profile = self
            .store
            .configuration()
            .connection_execution_binding(target.connection_profile_id())
            .map_err(|_| {
                ConnectionOperationError::terminal(
                    "CONNECTION_PROFILE_NOT_FOUND",
                    "unknown",
                    "Refresh Connections and retry with an active profile.",
                )
            })?;
        let authority = if let Some(selection) = &request.workbench_selection {
            use crate::application::workbench_protection::SelectionAuthority;
            if request.inventory_coverage.as_deref() != Some("selected")
                || !selection.is_well_formed()
                || serde_json::to_vec(selection)
                    .map_err(|_| invalid_scope_request())?
                    .len()
                    > restoreplane_domain::workbench_protection::MAX_SELECTION_BYTES
            {
                return Err(invalid_scope_request());
            }
            let authority: SelectionAuthority = request
                .workbench_authority
                .clone()
                .ok_or_else(invalid_scope_request)?;
            if authority.scope.database_target_id != target_id
                || authority.scope.connection_profile_id != *target.connection_profile_id()
            {
                return Err(invalid_scope_request());
            }
            let observed = self
                .store
                .workbench_binding()
                .snapshot(&authority.scope, &authority.owner_sid_hash)
                .map_err(|_| invalid_scope_request())?;
            if crate::application::ServiceApplication::workbench_descriptor_revision(
                observed.revision_input,
            )
            .map_err(|_| invalid_scope_request())?
                != selection.expected_descriptor_revision
            {
                return Err(invalid_scope_request());
            }
            Some((
                authority,
                observed
                    .descriptor
                    .expected_server_identity
                    .ok_or_else(invalid_scope_request)?,
            ))
        } else {
            if request.workbench_authority.is_some()
                || request
                    .inventory_coverage
                    .as_deref()
                    .is_some_and(|coverage| coverage != "full")
            {
                return Err(invalid_scope_request());
            }
            None
        };
        let credentials = self.resolve_credentials(&profile)?;
        let mut inspection = if authority.is_some() {
            DatabaseObjectInspection::new(Vec::new())
        } else {
            self.inspector
                .inspect_database_objects(&profile, credentials.as_ref(), &target)?
        };
        if let Some((authority, expected_server)) = authority {
            let selection = request
                .workbench_selection
                .as_ref()
                .ok_or_else(invalid_scope_request)?;
            let mut observed = self.inspector.inspect_selected_structure(
                &profile,
                credentials.as_ref(),
                &target,
                &selection.items,
            )?;
            observed.items.sort();
            if observed.server_identity != expected_server
                || observed.items != selection.canonical_items()
            {
                return Err(ConnectionOperationError::terminal(
                    "WORKBENCH_STRUCTURE_CHANGED",
                    "authorization",
                    "선택한 테이블의 실제 대상이나 구조가 변경되었습니다. 다시 선택하세요.",
                ));
            }
            let latest = self
                .store
                .workbench_binding()
                .snapshot(&authority.scope, &authority.owner_sid_hash)
                .map_err(|_| invalid_scope_request())?;
            if crate::application::ServiceApplication::workbench_descriptor_revision(
                latest.revision_input,
            )
            .map_err(|_| invalid_scope_request())?
                != selection.expected_descriptor_revision
            {
                return Err(invalid_scope_request());
            }
            let now = (self.clock)();
            let verified_at_ms = u64::try_from(now)
                .map_err(|_| invalid_scope_request())?
                .into();
            inspection.objects = observed
                .items
                .iter()
                .map(|item| {
                    DiscoveredDatabaseObject::eligible_table(
                        &item.engine_object_id,
                        &item.schema_name,
                        &item.object_name,
                    )
                })
                .collect();
            inspection.workbench_selection = Some(
                restoreplane_domain::workbench_protection::WorkbenchSelectionProof {
                    selection_digest: crate::application::workbench_protection::selection_digest(
                        &authority, selection,
                    )
                    .map_err(|_| invalid_scope_request())?,
                    descriptor_revision: selection.expected_descriptor_revision.clone(),
                    items: observed.items,
                    verified_at_ms,
                },
            );
        }
        Ok(inspection)
    }

    fn execute_destination_operation(
        &self,
        context: &restoreplane_catalog::OperationClaimContext,
    ) -> Result<DestinationInspection, ConnectionOperationError> {
        if context.subject_type() != "destination" {
            return Err(invalid_destination_request());
        }
        let request: DestinationOperationRequest = serde_json::from_str(context.request_json())
            .map_err(|_| invalid_destination_request())?;
        if request.schema_version != 1 || request.destination_id != context.subject_id() {
            return Err(invalid_destination_request());
        }
        let destination_id = DestinationId::from_str(&request.destination_id)
            .map_err(|_| invalid_destination_request())?;
        let binding = self
            .store
            .configuration()
            .destination_probe_binding(&destination_id)
            .map_err(|_| {
                ConnectionOperationError::terminal(
                    "DESTINATION_NOT_FOUND",
                    "unknown",
                    "Refresh Destinations and retry with an active Destination.",
                )
            })?;
        inspect_destination(&binding, self.secrets.as_deref())
    }

    fn resolve_credentials(
        &self,
        profile: &ConnectionExecutionBinding,
    ) -> Result<Option<DatabaseCredentials>, ConnectionOperationError> {
        let Some(secret_ref_id) = profile.database_auth_secret_ref_id() else {
            return Ok(None);
        };
        let secrets = self.secrets.as_ref().ok_or_else(|| {
            ConnectionOperationError::terminal(
                "CONNECTION_SECRET_UNAVAILABLE",
                "authentication",
                "Repair the service Secret Store and retry.",
            )
        })?;
        let secret = secrets
            .resolve(secret_ref_id, SecretPurpose::DatabaseAuth)
            .map_err(|_| {
                ConnectionOperationError::terminal(
                    "CONNECTION_SECRET_UNAVAILABLE",
                    "authentication",
                    "Re-enter the database credential and retry.",
                )
            })?;
        let stored: StoredDatabaseCredentials =
            serde_json::from_slice(secret.expose()).map_err(|_| {
                ConnectionOperationError::terminal(
                    "CONNECTION_SECRET_INVALID",
                    "authentication",
                    "Re-enter the database credential and retry.",
                )
            })?;
        Ok(Some(DatabaseCredentials {
            username: Zeroizing::new(stored.username),
            password: Zeroizing::new(stored.password),
        }))
    }

    fn execute_support_bundle(
        &self,
        context: &restoreplane_catalog::OperationClaimContext,
    ) -> Result<SupportBundleReport, ConnectionOperationError> {
        if context.subject_type() != "service_instance" || context.subject_id() != "local" {
            return Err(invalid_support_bundle_request());
        }
        let request: SupportBundleOperationRequest =
            serde_json::from_str(context.request_json())
                .map_err(|_| invalid_support_bundle_request())?;
        if request.schema_version != 1 {
            return Err(invalid_support_bundle_request());
        }
        let now_ms = self.now().map_err(|_| {
            ConnectionOperationError::terminal(
                "SUPPORT_BUNDLE_CLOCK_INVALID",
                "support",
                "Correct the host clock and retry.",
            )
        })?;
        let diagnostics = self.store.diagnostics().collect(now_ms).map_err(|error| {
            ConnectionOperationError::terminal(
                error.code(),
                "support",
                "Run service diagnostics, repair the Operational Store, and retry.",
            )
        })?;
        let mut diagnostics_value = serde_json::to_value(diagnostics).map_err(|_| {
            ConnectionOperationError::terminal(
                "SUPPORT_BUNDLE_ENCODING_FAILED",
                "support",
                "Install a compatible service build and retry.",
            )
        })?;
        let activity = diagnostics_value
            .as_object_mut()
            .and_then(|object| object.remove("activity"))
            .unwrap_or_else(|| json!({}));
        let engine_matrix = self
            .store
            .console_query()
            .engine_matrix(now_ms)
            .and_then(|matrix| {
                serde_json::to_value(matrix).map_err(|_| RepositoryError::DiagnosticsInputInvalid)
            })
            .map_err(|error| {
                ConnectionOperationError::terminal(
                    error.code(),
                    "support",
                    "Repair Engine Matrix evidence and retry.",
                )
            })?;
        let authority = self.store.cutover().authority().map_err(|error| {
            ConnectionOperationError::terminal(
                error.code(),
                "support",
                "Repair the cutover authority projection and retry.",
            )
        })?;
        let minimum_reader = self
            .store
            .cutover()
            .minimum_reader_version()
            .map_err(|error| {
                ConnectionOperationError::terminal(
                    error.code(),
                    "support",
                    "Repair the minimum reader projection and retry.",
                )
            })?;
        let executable_hash = current_executable_hash().map_err(|code| {
            ConnectionOperationError::terminal(
                code,
                "support",
                "Repair the service installation and retry.",
            )
        })?;
        let config_metadata = json!({
            "executionAuthority": match authority.execution_authority() {
                restoreplane_catalog::PersistedExecutionAuthority::LegacyDirect => "legacy_direct",
                restoreplane_catalog::PersistedExecutionAuthority::Service => "service",
            },
            "cutoverMode": match authority.cutover_mode() {
                restoreplane_catalog::PersistedCutoverMode::RehearsalOnly => "rehearsal_only",
                restoreplane_catalog::PersistedCutoverMode::Production => "production",
            },
            "cutoverMarkerPresent": authority.cutover_marker().is_some(),
            "minimumReaderVersion": minimum_reader
                .as_ref()
                .map(|minimum| minimum.minimum_reader_version()),
            "binaryVersion": env!("CARGO_PKG_VERSION"),
            "binaryHashBlake3": executable_hash,
        });
        let build_id = option_env!("RESTOREPLANE_BUILD_ID").unwrap_or(env!("CARGO_PKG_VERSION"));
        let revision = option_env!("RESTOREPLANE_SOURCE_REVISION").unwrap_or("unavailable");
        let input = SupportBundleInput::new(
            build_id,
            revision,
            diagnostics_value,
            engine_matrix,
            activity,
            config_metadata,
        )
        .map_err(|error| {
            ConnectionOperationError::terminal(
                error.code(),
                "support",
                "Reduce the selected diagnostic scope and retry.",
            )
        })?;
        SupportBundleBuilder::new(self.support_bundle_root.clone())
            .and_then(|builder| builder.create(&input, now_ms))
            .map_err(|error| {
                ConnectionOperationError::terminal(
                    error.code(),
                    "support",
                    "Repair the support bundle directory ACL or review the canary alert.",
                )
            })
    }

    fn persist_operation(
        &self,
        context: &restoreplane_catalog::OperationClaimContext,
        inspection: &OperationInspection,
        started_at_ms: i64,
        completed_at_ms: i64,
    ) -> Result<PersistedOperation, OperationWorkerError> {
        match inspection {
            OperationInspection::Connection(inspection) => self
                .persist_connection_result(context, inspection, started_at_ms, completed_at_ms)
                .map(PersistedOperation::succeeded),
            OperationInspection::DatabaseObjects(inspection) => self
                .persist_database_object_result(context, inspection, completed_at_ms)
                .map(PersistedOperation::succeeded),
            OperationInspection::Destination(inspection) => {
                self.persist_destination_result(context, inspection, started_at_ms, completed_at_ms)
            }
            OperationInspection::SupportBundle(report) => serde_json::to_string(&json!({
                "schemaVersion": 1,
                "path": report.path().to_string_lossy(),
                "checksumBlake3": report.checksum_blake3(),
                "entryCount": report.entry_count(),
                "redactionVersion": report.redaction_version(),
                "generatedAtMs": completed_at_ms,
            }))
            .map(PersistedOperation::succeeded)
            .map_err(|_| OperationWorkerError::ResultEncoding),
        }
    }

    fn persist_database_object_result(
        &self,
        context: &restoreplane_catalog::OperationClaimContext,
        inspection: &DatabaseObjectInspection,
        completed_at_ms: i64,
    ) -> Result<String, OperationWorkerError> {
        validate_database_object_inspection(inspection)?;
        let target_id = DatabaseTargetId::from_str(context.subject_id())
            .map_err(|_| OperationWorkerError::RequestInvalid)?;
        let mut inventory_hasher = blake3::Hasher::new();
        let mut records = Vec::with_capacity(inspection.objects.len());
        for object in &inspection.objects {
            let fingerprint = database_object_fingerprint(&target_id, object);
            for value in [
                object.engine_object_id.as_bytes(),
                object.schema_name.as_bytes(),
                object.object_name.as_bytes(),
                object.object_type.as_bytes(),
                fingerprint.as_bytes(),
            ] {
                inventory_hasher.update(&(value.len() as u64).to_le_bytes());
                inventory_hasher.update(value);
            }
            records.push(if object.eligible {
                DiscoveredDatabaseObjectSpec::eligible_table(
                    &object.engine_object_id,
                    &object.schema_name,
                    &object.object_name,
                    fingerprint,
                )
            } else {
                DiscoveredDatabaseObjectSpec::excluded(
                    &object.engine_object_id,
                    &object.schema_name,
                    &object.object_name,
                    &object.object_type,
                    object
                        .exclusion_reason
                        .as_deref()
                        .unwrap_or("engine policy"),
                    fingerprint,
                )
            });
        }
        let inventory_hash = inventory_hasher.finalize().to_hex().to_string();
        let eligible_count = inspection
            .objects
            .iter()
            .filter(|item| item.eligible)
            .count();
        let discovery = DatabaseObjectDiscoverySpec::new(
            context.operation_run_id().clone(),
            target_id,
            &inventory_hash,
            records,
            completed_at_ms,
        );
        let discovery = if inspection.workbench_selection.is_some() {
            discovery.with_selected_coverage()
        } else {
            discovery
        };
        self.store
            .backup_scopes()
            .record_object_discovery(discovery)?;
        let mut result = json!({
            "kind": "backup_scope_object_discovery",
            "inventoryHashBlake3": inventory_hash,
            "totalCount": inspection.objects.len(),
            "eligibleCount": eligible_count,
            "observedAtMs": completed_at_ms,
        });
        if let Some(proof) = &inspection.workbench_selection {
            result["inventoryCoverage"] = json!("selected");
            result["workbenchSelection"] =
                serde_json::to_value(proof).map_err(|_| OperationWorkerError::ResultEncoding)?;
        }
        serde_json::to_string(&result).map_err(|_| OperationWorkerError::ResultEncoding)
    }

    fn persist_connection_result(
        &self,
        context: &restoreplane_catalog::OperationClaimContext,
        inspection: &ConnectionInspection,
        started_at_ms: i64,
        completed_at_ms: i64,
    ) -> Result<String, OperationWorkerError> {
        validate_inspection(inspection)?;
        let connection_id = ConnectionProfileId::from_str(context.subject_id())
            .map_err(|_| OperationWorkerError::RequestInvalid)?;
        match context.operation_kind() {
            "connection_probe" => {
                self.store
                    .configuration()
                    .record_connection_probe(ConnectionProbeSpec::passed(
                        context.operation_run_id().as_str(),
                        connection_id,
                        &inspection.server_version,
                        inspection.capabilities.to_string(),
                        started_at_ms,
                        completed_at_ms,
                    ))?;
                serde_json::to_string(&json!({
                    "schemaVersion": 1,
                    "serverVersion": inspection.server_version,
                    "capabilities": inspection.capabilities,
                }))
                .map_err(|_| OperationWorkerError::ResultEncoding)
            }
            "connection_discovery" => {
                let observations = inspection
                    .databases
                    .iter()
                    .map(|database| {
                        let fingerprint = discovery_fingerprint(
                            &connection_id,
                            &database.engine_object_id,
                            &database.normalized_name,
                        );
                        if database.eligible {
                            DiscoveredDatabaseSpec::eligible(
                                &database.engine_object_id,
                                &database.database_name,
                                &database.normalized_name,
                                fingerprint,
                            )
                        } else {
                            DiscoveredDatabaseSpec::excluded(
                                &database.engine_object_id,
                                &database.database_name,
                                &database.normalized_name,
                                database
                                    .exclusion_reason
                                    .as_deref()
                                    .unwrap_or("engine policy"),
                                fingerprint,
                            )
                        }
                    })
                    .collect();
                let report = self.store.configuration().record_discovery_snapshot(
                    DiscoverySnapshotSpec::successful(
                        context.operation_run_id().as_str(),
                        connection_id,
                        observations,
                        started_at_ms,
                        completed_at_ms,
                    ),
                )?;
                serde_json::to_string(&json!({
                    "schemaVersion": 1,
                    "serverVersion": inspection.server_version,
                    "capabilities": inspection.capabilities,
                    "discoveredCount": inspection.databases.len(),
                    "addedCount": report.added_count(),
                    "missingCount": report.missing_count(),
                    "changedCount": report.changed_count(),
                }))
                .map_err(|_| OperationWorkerError::ResultEncoding)
            }
            _ => Err(OperationWorkerError::RequestInvalid),
        }
    }

    fn persist_destination_result(
        &self,
        context: &restoreplane_catalog::OperationClaimContext,
        inspection: &DestinationInspection,
        started_at_ms: i64,
        completed_at_ms: i64,
    ) -> Result<PersistedOperation, OperationWorkerError> {
        let destination_id = DestinationId::from_str(context.subject_id())
            .map_err(|_| OperationWorkerError::RequestInvalid)?;
        validate_destination_inspection(inspection)?;
        let probes = inspection
            .roles
            .iter()
            .map(|role| {
                let id = destination_probe_id(context, role.role);
                if role.passed {
                    DestinationProbeSpec::passed(
                        id,
                        destination_id.clone(),
                        role.role,
                        role.capabilities.to_string(),
                        role.free_bytes,
                        role.latency_ms,
                        started_at_ms,
                        completed_at_ms,
                    )
                } else {
                    DestinationProbeSpec::failed(
                        id,
                        destination_id.clone(),
                        role.role,
                        role.capabilities.to_string(),
                        role.failure_class.unwrap_or("unknown"),
                        role.operator_action
                            .unwrap_or("Review the Destination endpoint and role credential."),
                        role.latency_ms,
                        started_at_ms,
                        completed_at_ms,
                    )
                }
            })
            .collect();
        self.store
            .configuration()
            .record_destination_probes(probes)?;
        let roles: Vec<Value> = inspection
            .roles
            .iter()
            .map(|role| {
                json!({
                    "role": role.role.as_str(),
                    "status": if role.passed { "passed" } else { "failed" },
                    "capabilities": role.capabilities,
                    "freeBytes": role.free_bytes,
                    "latencyMs": role.latency_ms,
                    "failureClass": role.failure_class,
                    "operatorAction": role.operator_action,
                })
            })
            .collect();
        let result_json = serde_json::to_string(&json!({
            "schemaVersion": 1,
            "aggregateStatus": if inspection.aggregate_passed {
                "passed"
            } else {
                "failed"
            },
            "roles": roles,
        }))
        .map_err(|_| OperationWorkerError::ResultEncoding)?;
        Ok(if inspection.aggregate_passed {
            PersistedOperation::succeeded(result_json)
        } else {
            PersistedOperation::failed(result_json, "DESTINATION_PROBE_FAILED")
        })
    }

    fn persist_destination_failure(
        &self,
        context: &restoreplane_catalog::OperationClaimContext,
        error: &ConnectionOperationError,
        started_at_ms: i64,
        completed_at_ms: i64,
    ) -> Result<(), OperationWorkerError> {
        let Ok(destination_id) = DestinationId::from_str(context.subject_id()) else {
            return Ok(());
        };
        let Ok(binding) = self
            .store
            .configuration()
            .destination_probe_binding(&destination_id)
        else {
            return Ok(());
        };
        let mut roles = vec![
            DestinationSecretRoleV2::BackupWriter,
            DestinationSecretRoleV2::RestoreReader,
        ];
        if binding.kind() == "local" || binding.delete_secret_ref_id().is_some() {
            roles.push(DestinationSecretRoleV2::DeleteOperator);
        }
        let probes = roles
            .into_iter()
            .map(|role| {
                DestinationProbeSpec::failed(
                    destination_probe_id(context, role),
                    destination_id.clone(),
                    role,
                    r#"{"probe":"failed","immutability":"none"}"#,
                    error.failure_class,
                    &error.operator_action,
                    0,
                    started_at_ms,
                    completed_at_ms,
                )
            })
            .collect();
        self.store
            .configuration()
            .record_destination_probes(probes)?;
        Ok(())
    }

    fn append_event(
        &self,
        occurred_at_ms: i64,
        event_code: &str,
        work_item_id: &WorkItemId,
        message: &str,
        error: bool,
    ) -> Result<(), OperationWorkerError> {
        let event = if error {
            ActivityEventSpec::error(
                uuid::Uuid::new_v4().to_string(),
                occurred_at_ms,
                event_code,
                "work_item",
                work_item_id.as_str(),
                message,
            )
        } else {
            ActivityEventSpec::info(
                uuid::Uuid::new_v4().to_string(),
                occurred_at_ms,
                event_code,
                "work_item",
                work_item_id.as_str(),
                message,
            )
        };
        self.store.activity().append_batch(vec![event])?;
        Ok(())
    }

    fn now(&self) -> Result<i64, OperationWorkerError> {
        let now_ms = (self.clock)();
        if now_ms < 0 {
            Err(OperationWorkerError::ClockInvalid)
        } else {
            Ok(now_ms)
        }
    }
}

#[derive(Debug, Error)]
pub enum OperationWorkerError {
    #[error("CONNECTION_WORKER_REPOSITORY")]
    Repository(#[from] RepositoryError),
    #[error("CONNECTION_WORKER_REQUEST_INVALID")]
    RequestInvalid,
    #[error("CONNECTION_WORKER_RESULT_INVALID")]
    ResultInvalid,
    #[error("CONNECTION_WORKER_RESULT_ENCODING")]
    ResultEncoding,
    #[error("CONNECTION_WORKER_CLOCK_INVALID")]
    ClockInvalid,
}

impl OperationWorkerError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Repository(_) => "CONNECTION_WORKER_REPOSITORY",
            Self::RequestInvalid => "CONNECTION_WORKER_REQUEST_INVALID",
            Self::ResultInvalid => "CONNECTION_WORKER_RESULT_INVALID",
            Self::ResultEncoding => "CONNECTION_WORKER_RESULT_ENCODING",
            Self::ClockInvalid => "CONNECTION_WORKER_CLOCK_INVALID",
        }
    }
}

#[derive(Debug, Error)]
pub enum OperationWorkerStartError {
    #[error("CONNECTION_WORKER_SPAWN_FAILED")]
    Spawn(#[source] std::io::Error),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConnectionOperationRequest {
    schema_version: u32,
    connection_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DatabaseObjectOperationRequest {
    schema_version: u32,
    kind: String,
    database_target_id: String,
    #[serde(default)]
    inventory_coverage: Option<String>,
    #[serde(default)]
    workbench_selection: Option<restoreplane_domain::workbench_protection::WorkbenchSelection>,
    #[serde(default)]
    workbench_authority: Option<crate::application::workbench_protection::SelectionAuthority>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DestinationOperationRequest {
    schema_version: u32,
    destination_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SupportBundleOperationRequest {
    schema_version: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredDatabaseCredentials {
    username: String,
    password: String,
}

fn validate_inspection(inspection: &ConnectionInspection) -> Result<(), OperationWorkerError> {
    if inspection.server_version.trim().is_empty()
        || inspection.server_version.len() > 512
        || !inspection.capabilities.is_object()
        || inspection.capabilities.to_string().len() > 64 * 1024
        || inspection.databases.len() > 100_000
        || inspection.databases.iter().any(|database| {
            database.engine_object_id.is_empty()
                || database.engine_object_id.len() > 1024
                || database.database_name.is_empty()
                || database.database_name.len() > 1024
                || database.normalized_name.is_empty()
                || database.normalized_name.len() > 1024
                || (!database.eligible && database.exclusion_reason.is_none())
        })
    {
        return Err(OperationWorkerError::ResultInvalid);
    }
    Ok(())
}

fn validate_database_object_inspection(
    inspection: &DatabaseObjectInspection,
) -> Result<(), OperationWorkerError> {
    if inspection.objects.len() > 100_000
        || inspection.objects.iter().any(|object| {
            object.engine_object_id.trim().is_empty()
                || object.engine_object_id.len() > 2_048
                || object.schema_name.len() > 256
                || object.object_name.trim().is_empty()
                || object.object_name.len() > 256
                || !matches!(
                    object.object_type.as_str(),
                    "table" | "partitioned_table" | "view" | "materialized_view" | "foreign_table"
                )
                || (object.eligible
                    && (object.object_type != "table" || object.exclusion_reason.is_some()))
                || (!object.eligible
                    && object
                        .exclusion_reason
                        .as_ref()
                        .is_none_or(|reason| reason.is_empty() || reason.len() > 256))
        })
        || inspection
            .objects
            .iter()
            .map(|object| object.engine_object_id.as_str())
            .collect::<BTreeSet<_>>()
            .len()
            != inspection.objects.len()
    {
        return Err(OperationWorkerError::ResultInvalid);
    }
    Ok(())
}

fn validate_destination_inspection(
    inspection: &DestinationInspection,
) -> Result<(), OperationWorkerError> {
    let writer = inspection
        .roles
        .iter()
        .filter(|role| role.role == DestinationSecretRoleV2::BackupWriter)
        .count();
    let reader = inspection
        .roles
        .iter()
        .filter(|role| role.role == DestinationSecretRoleV2::RestoreReader)
        .count();
    let deleter = inspection
        .roles
        .iter()
        .filter(|role| role.role == DestinationSecretRoleV2::DeleteOperator)
        .count();
    let required_passed = inspection
        .roles
        .iter()
        .filter(|role| {
            matches!(
                role.role,
                DestinationSecretRoleV2::BackupWriter | DestinationSecretRoleV2::RestoreReader
            )
        })
        .all(|role| role.passed);
    if !matches!(inspection.roles.len(), 2 | 3)
        || writer != 1
        || reader != 1
        || deleter > 1
        || inspection.aggregate_passed != required_passed
        || inspection.roles.iter().any(|role| {
            role.role == DestinationSecretRoleV2::HostAuth
                || !role.capabilities.is_object()
                || role.capabilities.to_string().len() > 64 * 1024
                || role.free_bytes.is_some_and(|value| value < 0)
                || role.latency_ms < 0
                || (role.passed && (role.failure_class.is_some() || role.operator_action.is_some()))
                || (!role.passed
                    && (role.failure_class.is_none() || role.operator_action.is_none()))
        })
    {
        return Err(OperationWorkerError::ResultInvalid);
    }
    Ok(())
}

fn destination_probe_id(
    context: &restoreplane_catalog::OperationClaimContext,
    role: DestinationSecretRoleV2,
) -> String {
    uuid::Uuid::new_v5(
        &context.operation_run_id().as_uuid(),
        format!("destination-probe\0{}", role.as_str()).as_bytes(),
    )
    .to_string()
}

fn invalid_destination_request() -> ConnectionOperationError {
    ConnectionOperationError::terminal(
        "DESTINATION_REQUEST_INVALID",
        "unknown",
        "Create a new probe from the Destinations page.",
    )
}

fn invalid_scope_request() -> ConnectionOperationError {
    ConnectionOperationError::terminal(
        "BACKUP_SCOPE_DISCOVERY_REQUEST_INVALID",
        "unknown",
        "Run table discovery again from the selected Database page.",
    )
}

fn invalid_support_bundle_request() -> ConnectionOperationError {
    ConnectionOperationError::terminal(
        "SUPPORT_BUNDLE_REQUEST_INVALID",
        "support",
        "Create a new support bundle from the local service console.",
    )
}

fn current_executable_hash() -> Result<String, &'static str> {
    let path = std::env::current_exe().map_err(|_| "SUPPORT_BUNDLE_BINARY_IDENTITY_UNAVAILABLE")?;
    let mut file =
        std::fs::File::open(path).map_err(|_| "SUPPORT_BUNDLE_BINARY_IDENTITY_UNAVAILABLE")?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|_| "SUPPORT_BUNDLE_BINARY_IDENTITY_UNAVAILABLE")?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn discovery_fingerprint(
    connection_id: &ConnectionProfileId,
    engine_object_id: &str,
    normalized_name: &str,
) -> String {
    let mut hasher = blake3::Hasher::new();
    for value in [
        connection_id.as_str().as_bytes(),
        engine_object_id.as_bytes(),
        normalized_name.as_bytes(),
    ] {
        hasher.update(&(value.len() as u64).to_le_bytes());
        hasher.update(value);
    }
    hasher.finalize().to_hex().to_string()
}

fn database_object_fingerprint(
    target_id: &DatabaseTargetId,
    object: &DiscoveredDatabaseObject,
) -> String {
    let mut hasher = blake3::Hasher::new();
    for value in [
        target_id.as_str().as_bytes(),
        object.engine_object_id.as_bytes(),
        object.schema_name.as_bytes(),
        object.object_name.as_bytes(),
        object.object_type.as_bytes(),
    ] {
        hasher.update(&(value.len() as u64).to_le_bytes());
        hasher.update(value);
    }
    hasher.finalize().to_hex().to_string()
}

fn unix_time_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(-1)
}

#[cfg(test)]
mod handle_tests {
    use super::*;

    #[test]
    fn operation_handle_claim_runtime_stop_joins_thread() {
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
        let handle = ServiceOperationWorkerHandle {
            stop,
            thread: Mutex::new(Some(thread)),
        };

        let runtime: &dyn crate::host::ClaimRuntime = &handle;
        runtime.request_stop();
        runtime.wait_stopped().expect("operation worker must join");

        assert!(stopped.load(Ordering::Acquire));
    }
}
