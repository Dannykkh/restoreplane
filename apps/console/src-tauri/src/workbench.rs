//! Native current-user Workbench composition; never a privileged SQL proxy.
use async_trait::async_trait;
use restoreplane_application::{ApplicationCommand, WorkbenchCommand, WorkbenchRequest};
use restoreplane_domain::workbench::{
    AuthorizePurpose, AuthorizeRequest, BindingReceipt, DecimalU64, Engine, Mode, OperationState,
    Scope, SessionRef, WBError,
};
use restoreplane_domain::workbench_edit as edit_dto;
use restoreplane_domain::workbench_metadata as metadata_dto;
use restoreplane_domain::workbench_movement as movement_dto;
use restoreplane_domain::workbench_query as query_dto;
use restoreplane_domain::workbench_stream::{Ack, ResultNotice, StreamIdentity};
use restoreplane_ipc_client::IpcClient;
use restoreplane_workbench::browse::{
    BrowseInput as NativeBrowseInput, BrowseService, SnapshotCertification,
};
use restoreplane_workbench::credentials::LocalCredentials;
use restoreplane_workbench::data_movement::{
    FileCapabilities, ImportCoordinator, ImportPreviewInput, NativeDestinationSelection,
    NativeSourceSelection,
};
use restoreplane_workbench::driver::NativeDriverFactory;
use restoreplane_workbench::edits::EditService;
use restoreplane_workbench::generated_sql::WriteCertification;
use restoreplane_workbench::journal::OperationJournal;
use restoreplane_workbench::metadata::MetadataCatalog;
use restoreplane_workbench::movement_export::ExportCoordinator;
use restoreplane_workbench::protection::ProtectionLinks;
use restoreplane_workbench::query::QueryCoordinator;
use restoreplane_workbench::result_stream::{FramePull, ResultNoticeEmitter, ResultStreams};
use restoreplane_workbench::saved_queries::SavedQueries;
use restoreplane_workbench::session::{RestoredSessionCheck, RuntimeLimits};
use restoreplane_workbench::{
    BindingAuthorizer, InvocationContext, WorkbenchError, WorkbenchState,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};
use tauri::ipc::{Channel, JavaScriptChannelId};
use uuid::Uuid;
use zeroize::Zeroizing;

type OutputWriter = Box<dyn FnOnce(&mut dyn std::io::Write) -> Result<(), WBError> + Send>;
struct NativeOutput {
    write: OutputWriter,
    finish: Option<Box<dyn FnOnce() -> Result<(), WBError> + Send>>,
    max_bytes: usize,
    keepalive: Option<Box<dyn Send + Sync>>,
}
impl NativeOutput {
    fn hold<T: Send + Sync + 'static>(mut self, guard: T) -> Self {
        self.keepalive = Some(Box::new((self.keepalive.take(), guard)));
        self
    }
    fn projected<T: Send + Sync + 'static, F>(value: T, write: F) -> Self
    where
        F: FnOnce(&T, &mut dyn std::io::Write) -> Result<(), WBError> + Send + 'static,
    {
        let value = Arc::new(value);
        let serializing = value.clone();
        Self {
            write: Box::new(move |writer| write(serializing.as_ref(), writer)),
            finish: None,
            max_bytes: 1024 * 1024,
            keepalive: Some(Box::new(value)),
        }
    }
    fn frames(pull: FramePull) -> Self {
        let pull = Arc::new(pull);
        let writing = pull.clone();
        let finishing = pull.clone();
        Self {
            write: Box::new(move |writer| {
                serde_json::to_writer(writer, writing.as_ref()).map_err(|_| WBError::new("WB_IO"))
            }),
            finish: Some(Box::new(move || finishing.mark_sent().map_err(wire_error))),
            max_bytes: 256 * 1024,
            keepalive: Some(Box::new(pull)),
        }
    }
    fn value(value: Value) -> Self {
        Self::typed(value)
    }
    fn typed<T: Serialize + Send + Sync + 'static>(value: T) -> Self {
        let value = Arc::new(value);
        let serialized = value.clone();
        Self {
            write: Box::new(move |writer| {
                serde_json::to_writer(writer, serialized.as_ref())
                    .map_err(|_| WBError::new("WB_IO"))
            }),
            finish: None,
            max_bytes: 1024 * 1024,
            keepalive: Some(Box::new(value)),
        }
    }
    fn reply(self, request_id: &str) -> Result<tauri::ipc::Response, WBError> {
        let _keepalive = self.keepalive;
        let mut writer = BoundedReply {
            bytes: Vec::new(),
            limit: self.max_bytes,
            overflow: false,
        };
        writer
            .write_all(b"{\"ok\":true,\"requestId\":")
            .map_err(|_| WBError::new("WB_LIMIT"))?;
        serde_json::to_writer(&mut writer, request_id).map_err(|_| WBError::new("WB_IO"))?;
        writer
            .write_all(b",\"data\":")
            .map_err(|_| WBError::new("WB_LIMIT"))?;
        let result = (self.write)(&mut writer);
        if writer.overflow {
            return Err(WBError::new("WB_LIMIT"));
        }
        result?;
        writer
            .write_all(b"}")
            .map_err(|_| WBError::new("WB_LIMIT"))?;
        if let Some(finish) = self.finish {
            finish()?;
        }
        let serialized = String::from_utf8(writer.bytes).map_err(|_| WBError::new("WB_IO"))?;
        Ok(tauri::ipc::Response::new(
            tauri::ipc::InvokeResponseBody::Json(serialized),
        ))
    }
}
struct BoundedReply {
    bytes: Vec<u8>,
    limit: usize,
    overflow: bool,
}
impl std::io::Write for BoundedReply {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self
            .bytes
            .len()
            .checked_add(bytes.len())
            .is_none_or(|len| len > self.limit)
        {
            self.overflow = true;
            return Err(std::io::Error::other("bounded reply"));
        }
        self.bytes
            .try_reserve(bytes.len())
            .map_err(|_| std::io::Error::other("reply allocation"))?;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct ServiceBindingAuthorizer {
    app_nonce: Uuid,
}
#[async_trait]
impl BindingAuthorizer for ServiceBindingAuthorizer {
    async fn authorize(
        &self,
        _: &InvocationContext,
        scope: &Scope,
        purpose: AuthorizePurpose,
        expected_revision: Option<&str>,
    ) -> Result<BindingReceipt, WorkbenchError> {
        let payload = AuthorizeRequest {
            workspace_id: scope.workspace_id.clone(),
            connection_profile_id: scope.connection_profile_id.clone(),
            database_target_id: scope.database_target_id.clone(),
            purpose,
            expected_descriptor_revision: expected_revision.map(str::to_owned),
            app_nonce: self.app_nonce,
        };
        let outcome = IpcClient::production()
            .execute(
                ApplicationCommand::WorkbenchBindingAuthorize,
                None,
                serde_json::to_value(payload).map_err(|_| WorkbenchError::InvalidInput)?,
            )
            .await
            .map_err(|_| WorkbenchError::ServiceUnavailable)?;
        if !outcome.is_success() {
            return Err(match outcome.code() {
                "WB_BINDING_STALE" => WorkbenchError::BindingStale,
                "WB_BINDING_UNPROVEN" => WorkbenchError::BindingUnproven,
                "WB_TARGET_BUSY" => WorkbenchError::TargetBusy,
                "APPLICATION_FORBIDDEN" => WorkbenchError::Forbidden,
                _ => WorkbenchError::ServiceUnavailable,
            });
        }
        serde_json::from_value(outcome.result().clone())
            .map_err(|_| WorkbenchError::BindingUnproven)
    }
}

struct Runtime {
    state: Arc<WorkbenchState>,
    credentials: Arc<LocalCredentials>,
    authorizer: Arc<ServiceBindingAuthorizer>,
    journal: Arc<OperationJournal>,
    streams: Arc<ResultStreams>,
    metadata: Arc<MetadataCatalog>,
    browse: Arc<BrowseService>,
    queries: Arc<QueryCoordinator>,
    saved_queries: Arc<SavedQueries>,
    protection: Arc<ProtectionLinks>,
    edits: Arc<EditService>,
    files: Arc<FileCapabilities>,
    imports: Arc<ImportCoordinator>,
    exports: Arc<ExportCoordinator>,
}
#[derive(Clone)]
enum JournalPosition {
    Operations {
        state: Option<OperationState>,
        after: (u64, Uuid),
    },
    Statements {
        operation_id: Uuid,
        after: u32,
    },
}
#[derive(Clone)]
struct JournalCursor {
    window: Uuid,
    scope: Scope,
    position: JournalPosition,
    expires: std::time::Instant,
}
#[derive(Clone)]
struct ListCursor {
    window: Uuid,
    scope: Scope,
    kind: &'static str,
    after: Uuid,
}
pub struct DesktopWorkbench {
    runtime: tokio::sync::OnceCell<Result<Arc<Runtime>, WBError>>,
    windows: Mutex<HashMap<String, Uuid>>,
    cursors: Mutex<HashMap<Uuid, ListCursor>>,
    journal_cursors: Mutex<HashMap<Uuid, JournalCursor>>,
    request_slots: Arc<tokio::sync::Semaphore>,
    control_slots: Arc<tokio::sync::Semaphore>,
    ack_slots: Arc<tokio::sync::Semaphore>,
    pull_slots: Arc<tokio::sync::Semaphore>,
    dialog_slots: Arc<tokio::sync::Semaphore>,
}
impl Default for DesktopWorkbench {
    fn default() -> Self {
        Self {
            runtime: tokio::sync::OnceCell::new(),
            windows: Mutex::new(HashMap::new()),
            cursors: Mutex::new(HashMap::new()),
            journal_cursors: Mutex::new(HashMap::new()),
            request_slots: Arc::new(tokio::sync::Semaphore::new(32)),
            control_slots: Arc::new(tokio::sync::Semaphore::new(8)),
            ack_slots: Arc::new(tokio::sync::Semaphore::new(8)),
            pull_slots: Arc::new(tokio::sync::Semaphore::new(8)),
            dialog_slots: Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }
}

impl DesktopWorkbench {
    fn request_slot(
        &self,
        command: WorkbenchCommand,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, WBError> {
        use WorkbenchCommand::*;
        let slots = match command {
            ResultAck => &self.ack_slots,
            ResultFrames => &self.pull_slots,
            QueryCancel
            | TransactionRollback
            | TransactionGet
            | SessionClose
            | SessionList
            | SnapshotEnd
            | OperationGet
            | OperationList
            | OperationStatements
            | OperationAcknowledgeUnknown
            | ImportCancel
            | ExportCancel
            | MovementStatus
            | MovementFileRelease
            | MovementPartialRemove
            | CredentialDelete => &self.control_slots,
            _ => &self.request_slots,
        };
        slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| WBError::new("WB_LIMIT").with_reason("queued_requests"))
    }
    async fn runtime(&self) -> Result<Arc<Runtime>, WBError> {
        self.runtime
            .get_or_init(|| async {
                let environment =
                    tauri::async_runtime::spawn_blocking(crate::workbench_native::detect)
                        .await
                        .map_err(|_| WBError::new("WB_IO"))?
                        .map_err(wire_error)?;
                let principal = environment.principal;
                let policy = Arc::new(environment.policy);
                let root = environment.root;
                let credentials = {
                    let principal = principal.clone();
                    let policy = policy.clone();
                    let root = root.join("credentials");
                    Arc::new(
                        tauri::async_runtime::spawn_blocking(move || {
                            LocalCredentials::open(&root, &principal, policy.as_ref())
                        })
                        .await
                        .map_err(|_| WBError::new("WB_IO"))?
                        .map_err(wire_error)?,
                    )
                };
                let authorizer = Arc::new(ServiceBindingAuthorizer {
                    app_nonce: Uuid::new_v4(),
                });
                let snapshot_certification = verified_snapshot_matrix().map_err(wire_error)?;
                let write_certification = verified_write_matrix().map_err(wire_error)?;
                let state = Arc::new(
                    WorkbenchState::new(
                        principal,
                        authorizer.clone(),
                        credentials.clone(),
                        Arc::new(NativeDriverFactory::default()),
                        RuntimeLimits::default(),
                    )
                    .map_err(wire_error)?
                    .with_snapshot_certification(snapshot_certification.clone())
                    .with_write_certification(write_certification.clone()),
                );
                let store = state
                    .initialize_store(root, policy)
                    .await
                    .map_err(wire_error)?;
                let journal = Arc::new({
                    let store = store.clone();
                    tauri::async_runtime::spawn_blocking(move || OperationJournal::open(store))
                        .await
                        .map_err(|_| WBError::new("WB_IO"))?
                        .map_err(wire_error)?
                });
                let streams = Arc::new(ResultStreams::new_pull(state.clone()).map_err(wire_error)?);
                let expiring = Arc::downgrade(&streams);
                tauri::async_runtime::spawn(async move {
                    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
                    loop {
                        interval.tick().await;
                        let Some(streams) = expiring.upgrade() else {
                            break;
                        };
                        let _ = streams.expire_unacknowledged().await;
                    }
                });
                let metadata = Arc::new(MetadataCatalog::new(state.clone(), streams.budget()));
                let browse = Arc::new(BrowseService::with_snapshot_certification(
                    metadata.clone(),
                    streams.clone(),
                    snapshot_certification,
                ));
                let queries = Arc::new(QueryCoordinator::new(
                    state.clone(),
                    journal.clone(),
                    streams.clone(),
                ));
                let saved_queries = Arc::new(SavedQueries::new(state.clone(), store));
                let protection = Arc::new(ProtectionLinks::new(state.clone(), metadata.clone()));
                // Lead-approved exact-version engine evidence; lifecycle/IPC
                // and each operation's fresh origin/type gates remain required.
                let write_certification = Some(write_certification);
                let edits = Arc::new(EditService::new(
                    state.clone(),
                    metadata.clone(),
                    browse.clone(),
                    journal.clone(),
                    write_certification.clone(),
                ));
                let files = Arc::new(FileCapabilities::new(state.clone(), streams.budget()));
                let imports = Arc::new(ImportCoordinator::new(
                    state.clone(),
                    metadata.clone(),
                    files.clone(),
                    journal.clone(),
                    write_certification,
                ));
                let exports = Arc::new(
                    ExportCoordinator::new(
                        state.clone(),
                        files.clone(),
                        journal.clone(),
                        streams.clone(),
                    )
                    .with_fresh_reads(
                        queries.clone(),
                        metadata.clone(),
                        browse.clone(),
                    ),
                );
                let expiring_edits = Arc::downgrade(&edits);
                let expiring_imports = Arc::downgrade(&imports);
                tauri::async_runtime::spawn(async move {
                    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
                    loop {
                        interval.tick().await;
                        let Some(edits) = expiring_edits.upgrade() else {
                            break;
                        };
                        let _ = edits.expire_memory();
                        if let Some(imports) = expiring_imports.upgrade() {
                            let _ = imports.expire_memory();
                        }
                    }
                });
                let expiring = Arc::downgrade(&queries);
                tauri::async_runtime::spawn(async move {
                    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
                    loop {
                        interval.tick().await;
                        let Some(queries) = expiring.upgrade() else {
                            break;
                        };
                        let _ = queries.expire_transactions().await;
                    }
                });
                Ok(Arc::new(Runtime {
                    state,
                    credentials,
                    authorizer,
                    journal,
                    streams,
                    metadata,
                    browse,
                    queries,
                    saved_queries,
                    protection,
                    edits,
                    files,
                    imports,
                    exports,
                }))
            })
            .await
            .clone()
    }
    fn context(&self, runtime: &Runtime, label: &str) -> Result<InvocationContext, WBError> {
        let mut windows = self.windows.lock().map_err(|_| WBError::new("WB_IO"))?;
        if windows.len() >= 32 && !windows.contains_key(label) {
            return Err(WBError::new("WB_LIMIT"));
        }
        Ok(InvocationContext::new(
            runtime.state.principal().clone(),
            *windows.entry(label.to_owned()).or_insert_with(Uuid::new_v4),
        ))
    }
    pub async fn window_closed(&self, label: &str) {
        let id = self
            .windows
            .lock()
            .ok()
            .and_then(|mut windows| windows.remove(label));
        if let Some(id) = id {
            if let Ok(mut cursors) = self.cursors.lock() {
                cursors.retain(|_, cursor| cursor.window != id);
            }
            if let Ok(mut cursors) = self.journal_cursors.lock() {
                cursors.retain(|_, cursor| cursor.window != id);
            }
        }
        if let (Some(id), Some(Ok(runtime))) = (id, self.runtime.get()) {
            let context = InvocationContext::new(runtime.state.principal().clone(), id);
            let _ = runtime.streams.retire_window(&context);
            let _ = runtime.queries.retire_window(&context).await;
            let _ = runtime.saved_queries.retire_window(&context);
            let _ = runtime.edits.retire_window(&context);
            let _ = runtime.imports.retire_window(&context);
            let _ = runtime.exports.retire_window(&context);
            let _ = runtime.files.retire_window(&context);
            let _ = runtime.browse.retire_window(&context).await;
            let _ = runtime.metadata.retire_window(&context);
            let _ = runtime.state.close_window(&context).await;
        }
    }
    async fn execute(
        &self,
        label: &str,
        native_owner: Option<isize>,
        request: WorkbenchRequest,
        channel: Option<Channel<tauri::ipc::InvokeResponseBody>>,
    ) -> Result<NativeOutput, WBError> {
        use WorkbenchCommand::*;
        let request_slot = self.request_slot(request.command)?;
        if matches!(
            request.command,
            EditPrepare
                | EditApply
                | EditConflictGet
                | EditDiscard
                | MovementSourcePick
                | MovementDestinationPick
                | MovementFileRelease
                | MovementStatus
                | MovementPartialRemove
                | ImportPreview
                | ImportContinueReview
                | ImportStart
                | ImportContinue
                | ImportCancel
                | ExportStart
                | ExportCancel
        ) {
            return self
                .execute_movement(label, native_owner, request)
                .await
                .map(|output| output.hold(request_slot));
        }
        if matches!(
            request.command,
            QueryReview
                | QueryRun
                | QueryResults
                | QueryCancel
                | TransactionBegin
                | TransactionGet
                | TransactionCommit
                | TransactionRollback
                | SavedQueryList
                | SavedQueryGet
                | SavedQueryPut
                | SavedQueryDelete
        ) {
            return self
                .execute_queries(label, request)
                .await
                .map(|output| output.hold(request_slot));
        }
        if matches!(
            request.command,
            CatalogPage
                | CatalogDescribe
                | CatalogGraph
                | BrowseOpen
                | BrowsePage
                | SnapshotBegin
                | SnapshotEnd
                | ResultAttach
                | ResultFrames
                | ResultAck
                | ResultPage
                | ProtectionSelectionGet
        ) {
            return self
                .execute_metadata(label, request, channel)
                .await
                .map(|output| output.hold(request_slot));
        }
        self.execute_value(label, request)
            .await
            .map(|value| NativeOutput::value(value).hold(request_slot))
    }
    async fn execute_movement(
        &self,
        label: &str,
        native_owner: Option<isize>,
        request: WorkbenchRequest,
    ) -> Result<NativeOutput, WBError> {
        use WorkbenchCommand::*;
        let runtime = self.runtime().await?;
        let context = self.context(&runtime, label)?;
        match request.command {
            EditPrepare => {
                let input: edit_dto::EditPrepareInput = decode(request.payload)?;
                Ok(NativeOutput::typed(
                    runtime
                        .edits
                        .prepare(&context, input)
                        .await
                        .map_err(wire_error)?,
                ))
            }
            EditApply => {
                let input: edit_dto::EditApplyInput = decode(request.payload)?;
                let token = request
                    .attempt_token
                    .ok_or_else(|| WBError::new("WB_PAYLOAD_INVALID"))?;
                Ok(NativeOutput::typed(
                    runtime
                        .edits
                        .apply(&context, token, input)
                        .await
                        .map_err(wire_error)?,
                ))
            }
            EditConflictGet => {
                let input: EditConflictInput = decode(request.payload)?;
                Ok(NativeOutput::typed(
                    runtime
                        .edits
                        .conflict_get(&context, input.session, input.conflict_id)
                        .map_err(wire_error)?,
                ))
            }
            EditDiscard => {
                let input: EditDiscardInput = decode(request.payload)?;
                Ok(NativeOutput::typed(
                    json!({"discarded":runtime.edits.discard(&context,input.draft_id).map_err(wire_error)?}),
                ))
            }
            MovementSourcePick | MovementDestinationPick => {
                let input: MovementPickInput = decode(request.payload)?;
                runtime
                    .state
                    .binding_receipt(&context, input.session, AuthorizePurpose::Read)
                    .await
                    .map_err(wire_error)?;
                let owner = native_owner
                    .filter(|owner| *owner != 0)
                    .ok_or_else(|| WBError::new("WB_FORBIDDEN"))?;
                let dialog = self
                    .dialog_slots
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| WBError::new("WB_LIMIT"))?;
                let destination = request.command == MovementDestinationPick;
                let format = input.format;
                let session = input.session;
                let files = runtime.files.clone();
                let native_context = context.clone();
                let memory = runtime
                    .streams
                    .budget()
                    .reserve(4 * 1024 * 1024)
                    .map_err(wire_error)?;
                let selected=tauri::async_runtime::spawn_blocking(move||->Result<Value,WorkbenchError>{
                    let _dialog=dialog;let Some(path)=crate::workbench_files::pick(owner,destination,format)?else{return Ok(if destination{json!({"destination":null})}else{json!({"source":null})})};
                    if destination{let selection=NativeDestinationSelection::from_native_dialog(&path)?;let handle=files.register_destination(&native_context,session,selection)?;Ok(json!({"destination":{"destinationHandle":handle,"displayName":files.destination_display(&native_context,session,handle)?}}))}
                    else{let selection=NativeSourceSelection::from_native_dialog(&path)?;let handle=files.register_source(&native_context,session,selection)?;match files.source_columns(&native_context,session,handle,format){Ok(source)=>Ok(json!({"source":source})),Err(error)=>{let _=files.release(&native_context,handle);Err(error)}}}
                }).await.map_err(|_|WBError::new("WB_IO"))?.map_err(wire_error)?;
                Ok(NativeOutput::typed(selected).hold(memory))
            }
            MovementFileRelease => {
                let input: MovementReleaseInput = decode(request.payload)?;
                let files = runtime.files.clone();
                let released = tauri::async_runtime::spawn_blocking(move || {
                    files.release(&context, input.handle)
                })
                .await
                .map_err(|_| WBError::new("WB_IO"))?
                .map_err(wire_error)?;
                Ok(NativeOutput::typed(json!({"released":released})))
            }
            ImportPreview => {
                let input: ImportPreviewInput = decode(request.payload)?;
                Ok(NativeOutput::typed(
                    runtime
                        .imports
                        .preview(&context, input)
                        .await
                        .map_err(wire_error)?,
                ))
            }
            ImportContinueReview => {
                let input: ImportContinueReviewInput = decode(request.payload)?;
                Ok(NativeOutput::typed(
                    runtime
                        .imports
                        .continue_review(
                            &context,
                            input.session,
                            input.parent_operation_id,
                            input.preview_id,
                        )
                        .await
                        .map_err(wire_error)?,
                ))
            }
            ImportStart => {
                let input: movement_dto::ImportStartInput = decode(request.payload)?;
                let token = request
                    .attempt_token
                    .ok_or_else(|| WBError::new("WB_PAYLOAD_INVALID"))?;
                Ok(NativeOutput::typed(
                    runtime
                        .imports
                        .start(&context, &token, input)
                        .await
                        .map_err(|error| movement_error(error.error, error.operation_id))?,
                ))
            }
            ImportContinue => {
                let input: movement_dto::ImportContinueInput = decode(request.payload)?;
                let token = request
                    .attempt_token
                    .ok_or_else(|| WBError::new("WB_PAYLOAD_INVALID"))?;
                Ok(NativeOutput::typed(
                    runtime
                        .imports
                        .continue_import(&context, &token, input)
                        .await
                        .map_err(|error| movement_error(error.error, error.operation_id))?,
                ))
            }
            ImportCancel => {
                let input: ImportCancelInput = decode(request.payload)?;
                Ok(NativeOutput::typed(
                    json!({"cancelRequested":runtime.imports.cancel(&context,input.parent_operation_id).map_err(wire_error)?}),
                ))
            }
            ExportStart => {
                let input: movement_dto::ExportStartInput = decode(request.payload)?;
                Ok(NativeOutput::typed(
                    runtime
                        .exports
                        .start(context, input)
                        .await
                        .map_err(|error| movement_error(error.error, error.operation_id))?,
                ))
            }
            ExportCancel => {
                let input: MovementOperationInput = decode(request.payload)?;
                Ok(NativeOutput::typed(
                    json!({"cancelRequested":runtime.exports.cancel(&context,input.operation_id).map_err(wire_error)?}),
                ))
            }
            MovementStatus => {
                let input: MovementStatusInput = decode(request.payload)?;
                let journal = runtime.journal.clone();
                let native_context = context.clone();
                let (scope, mut status) = tauri::async_runtime::spawn_blocking(move || {
                    let scope = journal.scope_for_operation(&native_context, input.operation_id)?;
                    let status = journal.movement_status(
                        &native_context,
                        &scope,
                        input.operation_id,
                        input.page,
                    )?;
                    Ok::<_, WorkbenchError>((scope, status))
                })
                .await
                .map_err(|_| WBError::new("WB_IO"))?
                .map_err(wire_error)?;
                if let Some(observation) = runtime
                    .exports
                    .observation(&context, &scope, input.operation_id)
                    .map_err(wire_error)?
                {
                    status.file = Some(observation);
                }
                Ok(NativeOutput::typed(status))
            }
            MovementPartialRemove => {
                let input: MovementOperationInput = decode(request.payload)?;
                let journal = runtime.journal.clone();
                let budget = runtime.streams.budget();
                let removed = tauri::async_runtime::spawn_blocking(
                    move || -> Result<bool, WorkbenchError> {
                        let scope = journal.scope_for_operation(&context, input.operation_id)?;
                        let Some(partial) =
                            journal.recorded_partial(&context, &scope, input.operation_id)?
                        else {
                            return Ok(false);
                        };
                        let removed =
                            restoreplane_workbench::movement_export::remove_recorded_partial(
                                &partial.path,
                                &partial.file_identity,
                                budget,
                            )?;
                        if !removed {
                            return Ok(false);
                        }
                        match journal.mark_partial_removed(
                            &context,
                            &scope,
                            input.operation_id,
                            partial.version,
                            &partial.file_identity,
                        ) {
                            Ok(_) => {}
                            Err(WorkbenchError::IdempotencyConflict) => {
                                let fresh = journal
                                    .recorded_partial(&context, &scope, input.operation_id)?
                                    .ok_or(WorkbenchError::IdempotencyConflict)?;
                                if fresh.path != partial.path
                                    || fresh.file_identity != partial.file_identity
                                {
                                    return Err(WorkbenchError::IdempotencyConflict);
                                }
                                journal.mark_partial_removed(
                                    &context,
                                    &scope,
                                    input.operation_id,
                                    fresh.version,
                                    &partial.file_identity,
                                )?;
                            }
                            Err(error) => return Err(error),
                        }
                        Ok(true)
                    },
                )
                .await
                .map_err(|_| WBError::new("WB_IO"))?
                .map_err(|error| movement_error(error, Some(input.operation_id)))?;
                Ok(NativeOutput::typed(json!({"removed":removed})))
            }
            _ => Err(WBError::new("WB_CAPABILITY_UNSUPPORTED")),
        }
    }
    async fn execute_metadata(
        &self,
        label: &str,
        request: WorkbenchRequest,
        channel: Option<Channel<tauri::ipc::InvokeResponseBody>>,
    ) -> Result<NativeOutput, WBError> {
        let runtime = self.runtime().await?;
        let context = self.context(&runtime, label)?;
        match request.command {
            WorkbenchCommand::ProtectionSelectionGet => {
                let input: ProtectionSelectionInput = decode(request.payload)?;
                Ok(NativeOutput::typed(
                    runtime
                        .protection
                        .selection(&context, input.session, input.objects)
                        .await
                        .map_err(wire_error)?,
                ))
            }
            WorkbenchCommand::CatalogPage => {
                let input: CatalogPageInput = decode(request.payload)?;
                input.page.validate()?;
                let page = runtime
                    .metadata
                    .page(
                        &context,
                        input.session,
                        input.parent,
                        input.kind,
                        input.search,
                        input.page.native(),
                    )
                    .await
                    .map_err(wire_error)?;
                Ok(NativeOutput::typed(page))
            }
            WorkbenchCommand::CatalogDescribe => {
                let input: CatalogDescribeInput = decode(request.payload)?;
                input.page.validate()?;
                let page = runtime
                    .metadata
                    .describe(
                        &context,
                        input.session,
                        input.object,
                        input.part,
                        input.page.native(),
                    )
                    .await
                    .map_err(wire_error)?;
                Ok(NativeOutput::typed(page))
            }
            WorkbenchCommand::CatalogGraph => {
                let input: CatalogGraphInput = decode(request.payload)?;
                input.page.validate()?;
                Ok(NativeOutput::typed(
                    runtime
                        .metadata
                        .graph(&context, input.session, input.schema, input.page.native())
                        .await
                        .map_err(wire_error)?,
                ))
            }
            WorkbenchCommand::BrowseOpen => {
                let input: BrowseOpenInput = decode(request.payload)?;
                let emitter = channel.map(notice_emitter);
                let result = runtime
                    .browse
                    .open(
                        &context,
                        input.session,
                        NativeBrowseInput {
                            object: input.object,
                            filter: input.filter,
                            order: input.order,
                            snapshot_id: input.snapshot_id,
                            limits: input.limits,
                        },
                        emitter,
                    )
                    .await
                    .map_err(wire_error)?;
                Ok(NativeOutput::typed(result))
            }
            WorkbenchCommand::BrowsePage => {
                let input: BrowsePageInput = decode(request.payload)?;
                let page = runtime
                    .browse
                    .page(&context, input.session, input.cursor, input.limit)
                    .await
                    .map_err(wire_error)?;
                Ok(NativeOutput::projected(page, |value, writer| {
                    serde_json::to_writer(writer, value.result()).map_err(|_| WBError::new("WB_IO"))
                }))
            }
            WorkbenchCommand::SnapshotBegin => {
                let input: SnapshotBeginInput = decode(request.payload)?;
                let (id, expires) = runtime
                    .browse
                    .snapshot_begin(
                        &context,
                        input.session,
                        &input.isolation,
                        input.max_lifetime_ms,
                    )
                    .await
                    .map_err(wire_error)?;
                Ok(NativeOutput::value(
                    json!({"snapshotId":id,"expiresAtMs":expires.to_string()}),
                ))
            }
            WorkbenchCommand::SnapshotEnd => {
                let input: SnapshotEndInput = decode(request.payload)?;
                let ended = runtime
                    .browse
                    .snapshot_end(&context, input.session, input.snapshot_id)
                    .await
                    .map_err(wire_error)?;
                Ok(NativeOutput::value(json!({"ended":ended})))
            }
            WorkbenchCommand::ResultAttach => {
                let input: ResultAttachInput = decode(request.payload)?;
                let channel = channel.ok_or_else(|| {
                    WBError::new("WB_PAYLOAD_INVALID").with_reason("result_notice_channel_required")
                })?;
                let terminal = runtime
                    .streams
                    .attach_pull(
                        &context,
                        &input.identity,
                        input.after_sequence.map(DecimalU64::value),
                        notice_emitter(channel),
                    )
                    .map_err(wire_error)?;
                let attached = runtime
                    .streams
                    .is_retained(&context, &input.identity)
                    .map_err(wire_error)?;
                let terminal = terminal
                    .filter(|frame| {
                        !attached
                            || input.after_sequence.is_some_and(|after| {
                                frame.envelope().sequence.value() <= after.value()
                            })
                    })
                    .map(|frame| frame.envelope().clone());
                Ok(NativeOutput::value(
                    json!({"attached":attached,"terminal":terminal}),
                ))
            }
            WorkbenchCommand::ResultFrames => {
                let input: ResultAttachInput = decode(request.payload)?;
                let pull = runtime
                    .streams
                    .frames(
                        &context,
                        &input.identity,
                        input.after_sequence.map(DecimalU64::value),
                    )
                    .map_err(wire_error)?;
                Ok(NativeOutput::frames(pull))
            }
            WorkbenchCommand::ResultAck => {
                let ack: Ack = decode(request.payload)?;
                Ok(NativeOutput::typed(
                    runtime.streams.ack(&context, &ack).map_err(wire_error)?,
                ))
            }
            WorkbenchCommand::ResultPage => {
                let input: ResultPageInput = decode(request.payload)?;
                let page = runtime
                    .streams
                    .page(
                        &context,
                        &input.identity,
                        input.start_row.value(),
                        input.limit,
                    )
                    .map_err(wire_error)?;
                Ok(NativeOutput::projected(page, |value, writer| {
                    serde_json::to_writer(writer, value.page()).map_err(|_| WBError::new("WB_IO"))
                }))
            }
            _ => Err(WBError::new("WB_CAPABILITY_UNSUPPORTED")),
        }
    }

    async fn execute_queries(
        &self,
        label: &str,
        request: WorkbenchRequest,
    ) -> Result<NativeOutput, WBError> {
        let runtime = self.runtime().await?;
        let context = self.context(&runtime, label)?;
        match request.command {
            WorkbenchCommand::QueryReview => Ok(NativeOutput::typed(
                runtime
                    .queries
                    .review(
                        &context,
                        decode::<query_dto::QueryReviewInput>(request.payload)?,
                    )
                    .await
                    .map_err(wire_error)?,
            )),
            WorkbenchCommand::QueryRun => {
                let token = request.attempt_token.ok_or_else(|| {
                    WBError::new("WB_PAYLOAD_INVALID").with_reason("attempt_token_required")
                })?;
                Ok(NativeOutput::typed(
                    runtime
                        .queries
                        .run(
                            &context,
                            token,
                            decode::<query_dto::QueryRunInput>(request.payload)?,
                        )
                        .await
                        .map_err(wire_error)?,
                ))
            }
            WorkbenchCommand::QueryResults => Ok(NativeOutput::typed(
                runtime
                    .queries
                    .results(
                        &context,
                        decode::<query_dto::QueryResultsInput>(request.payload)?,
                    )
                    .await
                    .map_err(wire_error)?,
            )),
            WorkbenchCommand::QueryCancel => Ok(NativeOutput::typed(
                runtime
                    .queries
                    .cancel(
                        &context,
                        decode::<query_dto::QueryCancelInput>(request.payload)?,
                    )
                    .await
                    .map_err(wire_error)?,
            )),
            WorkbenchCommand::TransactionBegin => Ok(NativeOutput::typed(
                runtime
                    .queries
                    .transaction_begin(
                        &context,
                        decode::<query_dto::TransactionBeginInput>(request.payload)?,
                    )
                    .await
                    .map_err(wire_error)?,
            )),
            WorkbenchCommand::TransactionGet => {
                let input: SessionOnly = decode(request.payload)?;
                Ok(NativeOutput::typed(
                    runtime
                        .queries
                        .transaction_get(&context, input.session)
                        .await
                        .map_err(wire_error)?,
                ))
            }
            WorkbenchCommand::TransactionCommit => {
                let token = request.attempt_token.ok_or_else(|| {
                    WBError::new("WB_PAYLOAD_INVALID").with_reason("attempt_token_required")
                })?;
                Ok(NativeOutput::typed(
                    runtime
                        .queries
                        .transaction_commit(
                            &context,
                            token,
                            decode::<query_dto::TransactionInput>(request.payload)?,
                        )
                        .await
                        .map_err(wire_error)?,
                ))
            }
            WorkbenchCommand::TransactionRollback => Ok(NativeOutput::typed(
                runtime
                    .queries
                    .transaction_rollback(
                        &context,
                        decode::<query_dto::TransactionInput>(request.payload)?,
                    )
                    .await
                    .map_err(wire_error)?,
            )),
            WorkbenchCommand::SavedQueryList => {
                let input: SavedListInput = decode(request.payload)?;
                Ok(NativeOutput::typed(
                    runtime
                        .saved_queries
                        .list(&context, input.scope, input.page)
                        .await
                        .map_err(wire_error)?,
                ))
            }
            WorkbenchCommand::SavedQueryGet => {
                let input: SavedGetInput = decode(request.payload)?;
                Ok(NativeOutput::typed(
                    runtime
                        .saved_queries
                        .get(&context, input.query_id)
                        .await
                        .map_err(wire_error)?,
                ))
            }
            WorkbenchCommand::SavedQueryPut => Ok(NativeOutput::typed(
                runtime
                    .saved_queries
                    .put(
                        &context,
                        decode::<query_dto::SavedQueryPut>(request.payload)?,
                    )
                    .await
                    .map_err(wire_error)?,
            )),
            WorkbenchCommand::SavedQueryDelete => {
                let input: SavedDeleteInput = decode(request.payload)?;
                let deleted = runtime
                    .saved_queries
                    .delete(&context, input.query_id, input.expected_version.value())
                    .await
                    .map_err(wire_error)?;
                Ok(NativeOutput::value(json!({"deleted":deleted})))
            }
            _ => Err(WBError::new("WB_CAPABILITY_UNSUPPORTED")),
        }
    }
    async fn execute_value(
        &self,
        label: &str,
        request: WorkbenchRequest,
    ) -> Result<Value, WBError> {
        let runtime = self.runtime().await?;
        let context = self.context(&runtime, label)?;
        match request.command {
            WorkbenchCommand::SessionOpen => {
                let input: OpenInput = decode(request.payload)?;
                if input.restore_work_item_id.is_some() {
                    if input.mode != Mode::ReadOnly {
                        return Err(WBError::new("WB_FORBIDDEN"));
                    }
                    let work_id = input
                        .restore_work_item_id
                        .ok_or_else(|| WBError::new("WB_PAYLOAD_INVALID"))?;
                    let outcome = IpcClient::production()
                        .execute(
                            ApplicationCommand::WorkGet,
                            None,
                            json!({
                                "workspaceId":input.scope.workspace_id,"workItemId":work_id,
                                "includeWorkbenchRestoreProof":true,
                            }),
                        )
                        .await
                        .map_err(|_| wire_error(WorkbenchError::ServiceUnavailable))?;
                    if !outcome.is_success() {
                        return Err(wire_error(if outcome.code() == "APPLICATION_FORBIDDEN" {
                            WorkbenchError::Forbidden
                        } else {
                            WorkbenchError::BindingUnproven
                        }));
                    }
                    let proof = outcome
                        .result()
                        .get("workbenchRestoreProof")
                        .filter(|proof| !proof.is_null())
                        .ok_or_else(|| wire_error(WorkbenchError::Unsupported))?;
                    let proof: restoreplane_domain::workbench_protection::WorkbenchRestoreProof =
                        serde_json::from_value(proof.clone())
                            .map_err(|_| wire_error(WorkbenchError::BindingUnproven))?;
                    if proof.restore_work_item_id.as_str() != work_id.to_string() {
                        return Err(wire_error(WorkbenchError::BindingUnproven));
                    }
                    let check = RestoredSessionCheck::from_authenticated_work(
                        context.principal().clone(),
                        input.scope.workspace_id.clone(),
                        proof,
                    )
                    .map_err(wire_error)?;
                    return encode(
                        runtime
                            .state
                            .open_checked(
                                &context,
                                input.scope,
                                input.credential_ref,
                                input.expected_descriptor_revision.as_deref(),
                                check,
                            )
                            .await
                            .map_err(wire_error)?,
                    );
                }
                encode(
                    runtime
                        .state
                        .open(
                            &context,
                            input.scope,
                            input.credential_ref,
                            input.expected_descriptor_revision.as_deref(),
                            input.mode,
                        )
                        .await
                        .map_err(wire_error)?,
                )
            }
            WorkbenchCommand::SessionList => {
                let input: ListInput = decode(request.payload)?;
                input.page.validate()?;
                let values = runtime
                    .state
                    .list(&context, &input.scope)
                    .map_err(wire_error)?;
                self.list_page(
                    values,
                    "sessionId",
                    "sessions",
                    context.window_instance_id(),
                    input.scope,
                    input.page,
                )
            }
            WorkbenchCommand::SessionReconnect => {
                let input: ReconnectInput = decode(request.payload)?;
                runtime
                    .edits
                    .retire_session(&context, input.session)
                    .map_err(wire_error)?;
                runtime
                    .imports
                    .retire_session(&context, input.session)
                    .map_err(wire_error)?;
                runtime
                    .exports
                    .retire_session(&context, input.session)
                    .map_err(wire_error)?;
                runtime
                    .files
                    .retire_session(&context, input.session)
                    .map_err(wire_error)?;
                runtime
                    .streams
                    .retire_session(&context, input.session)
                    .map_err(wire_error)?;
                runtime
                    .browse
                    .retire_session(&context, input.session)
                    .await
                    .map_err(wire_error)?;
                runtime
                    .metadata
                    .retire_session(&context, input.session)
                    .map_err(wire_error)?;
                encode(
                    runtime
                        .queries
                        .reconnect(&context, input.session, &input.expected_descriptor_revision)
                        .await
                        .map_err(wire_error)?,
                )
            }
            WorkbenchCommand::SessionModeSet => {
                let input: ModeSetInput = decode(request.payload)?;
                let session = runtime
                    .state
                    .set_mode(&context, input.session, input.mode)
                    .await
                    .map_err(wire_error)?;
                runtime
                    .edits
                    .retire_session(&context, input.session)
                    .map_err(wire_error)?;
                runtime
                    .imports
                    .retire_session(&context, input.session)
                    .map_err(wire_error)?;
                runtime
                    .exports
                    .retire_session(&context, input.session)
                    .map_err(wire_error)?;
                runtime
                    .files
                    .retire_session(&context, input.session)
                    .map_err(wire_error)?;
                runtime
                    .streams
                    .retire_session(&context, input.session)
                    .map_err(wire_error)?;
                let _ = runtime
                    .queries
                    .retire_session(&context, input.session, false)
                    .await
                    .map_err(wire_error)?;
                runtime
                    .browse
                    .retire_session(&context, input.session)
                    .await
                    .map_err(wire_error)?;
                runtime
                    .metadata
                    .retire_session(&context, input.session)
                    .map_err(wire_error)?;
                encode(session)
            }
            WorkbenchCommand::SessionClose => {
                let input: CloseInput = decode(request.payload)?;
                runtime
                    .edits
                    .retire_session(&context, input.session)
                    .map_err(wire_error)?;
                runtime
                    .imports
                    .retire_session(&context, input.session)
                    .map_err(wire_error)?;
                runtime
                    .exports
                    .retire_session(&context, input.session)
                    .map_err(wire_error)?;
                runtime
                    .files
                    .retire_session(&context, input.session)
                    .map_err(wire_error)?;
                runtime
                    .streams
                    .retire_session(&context, input.session)
                    .map_err(wire_error)?;
                runtime
                    .browse
                    .retire_session(&context, input.session)
                    .await
                    .map_err(wire_error)?;
                runtime
                    .metadata
                    .retire_session(&context, input.session)
                    .map_err(wire_error)?;
                let transaction = runtime
                    .queries
                    .retire_session(
                        &context,
                        input.session,
                        matches!(input.disposition, CloseDisposition::RollbackRequest),
                    )
                    .await
                    .map_err(wire_error)?;
                let state = if transaction.as_ref().is_some_and(|transaction| {
                    transaction.state
                        == restoreplane_domain::workbench::TransactionState::OutcomeUnknown
                }) {
                    "retiring"
                } else {
                    "closed"
                };
                Ok(json!({"state":state,"transaction":transaction}))
            }
            WorkbenchCommand::CredentialPut => {
                let mut input: PutCredential = decode(request.payload)?;
                runtime
                    .authorizer
                    .authorize(&context, &input.scope, AuthorizePurpose::Connect, None)
                    .await
                    .map_err(wire_error)?;
                let secret = Zeroizing::new(std::mem::take(&mut input.secret));
                let credentials = runtime.credentials.clone();
                let owner_context = context.clone();
                let reference = tauri::async_runtime::spawn_blocking(move || {
                    credentials.put(
                        &owner_context,
                        input.scope,
                        input.credential_ref,
                        input.label,
                        input.username,
                        secret,
                    )
                })
                .await
                .map_err(|_| WBError::new("WB_IO"))?
                .map_err(wire_error)?;
                runtime
                    .state
                    .invalidate_credential(&context, reference)
                    .await
                    .map_err(wire_error)?;
                Ok(json!({"credentialRef":reference}))
            }
            WorkbenchCommand::CredentialList => {
                let input: ListInput = decode(request.payload)?;
                input.page.validate()?;
                runtime
                    .authorizer
                    .authorize(&context, &input.scope, AuthorizePurpose::Connect, None)
                    .await
                    .map_err(wire_error)?;
                let credentials = runtime.credentials.clone();
                let owner_context = context.clone();
                let scope = input.scope.clone();
                let values = tauri::async_runtime::spawn_blocking(move || {
                    credentials.list(&owner_context, &scope)
                })
                .await
                .map_err(|_| WBError::new("WB_IO"))?
                .map_err(wire_error)?;
                self.list_page(
                    values,
                    "credentialRef",
                    "credentials",
                    context.window_instance_id(),
                    input.scope,
                    input.page,
                )
            }
            WorkbenchCommand::CredentialDelete => {
                let input: DeleteCredential = decode(request.payload)?;
                runtime
                    .state
                    .invalidate_credential(&context, input.credential_ref)
                    .await
                    .map_err(wire_error)?;
                let credentials = runtime.credentials.clone();
                let owner_context = context.clone();
                let deletion = tauri::async_runtime::spawn_blocking(move || {
                    credentials.delete(&owner_context, input.credential_ref)
                })
                .await;
                // Bracket the file change: pending opens after the first scan
                // must also lose their credential epoch before this returns.
                runtime
                    .state
                    .invalidate_credential(&context, input.credential_ref)
                    .await
                    .map_err(wire_error)?;
                let deleted = deletion
                    .map_err(|_| WBError::new("WB_IO"))?
                    .map_err(wire_error)?;
                Ok(json!({"deleted":deleted}))
            }
            WorkbenchCommand::OperationPrepare => {
                let input: PrepareInput = decode(request.payload)?;
                input.intent.validate()?;
                match input.intent {
                    PreparedIntent::QueryRun(input) => {
                        return encode(
                            runtime
                                .queries
                                .prepare_query(&context, input)
                                .await
                                .map_err(wire_error)?,
                        )
                    }
                    PreparedIntent::TransactionCommit(input) => {
                        return encode(
                            runtime
                                .queries
                                .prepare_commit(&context, input)
                                .await
                                .map_err(wire_error)?,
                        )
                    }
                    PreparedIntent::EditApply(input) => encode(
                        runtime
                            .edits
                            .prepare_apply(&context, input)
                            .await
                            .map_err(wire_error)?,
                    ),
                    PreparedIntent::ImportStart(input) => encode(
                        runtime
                            .imports
                            .prepare_start(&context, &input)
                            .await
                            .map_err(wire_error)?,
                    ),
                    PreparedIntent::ImportContinue(input) => encode(
                        runtime
                            .imports
                            .prepare_continue(&context, &input)
                            .await
                            .map_err(wire_error)?,
                    ),
                }
            }
            WorkbenchCommand::OperationGet => {
                let input: GetOperationInput = decode(request.payload)?;
                let journal = runtime.journal.clone();
                tauri::async_runtime::spawn_blocking(move||{
                    let scope=journal.scope_for_operation(&context,input.operation_id).map_err(wire_error)?;
                    let operation=journal.get(&context,&scope,input.operation_id).map_err(wire_error)?;
                    let unchanged=journal.write_enabled()&&input.known_version==Some(operation.version);
                    Ok(json!({"operation":if unchanged{None}else{Some(operation)},"unchanged":unchanged}))
                }).await.map_err(|_|WBError::new("WB_IO"))?
            }
            WorkbenchCommand::OperationList => {
                let input: OperationListInput = decode(request.payload)?;
                input.page.validate()?;
                self.journal_page(runtime, context, input.scope, input.state, None, input.page)
                    .await
            }
            WorkbenchCommand::OperationStatements => {
                let input: OperationStatementsInput = decode(request.payload)?;
                input.page.validate()?;
                let journal = runtime.journal.clone();
                let owner_context = context.clone();
                let scope = tauri::async_runtime::spawn_blocking(move || {
                    journal.scope_for_operation(&owner_context, input.operation_id)
                })
                .await
                .map_err(|_| WBError::new("WB_IO"))?
                .map_err(wire_error)?;
                self.journal_page(
                    runtime,
                    context,
                    scope,
                    None,
                    Some(input.operation_id),
                    input.page,
                )
                .await
            }
            WorkbenchCommand::OperationAcknowledgeUnknown => {
                let input: UnknownAckInput = decode(request.payload)?;
                let journal = runtime.journal.clone();
                let operation = tauri::async_runtime::spawn_blocking(move || {
                    let scope = journal.scope_for_operation(&context, input.operation_id)?;
                    journal.acknowledge_unknown(
                        &context,
                        &scope,
                        input.operation_id,
                        input.expected_version.value(),
                    )
                })
                .await
                .map_err(|_| WBError::new("WB_IO"))?
                .map_err(wire_error)?;
                encode(operation)
            }
            _ => Err(WBError::new("WB_CAPABILITY_UNSUPPORTED")
                .with_reason("feature_section_not_enabled")),
        }
    }
    async fn journal_page(
        &self,
        runtime: Arc<Runtime>,
        context: InvocationContext,
        scope: Scope,
        state: Option<OperationState>,
        operation_id: Option<Uuid>,
        input: PageInput,
    ) -> Result<Value, WBError> {
        let now = std::time::Instant::now();
        let window = context.window_instance_id();
        let position = {
            let mut cursors = self
                .journal_cursors
                .lock()
                .map_err(|_| WBError::new("WB_IO"))?;
            cursors.retain(|_, c| c.expires > now);
            match &input.cursor {
                None => None,
                Some(token) => {
                    let token =
                        Uuid::parse_str(token).map_err(|_| WBError::new("WB_CURSOR_INVALID"))?;
                    let cursor = cursors
                        .get(&token)
                        .ok_or_else(|| WBError::new("WB_CURSOR_INVALID"))?;
                    let matching = match (&cursor.position, operation_id) {
                        (JournalPosition::Operations { state: s, .. }, None) => *s == state,
                        (
                            JournalPosition::Statements {
                                operation_id: id, ..
                            },
                            Some(current),
                        ) => *id == current,
                        _ => false,
                    };
                    if cursor.window != window || cursor.scope != scope || !matching {
                        return Err(WBError::new("WB_CURSOR_INVALID"));
                    }
                    Some(cursor.position.clone())
                }
            }
        };
        let journal = runtime.journal.clone();
        let owner_scope = scope.clone();
        let limit = input.limit;
        let (items, next) = tauri::async_runtime::spawn_blocking(
            move || -> Result<(Value, Option<JournalPosition>), WBError> {
                if let Some(id) = operation_id {
                    let after = match position {
                        Some(JournalPosition::Statements { after, .. }) => Some(after),
                        _ => None,
                    };
                    let values = journal
                        .statements(&context, &owner_scope, id, after, limit)
                        .map_err(wire_error)?;
                    let next = if values.len() == limit {
                        values.last().map(|v| JournalPosition::Statements {
                            operation_id: id,
                            after: v.ordinal,
                        })
                    } else {
                        None
                    };
                    Ok((encode(values)?, next))
                } else {
                    let after = match position {
                        Some(JournalPosition::Operations { after, .. }) => Some(after),
                        _ => None,
                    };
                    let mut values = journal
                        .list(&context, &owner_scope, state, after, limit)
                        .map_err(wire_error)?;
                    let next = if values.len() == limit {
                        values.last().map(|v| JournalPosition::Operations {
                            state,
                            after: (v.updated_at_ms.value(), v.operation_id),
                        })
                    } else {
                        None
                    };
                    for value in &mut values {
                        value.statements.clear();
                    }
                    Ok((encode(values)?, next))
                }
            },
        )
        .await
        .map_err(|_| WBError::new("WB_IO"))??;
        let next_cursor = {
            let mut cursors = self
                .journal_cursors
                .lock()
                .map_err(|_| WBError::new("WB_IO"))?;
            if let Some(consumed) = input.cursor.as_ref().and_then(|s| Uuid::parse_str(s).ok()) {
                cursors.remove(&consumed);
            }
            if let Some(position) = next {
                if cursors.len() >= 256 {
                    return Err(WBError::new("WB_LIMIT"));
                }
                let token = Uuid::new_v4();
                cursors.insert(
                    token,
                    JournalCursor {
                        window,
                        scope,
                        position,
                        expires: now + std::time::Duration::from_secs(300),
                    },
                );
                Some(token)
            } else {
                None
            }
        };
        let page = json!({"items":items,"nextCursor":next_cursor,"completeness":if next_cursor.is_some(){"limited"}else{"complete"},"count":{"kind":"unknown"},"asOfMs":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|_|WBError::new("WB_IO"))?.as_millis().to_string()});
        if serde_json::to_vec(&page)
            .map_err(|_| WBError::new("WB_IO"))?
            .len()
            > 512 * 1024
        {
            return Err(WBError::new("WB_LIMIT"));
        }
        Ok(page)
    }
    fn list_page<T: Serialize>(
        &self,
        items: Vec<T>,
        id_field: &str,
        kind: &'static str,
        window: Uuid,
        scope: Scope,
        input: PageInput,
    ) -> Result<Value, WBError> {
        let mut cursors = self.cursors.lock().map_err(|_| WBError::new("WB_IO"))?;
        let after = match input.cursor {
            Some(token) => {
                let token =
                    Uuid::parse_str(&token).map_err(|_| WBError::new("WB_CURSOR_INVALID"))?;
                let cursor = cursors
                    .get(&token)
                    .ok_or_else(|| WBError::new("WB_CURSOR_INVALID"))?;
                if cursor.window != window || cursor.scope != scope || cursor.kind != kind {
                    return Err(WBError::new("WB_CURSOR_INVALID"));
                }
                Some(cursor.after)
            }
            None => None,
        };
        let total = items.len();
        let mut selected = Vec::new();
        for item in items {
            let value = encode(item)?;
            let id = Uuid::parse_str(
                value[id_field]
                    .as_str()
                    .ok_or_else(|| WBError::new("WB_IO"))?,
            )
            .map_err(|_| WBError::new("WB_IO"))?;
            if after.is_none_or(|after| id > after) {
                selected.push((id, value));
            }
        }
        selected.sort_by_key(|(id, _)| *id);
        let has_more = selected.len() > input.limit;
        selected.truncate(input.limit);
        let next = if has_more {
            if cursors.len() >= 256 {
                return Err(WBError::new("WB_LIMIT").with_reason("cursor_capacity"));
            }
            let token = Uuid::new_v4();
            let after = selected.last().ok_or_else(|| WBError::new("WB_IO"))?.0;
            cursors.insert(
                token,
                ListCursor {
                    window,
                    scope,
                    kind,
                    after,
                },
            );
            Some(token)
        } else {
            None
        };
        let value = json!({"count":{"kind":"exact","value":total.to_string()},"items":selected.into_iter().map(|(_,value)|value).collect::<Vec<_>>(),"nextCursor":next,"completeness":if has_more{"limited"}else{"complete"},"asOfMs":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|_|WBError::new("WB_IO"))?.as_millis().to_string()});
        if serde_json::to_vec(&value)
            .map_err(|_| WBError::new("WB_IO"))?
            .len()
            > 512 * 1024
        {
            return Err(WBError::new("WB_LIMIT"));
        }
        Ok(value)
    }
}

fn wire_error(error: WorkbenchError) -> WBError {
    let mut wire = WBError::new(error.code());
    match error {
        WorkbenchError::EditRefused(reason)
            if !reason.is_empty()
                && reason.len() <= 128
                && reason
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') =>
        {
            wire.details.reason = Some(reason)
        }
        WorkbenchError::EditConflict(id) => wire.details.conflict_id = Some(id),
        _ => {}
    }
    wire
}
fn movement_error(error: WorkbenchError, operation_id: Option<Uuid>) -> WBError {
    let mut wire = wire_error(error);
    wire.operation_id = operation_id;
    wire
}
fn notice_emitter(channel: Channel<tauri::ipc::InvokeResponseBody>) -> Arc<ResultNoticeEmitter> {
    Arc::new(move |notice: &ResultNotice| {
        let json = serde_json::to_string(notice).map_err(|_| WorkbenchError::Io)?;
        if json.len() >= 1024 {
            return Err(WorkbenchError::Limit);
        }
        channel
            .send(tauri::ipc::InvokeResponseBody::Json(json))
            .map_err(|_| WorkbenchError::Io)
    })
}
fn verified_snapshot_matrix() -> Result<SnapshotCertification, WorkbenchError> {
    // Exact versions backed by executed owned-fixture isolation/TTL tests. This
    // certifies snapshot behavior only; it does not certify edits/read leases.
    SnapshotCertification::from_verified_build(
        "section04-owned-verify-full-three-engine-isolation;section04-real-sqlite-isolation".into(),
        verified_engine_versions(),
    )
}
fn verified_write_matrix() -> Result<WriteCertification, WorkbenchError> {
    WriteCertification::from_verified_build(
        "section06-native-20261117-current-exact".into(),
        verified_engine_versions(),
    )
}
fn verified_engine_versions() -> Vec<(Engine, String)> {
    vec![
            (Engine::Postgresql, "PostgreSQL 17.11 on x86_64-pc-linux-musl, compiled by gcc (Alpine 15.2.0) 15.2.0, 64-bit".into()),
            (Engine::Mysql, "8.4.13".into()),
            (Engine::Mariadb, "11.4.13-MariaDB-ubu2404".into()),
            (Engine::Sqlite, "3.54.1".into()),
        ]
}
fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, WBError> {
    serde_json::from_value(value).map_err(|_| WBError::new("WB_PAYLOAD_INVALID"))
}
fn encode<T: Serialize>(value: T) -> Result<Value, WBError> {
    serde_json::to_value(value).map_err(|_| WBError::new("WB_IO"))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OpenInput {
    scope: Scope,
    credential_ref: Uuid,
    expected_descriptor_revision: Option<String>,
    mode: Mode,
    #[serde(default)]
    restore_work_item_id: Option<Uuid>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ListInput {
    scope: Scope,
    page: PageInput,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PageInput {
    cursor: Option<String>,
    #[serde(default = "default_page_limit")]
    limit: usize,
}
fn default_page_limit() -> usize {
    100
}
impl PageInput {
    fn native(self) -> metadata_dto::PageInput {
        metadata_dto::PageInput {
            cursor: self.cursor,
            limit: self.limit,
        }
    }
    fn validate(&self) -> Result<(), WBError> {
        if self.limit == 0 || self.limit > 200 {
            Err(WBError::new("WB_LIMIT"))
        } else {
            Ok(())
        }
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReconnectInput {
    session: SessionRef,
    expected_descriptor_revision: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CloseInput {
    session: SessionRef,
    disposition: CloseDisposition,
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum CloseDisposition {
    RollbackRequest,
    Disconnect,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PutCredential {
    scope: Scope,
    credential_ref: Option<Uuid>,
    label: String,
    username: String,
    secret: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeleteCredential {
    credential_ref: Uuid,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SessionOnly {
    session: SessionRef,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProtectionSelectionInput {
    session: SessionRef,
    objects: Vec<metadata_dto::ObjectRef>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ModeSetInput {
    session: SessionRef,
    mode: Mode,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SavedListInput {
    scope: Scope,
    page: metadata_dto::PageInput,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SavedGetInput {
    query_id: Uuid,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SavedDeleteInput {
    query_id: Uuid,
    expected_version: DecimalU64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CatalogPageInput {
    session: SessionRef,
    parent: Option<metadata_dto::ObjectRef>,
    kind: metadata_dto::CatalogKind,
    search: String,
    page: PageInput,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CatalogDescribeInput {
    session: SessionRef,
    object: metadata_dto::ObjectRef,
    part: metadata_dto::DescribePart,
    page: PageInput,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CatalogGraphInput {
    session: SessionRef,
    schema: metadata_dto::ObjectRef,
    page: PageInput,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BrowseOpenInput {
    session: SessionRef,
    object: metadata_dto::ObjectRef,
    filter: metadata_dto::Filter,
    order: Vec<metadata_dto::Order>,
    snapshot_id: Option<Uuid>,
    limits: metadata_dto::Limits,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BrowsePageInput {
    session: SessionRef,
    cursor: String,
    #[serde(default = "default_page_limit")]
    limit: usize,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SnapshotBeginInput {
    session: SessionRef,
    isolation: String,
    max_lifetime_ms: u64,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SnapshotEndInput {
    session: SessionRef,
    snapshot_id: Uuid,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ResultAttachInput {
    identity: StreamIdentity,
    after_sequence: Option<DecimalU64>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ResultPageInput {
    identity: StreamIdentity,
    start_row: DecimalU64,
    #[serde(default = "default_page_limit")]
    limit: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GetOperationInput {
    operation_id: Uuid,
    known_version: Option<DecimalU64>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OperationListInput {
    scope: Scope,
    state: Option<OperationState>,
    page: PageInput,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OperationStatementsInput {
    operation_id: Uuid,
    page: PageInput,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UnknownAckInput {
    operation_id: Uuid,
    expected_version: DecimalU64,
    #[serde(rename = "acknowledgment")]
    _acknowledgment: UnknownAcknowledgment,
}
#[derive(Deserialize)]
enum UnknownAcknowledgment {
    #[serde(rename = "uncertainty_understood")]
    UncertaintyUnderstood,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PrepareInput {
    intent: PreparedIntent,
}
#[derive(Deserialize, Serialize)]
#[serde(tag = "command", content = "payload", deny_unknown_fields)]
enum PreparedIntent {
    #[serde(rename = "query.run")]
    QueryRun(query_dto::QueryRunInput),
    #[serde(rename = "transaction.commit")]
    TransactionCommit(query_dto::TransactionInput),
    #[serde(rename = "edit.apply")]
    EditApply(edit_dto::EditApplyInput),
    #[serde(rename = "import.start")]
    ImportStart(movement_dto::ImportStartInput),
    #[serde(rename = "import.continue")]
    ImportContinue(movement_dto::ImportContinueInput),
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EditDiscardInput {
    draft_id: Uuid,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EditConflictInput {
    session: SessionRef,
    conflict_id: Uuid,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MovementPickInput {
    session: SessionRef,
    format: movement_dto::MovementFormat,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MovementReleaseInput {
    handle: Uuid,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MovementOperationInput {
    operation_id: Uuid,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MovementStatusInput {
    operation_id: Uuid,
    page: metadata_dto::PageInput,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ImportCancelInput {
    parent_operation_id: Uuid,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ImportContinueReviewInput {
    session: SessionRef,
    parent_operation_id: Uuid,
    preview_id: Uuid,
}
impl PreparedIntent {
    fn session(&self) -> SessionRef {
        match self {
            Self::QueryRun(v) => v.session,
            Self::TransactionCommit(v) => v.session,
            Self::EditApply(v) => v.session,
            Self::ImportStart(v) => v.session,
            Self::ImportContinue(v) => v.session,
        }
    }
    fn validate(&self) -> Result<(), WBError> {
        let session = self.session();
        if session.session_id.is_nil() || session.connection_generation.value() == 0 {
            return Err(WBError::new("WB_PAYLOAD_INVALID"));
        }
        let warnings = match self {
            Self::QueryRun(v) => {
                if v.review_id.is_nil()
                    || v.limits.max_rows == 0
                    || v.limits.max_rows > 1_000_000
                    || v.limits.max_bytes == 0
                    || v.limits.max_bytes > 64 * 1024 * 1024
                    || v.limits.statement_timeout_ms == 0
                    || v.limits.statement_timeout_ms > 30_000
                    || v.limits.lock_wait_ms == 0
                    || v.limits.lock_wait_ms > 2_000
                {
                    return Err(WBError::new("WB_LIMIT"));
                }
                Some(&v.confirmed_warnings)
            }
            Self::EditApply(v) => {
                if v.draft_id.is_nil() || v.review_id.is_nil() {
                    return Err(WBError::new("WB_PAYLOAD_INVALID"));
                }
                Some(&v.confirmed_warnings)
            }
            Self::ImportStart(v) => {
                if v.preview_id.is_nil() {
                    return Err(WBError::new("WB_PAYLOAD_INVALID"));
                }
                Some(&v.confirmed_warnings)
            }
            Self::ImportContinue(v) => {
                if v.parent_operation_id.is_nil() || v.review_id.is_nil() {
                    return Err(WBError::new("WB_PAYLOAD_INVALID"));
                }
                None
            }
            Self::TransactionCommit(v) => {
                if v.transaction_id.is_nil() {
                    return Err(WBError::new("WB_PAYLOAD_INVALID"));
                }
                None
            }
        };
        if warnings.is_some_and(|values| values.len() > 32 || values.iter().any(|v| v.len() > 256))
        {
            return Err(WBError::new("WB_LIMIT"));
        }
        Ok(())
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchReply {
    ok: bool,
    request_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<WBError>,
}

#[tauri::command]
pub async fn workbench_request(
    window: tauri::WebviewWindow,
    webview: tauri::Webview,
    state: tauri::State<'_, Arc<DesktopWorkbench>>,
    request: String,
    on_result: Option<Value>,
) -> Result<tauri::ipc::Response, WBError> {
    // Channel itself implements CommandArg, not Deserialize, so Option<Channel>
    // cannot be an optional Tauri argument. Resolve its documented ID against
    // the native invoking Webview; callers still pass an optional JS Channel.
    let channel: Result<Option<Channel<tauri::ipc::InvokeResponseBody>>, WBError> = on_result
        .map(serde_json::from_value::<JavaScriptChannelId>)
        .transpose()
        .map(|id| id.map(|id| id.channel_on(webview)))
        .map_err(|_| WBError::new("WB_PAYLOAD_INVALID").with_reason("invalid_result_channel"));
    let raw = Zeroizing::new(request);
    let request = WorkbenchRequest::decode_json(&raw);
    let request_id = request
        .as_ref()
        .map(|r| r.request_id.to_string())
        .unwrap_or_else(|_| Uuid::new_v4().to_string());
    #[cfg(windows)]
    let native_owner = window.hwnd().ok().map(|handle| handle.0 as isize);
    #[cfg(not(windows))]
    let native_owner = None;
    let outcome = match (request, channel) {
        (Ok(request), Ok(channel)) => {
            state
                .execute(window.label(), native_owner, request, channel)
                .await
        }
        (Err(error), _) | (_, Err(error)) => Err(error),
    };
    match outcome {
        Ok(data) => match data.reply(&request_id) {
            Ok(response) => Ok(response),
            Err(error) => error_reply(request_id, error),
        },
        Err(error) => error_reply(request_id, error),
    }
}
fn error_reply(request_id: String, error: WBError) -> Result<tauri::ipc::Response, WBError> {
    let reply = WorkbenchReply {
        ok: false,
        request_id,
        data: None,
        error: Some(error),
    };
    let serialized = serde_json::to_string(&reply).map_err(|_| WBError::new("WB_IO"))?;
    Ok(tauri::ipc::Response::new(
        tauri::ipc::InvokeResponseBody::Json(serialized),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tauri::ipc::IpcResponse;

    fn body(response: tauri::ipc::Response) -> String {
        match response.body().unwrap() {
            tauri::ipc::InvokeResponseBody::Json(value) => value,
            _ => panic!("typed reply must be JSON"),
        }
    }

    #[test]
    fn deferred_reply_bounds_full_envelope_and_finishes_only_after_encoding() {
        let expected = r#"{"ok":true,"requestId":"request","data":{"value":"한국어"}}"#;
        let completed = Arc::new(AtomicBool::new(false));
        let mut output = NativeOutput::typed(json!({"value":"한국어"}));
        output.max_bytes = expected.len();
        let finish = completed.clone();
        output.finish = Some(Box::new(move || {
            finish.store(true, Ordering::SeqCst);
            Ok(())
        }));
        assert_eq!(body(output.reply("request").unwrap()), expected);
        assert!(completed.load(Ordering::SeqCst));

        completed.store(false, Ordering::SeqCst);
        let mut output = NativeOutput::typed(json!({"value":"한국어"}));
        output.max_bytes = expected.len() - 1; // Body fits; closing full envelope does not.
        let finish = completed.clone();
        output.finish = Some(Box::new(move || {
            finish.store(true, Ordering::SeqCst);
            Ok(())
        }));
        assert_eq!(output.reply("request").err().unwrap().code, "WB_LIMIT");
        assert!(!completed.load(Ordering::SeqCst));
    }

    #[test]
    fn failed_or_oversized_deferred_writer_releases_guard_without_finishing() {
        struct Guard(Arc<AtomicBool>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        for oversized in [false, true] {
            let dropped = Arc::new(AtomicBool::new(false));
            let finished = Arc::new(AtomicBool::new(false));
            let watching = dropped.clone();
            let mut output = NativeOutput::projected(Guard(dropped.clone()), move |_, writer| {
                assert!(!watching.load(Ordering::SeqCst));
                if oversized {
                    writer
                        .write_all(&[b'x'; 1024])
                        .map_err(|_| WBError::new("WB_IO"))?;
                }
                Err(WBError::new("WB_IO"))
            });
            output.max_bytes = 128;
            let watching = dropped.clone();
            let finishing = finished.clone();
            output.finish = Some(Box::new(move || {
                assert!(!watching.load(Ordering::SeqCst));
                finishing.store(true, Ordering::SeqCst);
                Ok(())
            }));
            let error = output.reply("request").err().unwrap();
            assert_eq!(error.code, if oversized { "WB_LIMIT" } else { "WB_IO" });
            assert!(dropped.load(Ordering::SeqCst));
            assert!(!finished.load(Ordering::SeqCst));
        }
    }

    #[test]
    fn deferred_reply_holds_guard_through_finish_and_accepts_ordinary_one_mib_limit() {
        struct Guard(Arc<AtomicBool>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let mut output = NativeOutput::projected(Guard(dropped.clone()), |_, writer| {
            serde_json::to_writer(writer, &"x".repeat(600 * 1024))
                .map_err(|_| WBError::new("WB_IO"))
        });
        let watching = dropped.clone();
        output.finish = Some(Box::new(move || {
            assert!(!watching.load(Ordering::SeqCst));
            Ok(())
        }));
        let encoded = body(output.reply("request").unwrap());
        assert!(encoded.len() > 512 * 1024 && encoded.len() < 1024 * 1024);
        assert!(dropped.load(Ordering::SeqCst));
        assert_eq!(
            serde_json::from_str::<Value>(&encoded).unwrap()["data"]
                .as_str()
                .unwrap()
                .len(),
            600 * 1024
        );
    }

    #[test]
    fn deferred_response_keeps_global_pull_capacity_until_finish() {
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = slots.clone().try_acquire_owned().unwrap();
        let mut output = NativeOutput::typed(json!({"frames":[],"hasMore":false})).hold(permit);
        let verifying = slots.clone();
        output.finish = Some(Box::new(move || {
            assert!(verifying.clone().try_acquire_owned().is_err());
            Ok(())
        }));
        assert!(slots.clone().try_acquire_owned().is_err());
        body(output.reply("request").unwrap());
        assert!(slots.try_acquire_owned().is_ok());
    }

    #[test]
    fn saturated_data_requests_leave_bounded_ack_and_cleanup_capacity() {
        use super::*;
        let desktop = DesktopWorkbench::default();
        let data = (0..32)
            .map(|_| desktop.request_slot(WorkbenchCommand::QueryRun).unwrap())
            .collect::<Vec<_>>();
        assert!(desktop.request_slot(WorkbenchCommand::QueryRun).is_err());
        let control = [
            WorkbenchCommand::QueryCancel,
            WorkbenchCommand::TransactionRollback,
            WorkbenchCommand::SessionClose,
            WorkbenchCommand::OperationGet,
            WorkbenchCommand::OperationList,
            WorkbenchCommand::TransactionGet,
            WorkbenchCommand::SnapshotEnd,
            WorkbenchCommand::ExportCancel,
        ]
        .into_iter()
        .map(|command| desktop.request_slot(command).unwrap())
        .collect::<Vec<_>>();
        assert!(desktop
            .request_slot(WorkbenchCommand::OperationGet)
            .is_err());
        let acks = (0..8)
            .map(|_| desktop.request_slot(WorkbenchCommand::ResultAck).unwrap())
            .collect::<Vec<_>>();
        assert!(desktop.request_slot(WorkbenchCommand::ResultAck).is_err());
        let pulls = (0..8)
            .map(|_| {
                desktop
                    .request_slot(WorkbenchCommand::ResultFrames)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert!(desktop
            .request_slot(WorkbenchCommand::ResultFrames)
            .is_err());
        drop(pulls);
        assert!(desktop.request_slot(WorkbenchCommand::ResultFrames).is_ok());
        drop(acks);
        assert!(desktop.request_slot(WorkbenchCommand::ResultAck).is_ok());
        drop(control);
        assert!(desktop.request_slot(WorkbenchCommand::QueryCancel).is_ok());
        drop(data);
    }
    #[test]
    fn csp_permits_only_self_and_tauri_local_ipc_connections() {
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let csp = config["app"]["security"]["csp"].as_str().unwrap();
        let connect = csp
            .split(';')
            .find(|directive| directive.trim_start().starts_with("connect-src "))
            .unwrap();
        let sources: std::collections::BTreeSet<_> = connect.split_whitespace().skip(1).collect();
        assert_eq!(
            sources,
            std::collections::BTreeSet::from(["'self'", "ipc:", "http://ipc.localhost"])
        );
        for required in [
            "default-src 'self'",
            "style-src 'self' 'unsafe-inline'",
            "font-src 'self' data:",
            "img-src 'self' data:",
        ] {
            assert!(csp.contains(required));
        }
    }
}
