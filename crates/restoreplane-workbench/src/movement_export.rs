//! Native-only export files. The destination originates in a scoped native
//! dialog capability; no frontend path or display-cell text is accepted here.
use crate::browse::BrowseService;
use crate::data_movement::{FileCapabilities, NativeDestinationSelection};
use crate::driver::{FileReadLimits, RowSink};
use crate::journal::{payload_digest, AdmissionSpec, OperationJournal, Transition};
use crate::metadata::MetadataCatalog;
use crate::movement_codec::{CodecStats, ExportFormat, ExportWriter, FormulaPolicy};
use crate::query::{ApprovedExportRead, QueryCoordinator};
use crate::result_stream::{MemoryReservation, NativeBudget, ResultStreams, RetainedPage};
use crate::{InvocationContext, WorkbenchError, WorkbenchState};
use restoreplane_domain::workbench::{
    Admission, AuthorizePurpose, Column, Engine, OperationKind, Scope, SessionRef, SessionState,
    TypedCell,
};
use restoreplane_domain::workbench_movement::{
    ExportSource, ExportStartInput, FileArtifact, FileArtifactState, MovementFormat,
    MovementMetadata, MovementProgress, PartialPolicy, PersistedExportSource, ShownRange,
};
use restoreplane_domain::workbench_stream::{Completion, StreamIdentity};
use std::collections::{HashMap, HashSet};
use std::fs::File;
#[cfg(windows)]
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

pub const EXPORT_STAGING_BYTES: usize = 512 * 1024;
const EXPORT_JOB_BYTES: usize = 512 * 1024;
const MAX_EXPORT_JOBS: usize = 4;

#[derive(Debug)]
pub struct ExportStartError {
    pub error: WorkbenchError,
    pub operation_id: Option<Uuid>,
}
impl From<WorkbenchError> for ExportStartError {
    fn from(error: WorkbenchError) -> Self {
        Self {
            error,
            operation_id: None,
        }
    }
}
struct ExportJob {
    context: InvocationContext,
    scope: Scope,
    session: SessionRef,
    approval: Option<Arc<ApprovedExportRead>>,
    snapshot: Option<ExportSnapshot>,
    cancel: AtomicBool,
    running: AtomicBool,
    observation: Mutex<Option<FileArtifact>>,
    _memory: MemoryReservation,
    _permit: OwnedSemaphorePermit,
}
/// Passing a snapshot to an admitted fresh export transfers its cleanup to
/// that job. Other browse/read callers continue to borrow their snapshots.
struct ExportSnapshot {
    id: Uuid,
    owners: Arc<Mutex<HashSet<Uuid>>>,
}
impl Drop for ExportSnapshot {
    fn drop(&mut self) {
        if let Ok(mut owners) = self.owners.lock() {
            owners.remove(&self.id);
        }
    }
}
struct ShownExport {
    input: ExportStartInput,
    identity: StreamIdentity,
    range: ShownRange,
    initial: RetainedPage,
    plan: ExportFilePlan,
    metadata: MovementMetadata,
    version: u64,
}
struct FreshExport {
    input: ExportStartInput,
    approval: Arc<ApprovedExportRead>,
    plan: ExportFilePlan,
    metadata: MovementMetadata,
    version: u64,
}
struct FreshWriter {
    file: Option<ExportFile>,
    metadata: MovementMetadata,
    version: u64,
    columns_seen: bool,
}
struct FreshSink {
    owner: Arc<ExportCoordinator>,
    job: Arc<ExportJob>,
    id: Uuid,
    engine: Engine,
    formula: FormulaPolicy,
    state: Arc<Mutex<FreshWriter>>,
}
#[async_trait::async_trait]
impl RowSink for FreshSink {
    async fn columns(&mut self, columns: Vec<Column>) -> Result<(), WorkbenchError> {
        if columns.is_empty() || columns.len() > 512 {
            return Err(WorkbenchError::Limit);
        }
        let bytes = crate::types::columns_memory(&columns, columns.capacity());
        if bytes > 256 * 1024 {
            return Err(WorkbenchError::Limit);
        }
        let memory = self.owner.streams.budget().reserve(bytes)?;
        let state = self.state.clone();
        let formula = self.formula;
        tokio::task::spawn_blocking(move || {
            let _memory = memory;
            let mut state = state.lock().map_err(|_| WorkbenchError::Io)?;
            if state.columns_seen {
                return Err(WorkbenchError::InvalidInput);
            }
            state
                .file
                .as_mut()
                .ok_or(WorkbenchError::Io)?
                .begin(&columns, formula)?;
            state.columns_seen = true;
            Ok(())
        })
        .await
        .map_err(|_| WorkbenchError::Io)?
    }
    async fn row(&mut self, cells: Vec<TypedCell>) -> Result<(), WorkbenchError> {
        if self.job.cancel.load(Ordering::SeqCst) {
            return Err(WorkbenchError::CancelUnsettled);
        }
        validate_export_cells(self.engine, &cells)?;
        let bytes = crate::types::cells_memory(&cells, cells.capacity());
        if bytes > 256 * 1024 {
            return Err(WorkbenchError::Limit);
        }
        let memory = self.owner.streams.budget().reserve(bytes)?;
        let state = self.state.clone();
        let owner = self.owner.clone();
        let job = self.job.clone();
        let id = self.id;
        tokio::task::spawn_blocking(move || {
            let _memory = memory;
            if job.cancel.load(Ordering::SeqCst) {
                return Err(WorkbenchError::CancelUnsettled);
            }
            let mut state = state.lock().map_err(|_| WorkbenchError::Io)?;
            if !state.columns_seen {
                return Err(WorkbenchError::InvalidInput);
            }
            let file = state.file.as_mut().ok_or(WorkbenchError::Io)?;
            file.write_row(&cells)?;
            if file.stats().rows % 128 == 0 {
                let artifact = file.artifact(Completion::Partial)?;
                let identity = file.identity().to_owned();
                metadata_update(&mut state.metadata, artifact, Some(identity));
                let FreshWriter {
                    metadata, version, ..
                } = &mut *state;
                owner.persist(id, &job, metadata, version, None)?;
            }
            Ok(())
        })
        .await
        .map_err(|_| WorkbenchError::Io)?
    }
}
fn cancel_approved(job: &ExportJob) {
    if let Some(approval) = &job.approval {
        let approval = approval.clone();
        let context = job.context.clone();
        tokio::spawn(async move {
            let _ = approval.cancel(&context).await;
        });
    }
}
/// Native composition only. Shown reads never call a driver or SQL coordinator.
/// Fresh reads require a native-only single-use approval and a bounded sink.
pub struct ExportCoordinator {
    runtime: Arc<WorkbenchState>,
    files: Arc<FileCapabilities>,
    journal: Arc<OperationJournal>,
    streams: Arc<ResultStreams>,
    slots: Arc<Semaphore>,
    jobs: Mutex<HashMap<Uuid, Arc<ExportJob>>>,
    snapshot_owners: Arc<Mutex<HashSet<Uuid>>>,
    faults: Arc<ExportFaultHook>,
    fresh_reads: Option<(
        Arc<QueryCoordinator>,
        Arc<MetadataCatalog>,
        Arc<BrowseService>,
    )>,
}
impl ExportCoordinator {
    pub fn new(
        runtime: Arc<WorkbenchState>,
        files: Arc<FileCapabilities>,
        journal: Arc<OperationJournal>,
        streams: Arc<ResultStreams>,
    ) -> Self {
        Self {
            runtime,
            files,
            journal,
            streams,
            slots: Arc::new(Semaphore::new(MAX_EXPORT_JOBS)),
            jobs: Mutex::new(HashMap::new()),
            snapshot_owners: Arc::new(Mutex::new(HashSet::new())),
            faults: Arc::new(|_| Ok(())),
            fresh_reads: None,
        }
    }
    pub fn with_fresh_reads(
        mut self,
        queries: Arc<QueryCoordinator>,
        metadata: Arc<MetadataCatalog>,
        browse: Arc<BrowseService>,
    ) -> Self {
        self.fresh_reads = Some((queries, metadata, browse));
        self
    }
    pub fn with_fault_hook(mut self, hook: Arc<ExportFaultHook>) -> Self {
        self.faults = hook;
        self
    }
    pub async fn start(
        self: &Arc<Self>,
        context: InvocationContext,
        input: ExportStartInput,
    ) -> Result<Admission, ExportStartError> {
        context.principal().ensure_standard()?;
        let permit = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| WorkbenchError::Limit)?;
        if self.jobs.lock().map_err(|_| WorkbenchError::Io)?.len() >= MAX_EXPORT_JOBS {
            return Err(WorkbenchError::Limit.into());
        }
        let (initial, approval, persisted_source, source_completeness) = match &input.source {
            ExportSource::Shown { identity, range } => {
                if range.row_count == 0
                    || range.row_count > 200
                    || identity.session_id != input.session.session_id
                    || identity.connection_generation != input.session.connection_generation
                {
                    return Err(WorkbenchError::InvalidInput.into());
                }
                range
                    .start_row
                    .value()
                    .checked_add(u64::from(range.row_count))
                    .ok_or(WorkbenchError::Limit)?;
                let initial = self
                    .streams
                    .page(&context, identity, range.start_row.value(), 1)?;
                if initial.page().page.items.len() != 1 {
                    return Err(WorkbenchError::ResultExpired.into());
                }
                let completeness = self
                    .streams
                    .source_completeness(&context, identity)?
                    .unwrap_or(Completion::Partial);
                (
                    Some(initial),
                    None,
                    PersistedExportSource::Shown {
                        identity: identity.clone(),
                        range: range.clone(),
                    },
                    completeness,
                )
            }
            ExportSource::Fresh {
                review_id,
                snapshot_id,
            } => {
                let (queries, metadata, browse) = self
                    .fresh_reads
                    .as_ref()
                    .ok_or(WorkbenchError::Unsupported)?;
                let approval = queries
                    .approve_export_read(
                        &context,
                        input.session,
                        *review_id,
                        *snapshot_id,
                        metadata.clone(),
                        browse.clone(),
                    )
                    .await?;
                let source = PersistedExportSource::Fresh {
                    review_digest: approval.review_digest().into(),
                    snapshot_id: *snapshot_id,
                    read_started_at_ms: crate::types::now_ms().into(),
                };
                (None, Some(approval), source, Completion::Partial)
            }
        };
        // Approval above verified the native snapshot scope/session/lease.
        // Reserve ownership atomically before admission so two concurrent
        // export requests cannot both acquire cleanup of the same snapshot.
        let snapshot = match &input.source {
            ExportSource::Fresh {
                snapshot_id: Some(id),
                ..
            } => {
                if !self
                    .snapshot_owners
                    .lock()
                    .map_err(|_| WorkbenchError::Io)?
                    .insert(*id)
                {
                    return Err(WorkbenchError::TargetBusy.into());
                }
                Some(ExportSnapshot {
                    id: *id,
                    owners: self.snapshot_owners.clone(),
                })
            }
            _ => None,
        };
        let receipt = self
            .runtime
            .binding_receipt(&context, input.session, AuthorizePurpose::Read)
            .await?;
        let scope = receipt.scope.clone();
        if let ExportSource::Shown { identity, .. } = &input.source {
            if identity.workspace_id != scope.workspace_id.as_str()
                || identity.database_target_id != scope.database_target_id.as_str()
            {
                return Err(WorkbenchError::Forbidden.into());
            }
        }
        let destination =
            self.files
                .take_destination(&context, input.session, input.destination_handle)?;
        let memory = self.streams.budget().reserve(EXPORT_JOB_BYTES)?;
        let payload = serde_json::to_value(&input).map_err(|_| WorkbenchError::InvalidInput)?;
        let source_digest = payload_digest("export.start", &payload)?;
        let credential = self.runtime.credential_reference(&context, input.session)?;
        let owner = self.clone();
        let admitted_context = context.clone();
        let admitted_receipt = receipt.clone();
        let admitted_input = input.clone();
        let admission = tokio::task::spawn_blocking(move || {
            let prepared = owner.journal.prepare(
                &admitted_context,
                &admitted_receipt,
                Some(admitted_input.session),
                "export.start",
                &payload,
            )?;
            owner.journal.admit(
                &admitted_context,
                &prepared.attempt_token,
                "export.start",
                &payload,
                &admitted_receipt,
                Some(credential),
                AdmissionSpec {
                    kind: OperationKind::ExportJob,
                    session: Some(admitted_input.session),
                    transaction_id: None,
                    statements: Vec::new(),
                    batch: None,
                },
            )
        })
        .await
        .map_err(|_| WorkbenchError::Io)??;
        let id = admission.operation_id;
        if admission.deduplicated {
            return Ok(admission);
        }
        let plan = ExportFilePlan::new(destination, id, input.format).map_err(|error| {
            ExportStartError {
                error,
                operation_id: Some(id),
            }
        })?;
        let job = Arc::new(ExportJob {
            context: context.clone(),
            scope: scope.clone(),
            session: input.session,
            approval: approval.clone(),
            snapshot,
            cancel: AtomicBool::new(false),
            running: AtomicBool::new(true),
            observation: Mutex::new(Some(plan.initial_artifact(source_completeness))),
            _memory: memory,
            _permit: permit,
        });
        self.jobs
            .lock()
            .map_err(|_| ExportStartError {
                error: WorkbenchError::Io,
                operation_id: Some(id),
            })?
            .insert(id, job.clone());
        let metadata = MovementMetadata::Export {
            signature_version: 1,
            format: input.format,
            formula_policy: input.formula_policy,
            partial_policy: input.partial_policy,
            final_path: plan.final_path().to_string_lossy().into_owned(),
            partial_path: Some(plan.partial_path().to_string_lossy().into_owned()),
            file_identity: None,
            source_digest,
            source: persisted_source,
            file: plan.initial_artifact(source_completeness),
            progress: MovementProgress {
                bytes_processed: 0.into(),
                total_bytes: None,
                rows_processed: 0.into(),
                confirmed_committed_rows: 0.into(),
                confirmed_committed_batches: 0.into(),
                unresolved_batches: 0.into(),
                next_batch_index: None,
            },
        };
        if let Err(error) = (self.faults)(ExportFaultPoint::AfterAdmission) {
            return Err(self.abort_start(id, &job, &metadata, error).await);
        }
        let owner = self.clone();
        let initial_context = context.clone();
        let initial_scope = scope.clone();
        let initial_meta = metadata.clone();
        let expected = admission.version.value();
        let initialized = tokio::task::spawn_blocking(move || {
            owner.journal.update_movement(
                &initial_context,
                &initial_scope,
                id,
                expected,
                initial_meta,
                None,
            )
        })
        .await
        .map_err(|_| WorkbenchError::Io)
        .and_then(|value| value);
        let initialized = match initialized {
            Ok(value) => value,
            Err(error) => {
                return Err(self.abort_start(id, &job, &metadata, error).await);
            }
        };
        let fresh = match self
            .runtime
            .binding_receipt(&context, input.session, AuthorizePurpose::Read)
            .await
        {
            Ok(value) => value,
            Err(error) => {
                return Err(self.abort_start(id, &job, &metadata, error).await);
            }
        };
        let epoch = match self.runtime.get(&context, input.session) {
            Ok(view) => view.execution_epoch.value(),
            Err(error) => return Err(self.abort_start(id, &job, &metadata, error).await),
        };
        let owner = self.clone();
        let dispatch_context = context.clone();
        let dispatch_scope = scope.clone();
        let dispatch_meta = metadata.clone();
        let expected = initialized.version.value();
        let running = tokio::task::spawn_blocking(move || {
            let dispatch = owner.journal.mark_movement_dispatch(
                &dispatch_context,
                &dispatch_scope,
                id,
                expected,
                epoch,
                &fresh,
            )?;
            owner.journal.update_movement(
                &dispatch_context,
                &dispatch_scope,
                id,
                dispatch.version(),
                dispatch_meta,
                Some(Transition::Running),
            )
        })
        .await
        .map_err(|_| WorkbenchError::Io)
        .and_then(|value| value);
        let running = match running {
            Ok(value) => value,
            Err(error) => {
                return Err(self.abort_start(id, &job, &metadata, error).await);
            }
        };
        let owner = self.clone();
        let version = running.version.value();
        match (&input.source, initial, approval) {
            (ExportSource::Shown { identity, range }, Some(initial), None) => {
                let identity = identity.clone();
                let range = range.clone();
                tokio::task::spawn_blocking(move || {
                    owner.write_shown(
                        id,
                        job,
                        ShownExport {
                            input,
                            identity,
                            range,
                            initial,
                            plan,
                            metadata,
                            version,
                        },
                    )
                });
            }
            (ExportSource::Fresh { .. }, None, Some(approval)) => {
                tokio::spawn(async move {
                    owner
                        .write_fresh(
                            id,
                            job,
                            FreshExport {
                                input,
                                approval,
                                plan,
                                metadata,
                                version,
                            },
                        )
                        .await;
                });
            }
            _ => {
                return Err(ExportStartError {
                    error: WorkbenchError::InvalidInput,
                    operation_id: Some(id),
                })
            }
        }
        Ok(Admission {
            operation_id: id,
            version: running.version,
            deduplicated: admission.deduplicated,
        })
    }
    pub fn cancel(&self, context: &InvocationContext, id: Uuid) -> Result<bool, WorkbenchError> {
        context.principal().ensure_standard()?;
        let jobs = self.jobs.lock().map_err(|_| WorkbenchError::Io)?;
        let Some(job) = jobs.get(&id) else {
            return Ok(false);
        };
        if job.context.principal().owner_id() != context.principal().owner_id()
            || job.context.window_instance_id() != context.window_instance_id()
        {
            return Err(WorkbenchError::Forbidden);
        }
        if !job.running.load(Ordering::SeqCst) {
            return Ok(false);
        }
        job.cancel.store(true, Ordering::SeqCst);
        cancel_approved(job);
        Ok(true)
    }
    pub fn observation(
        &self,
        context: &InvocationContext,
        scope: &Scope,
        id: Uuid,
    ) -> Result<Option<FileArtifact>, WorkbenchError> {
        context.principal().ensure_standard()?;
        let jobs = self.jobs.lock().map_err(|_| WorkbenchError::Io)?;
        let Some(job) = jobs.get(&id) else {
            return Ok(None);
        };
        if job.context.principal().owner_id() != context.principal().owner_id()
            || &job.scope != scope
        {
            return Err(WorkbenchError::Forbidden);
        }
        let result = job
            .observation
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .clone();
        Ok(result)
    }
    pub fn retire_window(&self, context: &InvocationContext) -> Result<usize, WorkbenchError> {
        context.principal().ensure_standard()?;
        let jobs = self.jobs.lock().map_err(|_| WorkbenchError::Io)?;
        let mut count = 0;
        for job in jobs.values() {
            if job.context.principal().owner_id() == context.principal().owner_id()
                && job.context.window_instance_id() == context.window_instance_id()
                && job.running.load(Ordering::SeqCst)
            {
                job.cancel.store(true, Ordering::SeqCst);
                cancel_approved(job);
                count += 1
            }
        }
        Ok(count)
    }
    pub fn retire_session(
        &self,
        context: &InvocationContext,
        session: SessionRef,
    ) -> Result<usize, WorkbenchError> {
        context.principal().ensure_standard()?;
        let jobs = self.jobs.lock().map_err(|_| WorkbenchError::Io)?;
        let mut count = 0;
        for job in jobs.values() {
            if job.context.principal().owner_id() == context.principal().owner_id()
                && job.context.window_instance_id() == context.window_instance_id()
                && job.session == session
                && job.running.load(Ordering::SeqCst)
            {
                job.cancel.store(true, Ordering::SeqCst);
                cancel_approved(job);
                count += 1
            }
        }
        Ok(count)
    }
    async fn abort_start(
        self: &Arc<Self>,
        id: Uuid,
        job: &Arc<ExportJob>,
        metadata: &MovementMetadata,
        error: WorkbenchError,
    ) -> ExportStartError {
        let cleanup = self.finish_snapshot(job).await;
        job.running.store(false, Ordering::SeqCst);
        let owner = self.clone();
        let failed_job = job.clone();
        let fallback = metadata_file(metadata).clone();
        let metadata = metadata.clone();
        let stopped = tokio::task::spawn_blocking(move || {
            let operation = owner
                .journal
                .get(&failed_job.context, &failed_job.scope, id)?;
            if operation.state == restoreplane_domain::workbench::OperationState::Accepted {
                owner.journal.transition(
                    &failed_job.context,
                    &failed_job.scope,
                    id,
                    operation.version.value(),
                    Transition::FailedBeforeSend,
                )?;
            } else {
                owner.journal.update_movement(
                    &failed_job.context,
                    &failed_job.scope,
                    id,
                    operation.version.value(),
                    metadata,
                    Some(Transition::ReadInterrupted),
                )?;
            }
            Ok::<_, WorkbenchError>(())
        })
        .await;
        if matches!(stopped, Ok(Ok(()))) {
            if let Ok(mut jobs) = self.jobs.lock() {
                jobs.remove(&id);
            }
        } else {
            self.uncertain(job, fallback);
        }
        ExportStartError {
            error: cleanup.err().unwrap_or(error),
            operation_id: Some(id),
        }
    }
    async fn finish_snapshot(&self, job: &ExportJob) -> Result<(), WorkbenchError> {
        let Some(snapshot) = &job.snapshot else {
            return Ok(());
        };
        let (_, _, browse) = self
            .fresh_reads
            .as_ref()
            .ok_or(WorkbenchError::Unsupported)?;
        let settled = browse
            .snapshot_end(&job.context, job.session, snapshot.id)
            .await;
        match settled {
            Ok(true) => Ok(()),
            Err(error) => {
                // snapshot_end already retired/dropped the native driver on
                // uncertain cleanup. Preserve its scoped reconnect basis.
                Err(error)
            }
            Ok(false) => {
                // Absence is not positive cleanup evidence. Retire even if
                // another cleanup raced this job or its physical rollback
                // could not be observed; the file must remain partial.
                let _ = self
                    .runtime
                    .retire_connection(&job.context, job.session)
                    .await;
                Err(WorkbenchError::CancelUnsettled)
            }
        }
    }
    fn uncertain(&self, job: &ExportJob, mut file: FileArtifact) {
        if file.state != FileArtifactState::Removed {
            file.state = FileArtifactState::Unconfirmed;
            file.finalized = false
        }
        if let Ok(mut observation) = job.observation.lock() {
            *observation = Some(file)
        }
    }
    fn persist(
        &self,
        id: Uuid,
        job: &ExportJob,
        metadata: &MovementMetadata,
        version: &mut u64,
        next: Option<Transition>,
    ) -> Result<(), WorkbenchError> {
        let operation = self.journal.update_movement(
            &job.context,
            &job.scope,
            id,
            *version,
            metadata.clone(),
            next,
        )?;
        *version = operation.version.value();
        *job.observation.lock().map_err(|_| WorkbenchError::Io)? =
            Some(metadata_file(metadata).clone());
        Ok(())
    }
    async fn write_fresh(self: Arc<Self>, id: Uuid, job: Arc<ExportJob>, task: FreshExport) {
        let FreshExport {
            input,
            approval,
            plan,
            metadata,
            version,
        } = task;
        let state = Arc::new(Mutex::new(FreshWriter {
            file: None,
            metadata,
            version,
            columns_seen: false,
        }));
        let owner = self.clone();
        let native_job = job.clone();
        let native_state = state.clone();
        let created = tokio::task::spawn_blocking(move || {
            if native_job.cancel.load(Ordering::SeqCst) {
                return Err(WorkbenchError::CancelUnsettled);
            }
            let file = plan
                .create(&owner.streams.budget())?
                .with_fault_hook(owner.faults.clone());
            let artifact = file.artifact(Completion::Partial)?;
            let identity = file.identity().to_owned();
            let mut state = native_state.lock().map_err(|_| WorkbenchError::Io)?;
            state.file = Some(file);
            metadata_update(&mut state.metadata, artifact, Some(identity));
            let FreshWriter {
                metadata, version, ..
            } = &mut *state;
            owner.persist(id, &native_job, metadata, version, None)?;
            (owner.faults)(ExportFaultPoint::BeforeRead)
        })
        .await
        .map_err(|_| WorkbenchError::Io)
        .and_then(|value| value);
        let formula = if input.formula_policy
            == restoreplane_domain::workbench_movement::FormulaPolicy::Escape
        {
            FormulaPolicy::Escape
        } else {
            FormulaPolicy::RawConfirmed
        };
        let result = match created {
            Ok(()) => {
                approval
                    .execute(
                        &job.context,
                        Box::new(FreshSink {
                            owner: self.clone(),
                            job: job.clone(),
                            id,
                            engine: approval.engine(),
                            formula,
                            state: state.clone(),
                        }),
                        FileReadLimits::default(),
                    )
                    .await
            }
            Err(error) => Err(error),
        };
        let cleanup = self.finish_snapshot(&job).await;
        // The driver completes and releases its own read control before this
        // stage can certify, seal, sync, or rename the file. An export-owned
        // snapshot also needs positive cleanup first. A cap is partial.
        let owner = self.clone();
        let completed = tokio::task::spawn_blocking(move || {
            let mut state = state.lock().map_err(|_| WorkbenchError::Io)?;
            let source = match &result {
                Ok(outcome) => outcome.completeness,
                Err(_) if job.cancel.load(Ordering::SeqCst) => Completion::Cancelled,
                Err(_) => Completion::Failed,
            };
            let expected_rows = result.as_ref().ok().map(|value| value.summary.rows_kept);
            let mut file = state.file.take();
            let outcome = (|| -> Result<(), WorkbenchError> {
                cleanup?;
                let file = file.as_mut().ok_or(WorkbenchError::Io)?;
                if !state.columns_seen
                    || source != Completion::Complete
                    || expected_rows != Some(file.stats().rows)
                    || job.cancel.load(Ordering::SeqCst)
                {
                    return Err(WorkbenchError::CancelUnsettled);
                }
                file.seal()?;
                if job.cancel.load(Ordering::SeqCst) {
                    return Err(WorkbenchError::CancelUnsettled);
                }
                file.finalize()?;
                metadata_update(
                    &mut state.metadata,
                    file.artifact(source)?,
                    Some(file.identity().into()),
                );
                let FreshWriter {
                    metadata, version, ..
                } = &mut *state;
                owner.persist(id, &job, metadata, version, Some(Transition::JobCompleted))
            })();
            if outcome.is_err() {
                if let Some(file) = file {
                    let identity = file.identity().to_owned();
                    let artifact = if job.cancel.load(Ordering::SeqCst)
                        && input.partial_policy == PartialPolicy::RemoveOnCancel
                    {
                        file.discard(source)
                    } else {
                        file.artifact(source)
                    };
                    if let Ok(mut artifact) = artifact {
                        if artifact.finalized {
                            artifact.state = FileArtifactState::Unconfirmed;
                            artifact.finalized = false;
                        }
                        metadata_update(&mut state.metadata, artifact, Some(identity));
                    }
                }
                let FreshWriter {
                    metadata, version, ..
                } = &mut *state;
                if owner
                    .persist(
                        id,
                        &job,
                        metadata,
                        version,
                        Some(Transition::ReadInterrupted),
                    )
                    .is_err()
                {
                    owner.uncertain(&job, metadata_file(metadata).clone());
                    job.running.store(false, Ordering::SeqCst);
                    return Ok(());
                }
            }
            job.running.store(false, Ordering::SeqCst);
            owner
                .jobs
                .lock()
                .map_err(|_| WorkbenchError::Io)?
                .remove(&id);
            Ok::<_, WorkbenchError>(())
        })
        .await;
        // A failed blocking task keeps the bounded job overlay and never
        // certifies a physical file whose durable completion is unconfirmed.
        if !matches!(completed, Ok(Ok(()))) {
            if let Ok(jobs) = self.jobs.lock() {
                if let Some(job) = jobs.get(&id) {
                    job.running.store(false, Ordering::SeqCst);
                }
            }
        }
    }
    fn write_shown(&self, id: Uuid, job: Arc<ExportJob>, task: ShownExport) {
        let ShownExport {
            input,
            identity,
            range,
            initial,
            plan,
            mut metadata,
            mut version,
        } = task;
        let metadata = &mut metadata;
        let mut output = None;
        let mut cancelled = false;
        let result = (|| -> Result<(), WorkbenchError> {
            let view = self.runtime.get(&job.context, input.session)?;
            if view.state != SessionState::Ready {
                return Err(WorkbenchError::SessionStale);
            }
            if job.cancel.load(Ordering::SeqCst) {
                cancelled = true;
                return Ok(());
            }
            self.streams.source_completeness(&job.context, &identity)?;
            let source_engine = self.streams.source_engine(&job.context, &identity)?;
            output = Some(
                plan.create(&self.streams.budget())?
                    .with_fault_hook(self.faults.clone()),
            );
            let file = output.as_mut().ok_or(WorkbenchError::Io)?;
            let original_completeness = metadata_file(metadata).source_completeness;
            metadata_update(
                metadata,
                file.artifact(original_completeness)?,
                Some(file.identity().into()),
            );
            self.persist(id, &job, metadata, &mut version, None)?;
            let formula = if input.formula_policy
                == restoreplane_domain::workbench_movement::FormulaPolicy::Escape
            {
                FormulaPolicy::Escape
            } else {
                FormulaPolicy::RawConfirmed
            };
            file.begin(&initial.page().columns, formula)?;
            for offset in 0..u64::from(range.row_count) {
                if job.cancel.load(Ordering::SeqCst) {
                    cancelled = true;
                    return Ok(());
                }
                let following;
                let page = if offset == 0 {
                    initial.page()
                } else {
                    following = self.streams.page(
                        &job.context,
                        &identity,
                        range.start_row.value() + offset,
                        1,
                    )?;
                    following.page()
                };
                if page.page.items.len() != 1
                    || page.schema_version != initial.page().schema_version
                    || page.columns != initial.page().columns
                {
                    return Err(WorkbenchError::ResultExpired);
                }
                validate_export_cells(source_engine, &page.page.items[0].cells)?;
                file.write_row(&page.page.items[0].cells)?;
                if offset % 16 == 15 {
                    let original_completeness = metadata_file(metadata).source_completeness;
                    metadata_update(
                        metadata,
                        file.artifact(original_completeness)?,
                        Some(file.identity().into()),
                    );
                    self.persist(id, &job, metadata, &mut version, None)?;
                }
            }
            let source_completeness = self
                .streams
                .source_completeness(&job.context, &identity)?
                .unwrap_or(Completion::Partial);
            if file.stats().rows != u64::from(range.row_count) {
                return Err(WorkbenchError::ResultExpired);
            }
            if job.cancel.load(Ordering::SeqCst) {
                cancelled = true;
                return Ok(());
            }
            file.seal()?;
            if job.cancel.load(Ordering::SeqCst) {
                cancelled = true;
                return Ok(());
            }
            file.finalize()?;
            metadata_update(
                metadata,
                file.artifact(source_completeness)?,
                Some(file.identity().into()),
            );
            self.persist(
                id,
                &job,
                metadata,
                &mut version,
                Some(Transition::JobCompleted),
            )?;
            Ok(())
        })();
        let success = result.is_ok() && !cancelled && metadata_file(metadata).finalized;
        if !success {
            let source_completeness = metadata_file(metadata).source_completeness;
            if let Some(file) = output.take() {
                let identity = file.identity().to_owned();
                let artifact = if cancelled && input.partial_policy == PartialPolicy::RemoveOnCancel
                {
                    file.discard(source_completeness)
                } else {
                    file.artifact(source_completeness)
                };
                if let Ok(mut artifact) = artifact {
                    // A physical rename with a failed durable completion cannot
                    // be reclassified as a deletable partial or a completed job.
                    if artifact.finalized {
                        artifact.state = FileArtifactState::Unconfirmed;
                        artifact.finalized = false;
                    }
                    metadata_update(metadata, artifact, Some(identity));
                }
            }
            if self
                .persist(
                    id,
                    &job,
                    metadata,
                    &mut version,
                    Some(Transition::ReadInterrupted),
                )
                .is_err()
            {
                self.uncertain(&job, metadata_file(metadata).clone());
                job.running.store(false, Ordering::SeqCst);
                return;
            }
        }
        job.running.store(false, Ordering::SeqCst);
        if let Ok(mut jobs) = self.jobs.lock() {
            jobs.remove(&id);
        }
    }
}
/// Native typed values retain their temporal basis. MySQL/MariaDB TIMESTAMP
/// conversion without a proved session offset cannot certify the original
/// instant. PostgreSQL timestamp-without-time-zone is a distinct naive value.
pub fn validate_export_cells(engine: Engine, cells: &[TypedCell]) -> Result<(), WorkbenchError> {
    crate::types::validate_cells(cells)?;
    if matches!(engine,Engine::Mysql|Engine::Mariadb) && cells.iter().any(|cell|matches!(cell,TypedCell::Temporal{type_name,offset,..} if type_name.eq_ignore_ascii_case("timestamp") && offset.as_ref().is_none_or(|offset|offset.is_empty()))) {
        return Err(WorkbenchError::Unsupported);
    }
    Ok(())
}
fn metadata_file(metadata: &MovementMetadata) -> &FileArtifact {
    let MovementMetadata::Export { file, .. } = metadata else {
        unreachable!("export coordinator metadata")
    };
    file
}
fn metadata_update(metadata: &mut MovementMetadata, file: FileArtifact, identity: Option<String>) {
    let MovementMetadata::Export {
        file: current,
        file_identity,
        partial_path,
        progress,
        ..
    } = metadata
    else {
        unreachable!("export coordinator metadata")
    };
    *partial_path = file.partial_display_path.clone();
    if identity.is_some() {
        *file_identity = identity
    };
    progress.bytes_processed = file.bytes_written;
    progress.rows_processed = file.rows_written;
    *current = file;
}

/// Native test seam, never deserialized from an IPC payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportFaultPoint {
    AfterAdmission,
    BeforeRead,
    BeforeFlush,
    BeforeSync,
    BeforeRename,
    AfterRename,
}
pub type ExportFaultHook = dyn Fn(ExportFaultPoint) -> Result<(), WorkbenchError> + Send + Sync;

/// Build this before the initial durable metadata CAS. Calling `create` is a
/// separate step and must follow durable admission/metadata and dispatch CAS.
pub struct ExportFilePlan {
    destination: NativeDestinationSelection,
    partial_path: PathBuf,
    format: MovementFormat,
}
impl ExportFilePlan {
    pub fn new(
        destination: NativeDestinationSelection,
        operation_id: Uuid,
        format: MovementFormat,
    ) -> Result<Self, WorkbenchError> {
        if operation_id.is_nil() {
            return Err(WorkbenchError::InvalidInput);
        }
        let parent = destination
            .final_path()
            .parent()
            .ok_or(WorkbenchError::InvalidInput)?;
        // Fixed-length private name; a long selected basename cannot overflow
        // NTFS's single-component bound. No existing partial is ever resumed.
        let partial_path = parent.join(format!("restoreplane-{operation_id}.partial"));
        if partial_path == destination.final_path() {
            return Err(WorkbenchError::InvalidInput);
        }
        Ok(Self {
            destination,
            partial_path,
            format,
        })
    }
    pub fn final_path(&self) -> &Path {
        self.destination.final_path()
    }
    pub fn partial_path(&self) -> &Path {
        &self.partial_path
    }
    pub fn initial_artifact(&self, source_completeness: Completion) -> FileArtifact {
        FileArtifact {
            display_path: self.final_path().to_string_lossy().into_owned(),
            partial_display_path: Some(self.partial_path.to_string_lossy().into_owned()),
            format: self.format,
            state: FileArtifactState::NotStarted,
            bytes_written: 0.into(),
            rows_written: 0.into(),
            source_completeness,
            finalized: false,
        }
    }
    pub fn create(self, budget: &Arc<NativeBudget>) -> Result<ExportFile, WorkbenchError> {
        #[cfg(not(windows))]
        {
            let _ = budget;
            Err(WorkbenchError::Unsupported)
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
            let memory = budget.reserve(EXPORT_STAGING_BYTES)?;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                // GENERIC_READ | GENERIC_WRITE | DELETE. Deny other writers
                // and delete/rename while retaining our own rename authority.
                .access_mode(0xC001_0000)
                .share_mode(1)
                .custom_flags(0x0020_0000 | 0x0800_0000)
                .open(&self.partial_path)
                .map_err(file_error)?;
            let metadata = file.metadata().map_err(|_| WorkbenchError::Io)?;
            if !metadata.is_file() || metadata.file_attributes() & 0x400 != 0 {
                return Err(WorkbenchError::Forbidden);
            }
            let identity = restoreplane_source_sqlite::opened_file_object_id(&file)
                .map_err(|_| WorkbenchError::Io)?;
            Ok(ExportFile {
                plan: self,
                file,
                writer: None,
                identity,
                stats: CodecStats { rows: 0, bytes: 0 },
                begun: false,
                sealed: false,
                failed: false,
                renamed: false,
                finalized: false,
                faults: Arc::new(|_| Ok(())),
                _memory: memory,
            })
        }
    }
}

/// Holds both the opened partial file and all native ancestor guards until job
/// completion/drop. Errors retain the partial; Drop never finalizes or removes.
pub struct ExportFile {
    plan: ExportFilePlan,
    file: File,
    writer: Option<ExportWriter<File>>,
    identity: String,
    stats: CodecStats,
    begun: bool,
    sealed: bool,
    failed: bool,
    renamed: bool,
    finalized: bool,
    faults: Arc<ExportFaultHook>,
    _memory: MemoryReservation,
}
impl ExportFile {
    pub fn with_fault_hook(mut self, hook: Arc<ExportFaultHook>) -> Self {
        self.faults = hook;
        self
    }
    pub fn identity(&self) -> &str {
        &self.identity
    }
    pub fn partial_path(&self) -> &Path {
        self.plan.partial_path()
    }
    pub fn final_path(&self) -> &Path {
        self.plan.final_path()
    }
    pub fn stats(&self) -> CodecStats {
        self.writer.as_ref().map_or(self.stats, ExportWriter::stats)
    }
    /// Persist the created file's identity before calling this method. The
    /// duplicate handle shares the same file and cursor; the original stays
    /// open through rename so no close-then-path replacement race is possible.
    pub fn begin(
        &mut self,
        columns: &[Column],
        formula: FormulaPolicy,
    ) -> Result<(), WorkbenchError> {
        if self.begun || self.failed || self.renamed {
            return Err(WorkbenchError::InvalidInput);
        }
        self.begun = true;
        let duplicate = self.file.try_clone().map_err(|_| WorkbenchError::Io)?;
        match ExportWriter::new(duplicate, codec_format(self.plan.format), columns, formula) {
            Ok(writer) => {
                self.stats = writer.stats();
                self.writer = Some(writer);
                Ok(())
            }
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }
    pub fn write_row(&mut self, cells: &[TypedCell]) -> Result<(), WorkbenchError> {
        if self.failed || self.sealed || self.renamed {
            return Err(WorkbenchError::InvalidInput);
        }
        let writer = self.writer.as_mut().ok_or(WorkbenchError::InvalidInput)?;
        let result = writer.write_row(cells);
        self.stats = writer.stats();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    /// Codec completion is only structural. A source Complete terminal (or the
    /// exact finite shown range), journal CAS and rename are separate gates.
    pub fn seal(&mut self) -> Result<CodecStats, WorkbenchError> {
        if self.failed || self.sealed || self.renamed {
            return Err(WorkbenchError::InvalidInput);
        }
        let writer = self.writer.take().ok_or(WorkbenchError::InvalidInput)?;
        let result = (|| {
            let (mut duplicate, stats) = writer.finish_with_stats()?;
            self.stats = stats;
            (self.faults)(ExportFaultPoint::BeforeFlush)?;
            duplicate.flush().map_err(|_| WorkbenchError::Io)?;
            (self.faults)(ExportFaultPoint::BeforeSync)?;
            duplicate.sync_all().map_err(|_| WorkbenchError::Io)?;
            drop(duplicate);
            Ok(stats)
        })();
        match result {
            Ok(stats) => {
                self.sealed = true;
                Ok(stats)
            }
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }
    /// Native coordinator only: prove the exact selected range/source terminal
    /// before calling. The same opened handle is renamed without replacement.
    pub fn finalize(&mut self) -> Result<(), WorkbenchError> {
        if !self.sealed || self.failed || self.renamed {
            return Err(WorkbenchError::InvalidInput);
        }
        let result = (|| {
            let actual = restoreplane_source_sqlite::opened_file_object_id(&self.file)
                .map_err(|_| WorkbenchError::Io)?;
            if actual != self.identity {
                return Err(WorkbenchError::Forbidden);
            }
            (self.faults)(ExportFaultPoint::BeforeRename)?;
            rename_open_file(&self.file, self.final_path())?;
            self.renamed = true;
            (self.faults)(ExportFaultPoint::AfterRename)?;
            self.file.sync_all().map_err(|_| WorkbenchError::Io)?;
            Ok(())
        })();
        if result.is_ok() {
            self.finalized = true;
        } else {
            self.failed = true;
        }
        result
    }
    pub fn artifact(
        &self,
        source_completeness: Completion,
    ) -> Result<FileArtifact, WorkbenchError> {
        let bytes = self.file.metadata().map_err(|_| WorkbenchError::Io)?.len();
        Ok(FileArtifact {
            display_path: self.final_path().to_string_lossy().into_owned(),
            partial_display_path: (!self.renamed)
                .then(|| self.partial_path().to_string_lossy().into_owned()),
            format: self.plan.format,
            state: if self.finalized {
                FileArtifactState::Complete
            } else if self.renamed {
                FileArtifactState::Unconfirmed
            } else {
                FileArtifactState::Partial
            },
            bytes_written: bytes.into(),
            rows_written: self.stats().rows.into(),
            source_completeness,
            finalized: self.finalized,
        })
    }
    /// Cancellation's remove policy applies only to this opened partial. An
    /// already renamed/unconfirmed final file can never enter this path.
    pub fn discard(
        mut self,
        source_completeness: Completion,
    ) -> Result<FileArtifact, WorkbenchError> {
        if self.renamed {
            return Err(WorkbenchError::Forbidden);
        }
        self.writer.take();
        let mut artifact = self.artifact(source_completeness)?;
        let actual = restoreplane_source_sqlite::opened_file_object_id(&self.file)
            .map_err(|_| WorkbenchError::Io)?;
        if actual != self.identity {
            return Err(WorkbenchError::Forbidden);
        }
        delete_open_file(&self.file)?;
        drop(self);
        artifact.state = FileArtifactState::Removed;
        artifact.partial_display_path = None;
        artifact.finalized = false;
        Ok(artifact)
    }
}

fn codec_format(format: MovementFormat) -> ExportFormat {
    match format {
        MovementFormat::Csv => ExportFormat::Csv,
        MovementFormat::Tsv => ExportFormat::Tsv,
        MovementFormat::Json => ExportFormat::TypedJson,
    }
}
/// Native journal cleanup only. The caller authenticates the owner/operation,
/// refuses finalized/final paths, and supplies the recorded partial identity.
/// This helper has no IPC decoder and never accepts frontend path authority.
pub fn remove_recorded_partial(
    path: &Path,
    expected_file_identity: &str,
    budget: Arc<NativeBudget>,
) -> Result<bool, WorkbenchError> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(WorkbenchError::InvalidInput)?;
    let token = name
        .strip_prefix("restoreplane-")
        .and_then(|name| name.strip_suffix(".partial"))
        .ok_or(WorkbenchError::Forbidden)?;
    let id = Uuid::parse_str(token).map_err(|_| WorkbenchError::Forbidden)?;
    if id.is_nil()
        || id.to_string() != token
        || expected_file_identity.is_empty()
        || expected_file_identity.len() > 512
        || expected_file_identity.contains(['\0', '\r', '\n'])
    {
        return Err(WorkbenchError::Forbidden);
    }
    #[cfg(not(windows))]
    {
        let _ = budget;
        Err(WorkbenchError::Unsupported)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
        let _memory = budget.reserve(128 * 1024)?;
        let guard = match NativeDestinationSelection::from_recorded_partial(path) {
            Ok(guard) => guard,
            Err(WorkbenchError::ResultExpired) => return Ok(false),
            Err(error) => return Err(error),
        };
        let file = match OpenOptions::new()
            .read(true)
            .access_mode(0x8001_0000)
            .share_mode(1)
            .custom_flags(0x0020_0000)
            .open(guard.final_path())
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(_) => return Err(WorkbenchError::Io),
        };
        let metadata = file.metadata().map_err(|_| WorkbenchError::Io)?;
        if !metadata.is_file() || metadata.file_attributes() & 0x400 != 0 {
            return Err(WorkbenchError::Forbidden);
        }
        let actual = restoreplane_source_sqlite::opened_file_object_id(&file)
            .map_err(|_| WorkbenchError::Io)?;
        if actual != expected_file_identity {
            return Err(WorkbenchError::Forbidden);
        }
        delete_open_file(&file)?;
        drop(file);
        Ok(true)
    }
}
#[cfg(windows)]
fn file_error(error: std::io::Error) -> WorkbenchError {
    if error.kind() == std::io::ErrorKind::AlreadyExists {
        WorkbenchError::IdempotencyConflict
    } else {
        WorkbenchError::Io
    }
}
#[cfg(windows)]
fn rename_open_file(file: &File, destination: &Path) -> Result<(), WorkbenchError> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        FileRenameInfo, SetFileInformationByHandle, FILE_RENAME_INFO,
    };
    let mut name = destination.as_os_str().encode_wide().collect::<Vec<_>>();
    if name.is_empty() || name.len() > 32766 || name.contains(&0) {
        return Err(WorkbenchError::InvalidInput);
    }
    let name_bytes = name.len().checked_mul(2).ok_or(WorkbenchError::Limit)?;
    name.push(0);
    let offset = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
    let bytes = (offset + name.len() * 2).max(std::mem::size_of::<FILE_RENAME_INFO>());
    // usize storage gives the flexible C structure its required pointer alignment.
    let mut storage = vec![0usize; bytes.div_ceil(std::mem::size_of::<usize>())];
    let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // SAFETY: buffer is aligned and covers the struct plus the UTF-16 name.
    // ReplaceIfExists stays FALSE; destination's ancestor handles remain held.
    unsafe {
        info.write(FILE_RENAME_INFO::default());
        (*info).FileNameLength = name_bytes.try_into().map_err(|_| WorkbenchError::Limit)?;
        let name_pointer = storage.as_mut_ptr().cast::<u8>().add(offset).cast::<u16>();
        std::ptr::copy_nonoverlapping(name.as_ptr(), name_pointer, name.len());
        SetFileInformationByHandle(
            HANDLE(file.as_raw_handle()),
            FileRenameInfo,
            info.cast(),
            bytes.try_into().map_err(|_| WorkbenchError::Limit)?,
        )
        .map_err(|error| {
            if matches!(error.code().0 as u32 & 0xffff, 80 | 183) {
                WorkbenchError::IdempotencyConflict
            } else {
                WorkbenchError::Io
            }
        })
    }
}
#[cfg(not(windows))]
fn rename_open_file(_: &File, _: &Path) -> Result<(), WorkbenchError> {
    Err(WorkbenchError::Unsupported)
}
#[cfg(windows)]
fn delete_open_file(file: &File) -> Result<(), WorkbenchError> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::{BOOLEAN, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        FileDispositionInfo, SetFileInformationByHandle, FILE_DISPOSITION_INFO,
    };
    let info = FILE_DISPOSITION_INFO {
        DeleteFile: BOOLEAN(1),
    };
    // SAFETY: live owned DELETE-capable handle; fixed C layout and byte count.
    unsafe {
        SetFileInformationByHandle(
            HANDLE(file.as_raw_handle()),
            FileDispositionInfo,
            (&info as *const FILE_DISPOSITION_INFO).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
        .map_err(|_| WorkbenchError::Io)
    }
}
#[cfg(not(windows))]
fn delete_open_file(_: &File) -> Result<(), WorkbenchError> {
    Err(WorkbenchError::Unsupported)
}
