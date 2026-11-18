//! Native import reviews and bounded, independently committed child batches.
//! A source is parsed through EOF before admission; rows are never journalled.
use super::*;
use crate::driver::{DriverTransactionState, QueryLimits, RowSink, TransactionCommand};
use crate::generated_sql::{
    prepare_insert, validate_column_cell, FreshWriteGate, NativeInsertLimits, WriteCertification,
};
use crate::journal::{
    AdmissionSpec, ImportBatch, NativeContinuationGrant, OperationJournal, StatementSpec,
    StatementUpdate, Transition,
};
use crate::metadata::{ColumnMetadata, MetadataCatalog, RelationMetadata};
use crate::movement_codec::{decode_delimited_value, CodecLimits, DelimitedReader};
use crate::result_stream::encoded_size;
use crate::types::now_ms;
use async_trait::async_trait;
use restoreplane_domain::workbench::*;
use restoreplane_domain::workbench_metadata::{
    Completeness, Count, ObjectRef, Page, PageInput, Row,
};
use restoreplane_domain::workbench_movement::*;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

const PREVIEW_ROWS: usize = 100;
const PREVIEW_BYTES: usize = 512 * 1024;
const REVIEW_MS: u64 = 60_000;
const STAGING_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImportPreviewInput {
    pub session: SessionRef,
    pub object: ObjectRef,
    pub source_handle: Uuid,
    pub format: MovementFormat,
    pub mapping: BTreeMap<String, Uuid>,
    pub page: PageInput,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportPreviewReply {
    pub preview_id: Uuid,
    pub columns: Vec<Column>,
    pub rows: Page<Row>,
    pub warnings: Vec<String>,
    #[serde(skip)]
    _memory: MemoryReservation,
}
pub struct ImportStartError {
    pub error: WorkbenchError,
    pub operation_id: Option<Uuid>,
}
impl From<WorkbenchError> for ImportStartError {
    fn from(error: WorkbenchError) -> Self {
        Self {
            error,
            operation_id: None,
        }
    }
}
struct SourceRecord {
    end: u64,
    fields: Vec<SourceScalar>,
}
enum Records<'a> {
    Delimited(Box<DelimitedReader<&'a mut File>>),
    Json(JsonRecordReader<&'a mut File>),
}
struct SourceReader<'a> {
    records: Records<'a>,
    names: Vec<String>,
    first: Option<SourceRecord>,
}
impl<'a> SourceReader<'a> {
    fn new(file: &'a mut File, format: MovementFormat) -> Result<Self, WorkbenchError> {
        file.seek(SeekFrom::Start(0))
            .map_err(|_| WorkbenchError::Io)?;
        if format == MovementFormat::Json {
            let mut reader = JsonRecordReader::new(file)?;
            let first = reader.next_record()?;
            let names = if let Some(columns) = reader.columns() {
                columns.iter().map(|column| column.name.clone()).collect()
            } else if let Some(record) = &first {
                record.names.clone().unwrap_or_else(|| {
                    (0..record.fields.len())
                        .map(|index| format!("column_{}", index + 1))
                        .collect()
                })
            } else {
                return Err(WorkbenchError::InvalidInput);
            };
            Ok(Self {
                records: Records::Json(reader),
                names,
                first: first.map(|record| SourceRecord {
                    end: record.byte_end,
                    fields: record.fields,
                }),
            })
        } else {
            let mut reader = DelimitedReader::new(
                file,
                if format == MovementFormat::Csv {
                    b','
                } else {
                    b'\t'
                },
                CodecLimits::default(),
            )?;
            let header = reader.next_record()?.ok_or(WorkbenchError::InvalidInput)?;
            if header.fields.is_empty() || header.fields.iter().any(|field| field.len() > 64 * 1024)
            {
                return Err(WorkbenchError::InvalidInput);
            }
            Ok(Self {
                records: Records::Delimited(Box::new(reader)),
                names: header.fields,
                first: None,
            })
        }
    }
    fn next(&mut self) -> Result<Option<SourceRecord>, WorkbenchError> {
        if let Some(first) = self.first.take() {
            return Ok(Some(first));
        }
        let record = match &mut self.records {
            Records::Json(reader) => reader.next_record()?.map(|record| SourceRecord {
                end: record.byte_end,
                fields: record.fields,
            }),
            Records::Delimited(reader) => reader.next_record()?.map(|record| SourceRecord {
                end: record.byte_end,
                fields: record
                    .fields
                    .into_iter()
                    .map(|value| match decode_delimited_value(&value) {
                        None => SourceScalar::Null,
                        Some(value) => SourceScalar::Text(value.into()),
                    })
                    .collect(),
            }),
        };
        if record
            .as_ref()
            .is_some_and(|record| record.fields.len() != self.names.len())
        {
            return Err(WorkbenchError::InvalidInput);
        }
        if record.is_none() && matches!(&self.records,Records::Json(reader) if !reader.complete()) {
            return Err(WorkbenchError::InvalidInput);
        }
        Ok(record)
    }
}
struct Review {
    id: Uuid,
    context: InvocationContext,
    input: ImportPreviewInput,
    scope: Scope,
    revision: String,
    authority: String,
    relation: Arc<RelationMetadata>,
    mapping: Vec<MovementMapping>,
    basis: MovementTargetBasis,
    source_digest: String,
    source_bytes: u64,
    total_rows: u64,
    sample: Vec<Row>,
    warnings: Vec<String>,
    expires: u64,
    _memory: MemoryReservation,
}
impl Review {
    fn initial_metadata(&self) -> MovementMetadata {
        MovementMetadata::Import {
            signature_version: 1,
            source_digest: self.source_digest.clone(),
            format: self.input.format,
            object: self.relation.object.clone(),
            target_basis: self.basis.clone(),
            mapping: self.mapping.clone(),
            total_bytes: self.source_bytes.into(),
            total_rows: self.total_rows.into(),
            progress: MovementProgress {
                bytes_processed: 0.into(),
                total_bytes: Some(self.source_bytes.into()),
                rows_processed: 0.into(),
                confirmed_committed_rows: 0.into(),
                confirmed_committed_batches: 0.into(),
                unresolved_batches: 0.into(),
                next_batch_index: Some(0),
            },
            latest_continuation: None,
        }
    }
}
struct ImportJob {
    context: InvocationContext,
    scope: Scope,
    session: SessionRef,
    cancelled: AtomicBool,
    active_epoch: AtomicU64,
    continuation: Option<Arc<NativeContinuationGrant>>,
}
struct ActiveImportExecution {
    job: Arc<ImportJob>,
    epoch: u64,
}
impl Drop for ActiveImportExecution {
    fn drop(&mut self) {
        let _ = self.job.active_epoch.compare_exchange(
            self.epoch,
            0,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }
}
struct ContinueReview {
    id: Uuid,
    parent: Uuid,
    expected: u64,
    preview: Arc<Review>,
    next: NextUnsent,
    expires: u64,
}
pub struct ImportCoordinator {
    runtime: Arc<WorkbenchState>,
    metadata: Arc<MetadataCatalog>,
    files: Arc<FileCapabilities>,
    journal: Arc<OperationJournal>,
    certification: Option<WriteCertification>,
    reviews: Mutex<HashMap<Uuid, Arc<Review>>>,
    jobs: Mutex<HashMap<Uuid, Arc<ImportJob>>>,
    continuations: Mutex<HashMap<Uuid, Arc<ContinueReview>>>,
    slots: Arc<Semaphore>,
}
impl FileCapabilities {
    fn with_source<T>(
        &self,
        ctx: &InvocationContext,
        session: SessionRef,
        handle: Uuid,
        action: impl FnOnce(&mut HeldSource) -> Result<T, WorkbenchError>,
    ) -> Result<T, WorkbenchError> {
        let capability = self.source_capability(ctx, session, handle)?;
        let mut held = capability.value.try_lock().map_err(|error| match error {
            std::sync::TryLockError::WouldBlock => WorkbenchError::TargetBusy,
            std::sync::TryLockError::Poisoned(_) => WorkbenchError::Io,
        })?;
        let source = held.as_mut().ok_or(WorkbenchError::ResultExpired)?;
        if source.binding != capability.binding {
            return Err(WorkbenchError::Forbidden);
        }
        if source.in_use {
            return Err(WorkbenchError::TargetBusy);
        }
        let result = action(source);
        if source.revoked.load(Ordering::SeqCst) {
            return Err(WorkbenchError::ResultExpired);
        }
        if FileScope::current(&self.runtime, ctx, session)? != capability.binding {
            return Err(WorkbenchError::SessionStale);
        }
        result
    }
    pub(super) fn source_capability(
        &self,
        ctx: &InvocationContext,
        session: SessionRef,
        handle: Uuid,
    ) -> Result<Arc<ScopedSource>, WorkbenchError> {
        let binding = FileScope::current(&self.runtime, ctx, session)?;
        let capability = self
            .sources
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .get(&handle)
            .cloned()
            .ok_or(WorkbenchError::ResultExpired)?;
        if capability.binding != binding {
            return Err(WorkbenchError::Forbidden);
        }
        Ok(capability)
    }
    pub fn source_columns(
        &self,
        ctx: &InvocationContext,
        session: SessionRef,
        handle: Uuid,
        format: MovementFormat,
    ) -> Result<MovementSource, WorkbenchError> {
        self.with_source(ctx, session, handle, |source| {
            let reader = SourceReader::new(&mut source.selection.file, format)?;
            let columns = reader
                .names
                .iter()
                .enumerate()
                .map(|(ordinal, name)| MovementSourceColumn {
                    source_key: format!("c{ordinal}"),
                    ordinal: ordinal as u32,
                    name: name.clone(),
                    type_hint: None,
                })
                .collect();
            Ok(MovementSource {
                source_handle: handle,
                display_name: source.selection.display_name.clone(),
                byte_length: source.selection.length.into(),
                columns,
            })
        })
    }
    fn take_source(
        &self,
        ctx: &InvocationContext,
        session: SessionRef,
        handle: Uuid,
    ) -> Result<HeldSource, WorkbenchError> {
        let binding = FileScope::current(&self.runtime, ctx, session)?;
        let mut sources = self.sources.lock().map_err(|_| WorkbenchError::Io)?;
        let source = sources
            .get(&handle)
            .cloned()
            .ok_or(WorkbenchError::ResultExpired)?;
        if source.binding != binding {
            return Err(WorkbenchError::Forbidden);
        }
        let mut held = source.value.try_lock().map_err(|error| match error {
            std::sync::TryLockError::WouldBlock => WorkbenchError::TargetBusy,
            std::sync::TryLockError::Poisoned(_) => WorkbenchError::Io,
        })?;
        let owned = held.take().ok_or(WorkbenchError::ResultExpired)?;
        drop(held);
        sources.remove(&handle);
        Ok(owned)
    }
}
impl ImportCoordinator {
    pub fn new(
        runtime: Arc<WorkbenchState>,
        metadata: Arc<MetadataCatalog>,
        files: Arc<FileCapabilities>,
        journal: Arc<OperationJournal>,
        certification: Option<WriteCertification>,
    ) -> Self {
        Self {
            runtime,
            metadata,
            files,
            journal,
            certification,
            reviews: Mutex::new(HashMap::new()),
            jobs: Mutex::new(HashMap::new()),
            continuations: Mutex::new(HashMap::new()),
            slots: Arc::new(Semaphore::new(4)),
        }
    }
    pub async fn preview(
        self: &Arc<Self>,
        ctx: &InvocationContext,
        input: ImportPreviewInput,
    ) -> Result<ImportPreviewReply, WorkbenchError> {
        if input.mapping.is_empty()
            || input.mapping.len() > 512
            || input.page.limit == 0
            || input.page.limit > 100
        {
            return Err(WorkbenchError::InvalidInput);
        }
        if let Some(cursor) = &input.page.cursor {
            let (id, start) = cursor
                .split_once(':')
                .ok_or(WorkbenchError::CursorInvalid)?;
            let id = Uuid::parse_str(id).map_err(|_| WorkbenchError::CursorInvalid)?;
            let start = start
                .parse::<usize>()
                .map_err(|_| WorkbenchError::CursorInvalid)?;
            let review = self.review(ctx, input.session, id)?;
            if review.input.object != input.object
                || review.input.source_handle != input.source_handle
                || review.input.mapping != input.mapping
                || review.input.format != input.format
            {
                return Err(WorkbenchError::CursorInvalid);
            }
            return preview_page(&review, start, input.page.limit, self.metadata.budget());
        }
        let slot = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| WorkbenchError::Limit)?;
        let receipt = self
            .runtime
            .binding_receipt(ctx, input.session, AuthorizePurpose::Read)
            .await?;
        let relation = self
            .metadata
            .resolve_relation(ctx, input.session, &input.object)
            .await?;
        let facts = self
            .runtime
            .with_connection(ctx, input.session, AuthorizePurpose::Read, |connection| {
                Box::pin(async move { Ok(connection.facts().clone()) })
            })
            .await?;
        let mapping = resolve_mapping(&input.mapping, &relation)?;
        let mut digest = blake3::Hasher::new();
        for column in &relation.columns {
            let encoded = serde_json::to_vec(&(
                column.ordinal,
                &column.column.name,
                &column.column.type_name,
                column.column.nullable,
                column.generated,
                column.defaulted,
                &column.declared_type,
                &column.type_detail,
            ))
            .map_err(|_| WorkbenchError::InvalidInput)?;
            digest.update(&(encoded.len() as u64).to_be_bytes());
            digest.update(&encoded);
        }
        let basis = MovementTargetBasis {
            schema_name: relation.schema.clone(),
            table_name: relation.name.clone(),
            schema_version: relation.object.schema_version.clone(),
            column_basis_digest: digest.finalize().to_hex().to_string(),
            server_identity: facts.server_identity,
            actual_database_identity: facts.actual_database_identity,
        };
        let this = self.clone();
        let native_ctx = ctx.clone();
        let native_input = input.clone();
        let native_relation = relation.clone();
        let native_mapping = mapping.clone();
        let staging = self.metadata.budget().reserve(STAGING_BYTES)?;
        let (source_digest, source_bytes, total_rows, sample) =
            tokio::task::spawn_blocking(move || {
                let _slot = slot;
                let _staging = staging;
                this.files.with_source(
                    &native_ctx,
                    native_input.session,
                    native_input.source_handle,
                    |source| {
                        if source.digest.is_empty() {
                            source
                                .selection
                                .file
                                .seek(SeekFrom::Start(0))
                                .map_err(|_| WorkbenchError::Io)?;
                            let mut hash = blake3::Hasher::new();
                            let mut buffer = [0u8; 16 * 1024];
                            let mut length = 0u64;
                            loop {
                                let count = source
                                    .selection
                                    .file
                                    .read(&mut buffer)
                                    .map_err(|_| WorkbenchError::Io)?;
                                if count == 0 {
                                    break;
                                }
                                hash.update(&buffer[..count]);
                                length = length
                                    .checked_add(count as u64)
                                    .ok_or(WorkbenchError::Limit)?;
                                if length.is_multiple_of(1024 * 1024) {
                                    if source.revoked.load(Ordering::SeqCst) {
                                        return Err(WorkbenchError::ResultExpired);
                                    }
                                    FileScope::current(
                                        &this.runtime,
                                        &native_ctx,
                                        native_input.session,
                                    )?;
                                }
                            }
                            if length != source.selection.length
                                || restoreplane_source_sqlite::opened_file_object_id(
                                    &source.selection.file,
                                )
                                .map_err(|_| WorkbenchError::Io)?
                                    != source.selection.file_identity
                            {
                                return Err(WorkbenchError::IdempotencyConflict);
                            }
                            source.digest = hash.finalize().to_hex().to_string();
                        }
                        let mut reader =
                            SourceReader::new(&mut source.selection.file, native_input.format)?;
                        if native_mapping
                            .iter()
                            .any(|item| item.source_ordinal as usize >= reader.names.len())
                        {
                            return Err(WorkbenchError::InvalidInput);
                        }
                        let mut rows = 0u64;
                        let mut sample = Vec::new();
                        let mut sample_bytes = 0usize;
                        while let Some(record) = reader.next()? {
                            let values =
                                map_record(&record.fields, &native_mapping, &native_relation)?;
                            let destination = native_mapping
                                .iter()
                                .map(|item| item.column_id)
                                .collect::<Vec<_>>();
                            let _validated = prepare_insert(
                                native_relation.clone(),
                                &destination,
                                std::slice::from_ref(&values),
                                NativeInsertLimits::importing(),
                            )?;
                            rows = rows.checked_add(1).ok_or(WorkbenchError::Limit)?;
                            let size = encoded_size(&values, 256 * 1024)?;
                            if sample.len() < PREVIEW_ROWS
                                && sample_bytes.saturating_add(size) < PREVIEW_BYTES - 16 * 1024
                            {
                                sample.push(Row {
                                    row_handle: Uuid::new_v4(),
                                    cells: values,
                                });
                                if crate::types::rows_memory(&sample, sample.capacity())
                                    > PREVIEW_BYTES
                                {
                                    sample.pop();
                                } else {
                                    sample_bytes += size;
                                }
                            }
                            if rows.is_multiple_of(64) {
                                if source.revoked.load(Ordering::SeqCst) {
                                    return Err(WorkbenchError::ResultExpired);
                                }
                                FileScope::current(
                                    &this.runtime,
                                    &native_ctx,
                                    native_input.session,
                                )?;
                            }
                        }
                        // Typed wrappers must have complete:true and exact EOF. The
                        // full scan occurs before any operation or child is admitted.
                        if rows == 0 {
                            return Err(WorkbenchError::InvalidInput);
                        }
                        Ok((source.digest.clone(), source.selection.length, rows, sample))
                    },
                )
            })
            .await
            .map_err(|_| WorkbenchError::Io)??;
        let memory = self.metadata.budget().reserve(1024 * 1024)?;
        let review = Arc::new(Review {
            id: Uuid::new_v4(),
            context: ctx.clone(),
            input: input.clone(),
            scope: receipt.scope,
            revision: receipt.descriptor_revision,
            authority: receipt.authority_epoch,
            relation,
            mapping,
            basis,
            source_digest,
            source_bytes,
            total_rows,
            sample,
            warnings: vec![
                "import_independent_batches".into(),
                "import_partial_commits".into(),
            ],
            expires: now_ms().saturating_add(REVIEW_MS),
            _memory: memory,
        });
        let mut reviews = self.reviews.lock().map_err(|_| WorkbenchError::Io)?;
        reviews.retain(|_, review| review.expires > now_ms());
        if reviews.len() >= 16 {
            return Err(WorkbenchError::Limit);
        }
        reviews.insert(review.id, review.clone());
        drop(reviews);
        preview_page(&review, 0, input.page.limit, self.metadata.budget())
    }
    fn review(
        &self,
        ctx: &InvocationContext,
        session: SessionRef,
        id: Uuid,
    ) -> Result<Arc<Review>, WorkbenchError> {
        ctx.principal().ensure_standard()?;
        let review = self
            .reviews
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .get(&id)
            .cloned()
            .ok_or(WorkbenchError::AttemptExpired)?;
        if review.context.principal() != ctx.principal()
            || review.context.window_instance_id() != ctx.window_instance_id()
            || review.input.session != session
        {
            return Err(WorkbenchError::Forbidden);
        }
        if review.expires <= now_ms() {
            return Err(WorkbenchError::AttemptExpired);
        }
        let current = self.runtime.get(ctx, session)?;
        if current.scope != review.scope || current.descriptor_revision != review.revision {
            return Err(WorkbenchError::BindingStale);
        }
        Ok(review)
    }
    fn resolved(review: &Review, input: &ImportStartInput) -> Value {
        json!({"input":input,"sourceDigest":review.source_digest,"sourceBytes":review.source_bytes.to_string(),"totalRows":review.total_rows.to_string(),"mapping":review.mapping,"targetBasis":review.basis,"descriptorRevision":review.revision,"authorityEpoch":review.authority})
    }
    pub async fn prepare_start(
        &self,
        ctx: &InvocationContext,
        input: &ImportStartInput,
    ) -> Result<PreparedAttempt, WorkbenchError> {
        let review = self.review(ctx, input.session, input.preview_id)?;
        validate_warnings(&review, input)?;
        let receipt = self
            .runtime
            .binding_receipt(ctx, input.session, AuthorizePurpose::Mutate)
            .await?;
        same_basis(&review, &receipt)?;
        let payload = Self::resolved(&review, input);
        let public = serde_json::to_value(input).map_err(|_| WorkbenchError::InvalidInput)?;
        let context = ctx.clone();
        let session = input.session;
        self.jcall(move |journal| {
            journal.prepare_import_attempt(&context, &receipt, session, &payload, &public)
        })
        .await
    }
    pub async fn start(
        self: &Arc<Self>,
        ctx: &InvocationContext,
        token: &str,
        input: ImportStartInput,
    ) -> Result<Admission, ImportStartError> {
        let public = serde_json::to_value(&input).map_err(|_| WorkbenchError::InvalidInput)?;
        let native_ctx = ctx.clone();
        let native_token = token.to_string();
        let session = input.session;
        if let Some(admission) = self
            .jcall(move |journal| {
                journal.import_start_status(&native_ctx, &native_token, &public, session)
            })
            .await?
        {
            return Ok(admission);
        }
        let review = self.review(ctx, input.session, input.preview_id)?;
        validate_warnings(&review, &input)?;
        if self.certification.is_none() {
            return Err(WorkbenchError::Unsupported.into());
        }
        let slot = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| WorkbenchError::Limit)?;
        let receipt = self
            .runtime
            .binding_receipt(ctx, input.session, AuthorizePurpose::Mutate)
            .await?;
        same_basis(&review, &receipt)?;
        let payload = Self::resolved(&review, &input);
        let native_ctx = ctx.clone();
        let native_receipt = receipt.clone();
        let token = token.to_string();
        let session = input.session;
        let credential = self.runtime.credential_reference(ctx, session)?;
        let admission = self
            .jcall(move |journal| {
                journal.admit(
                    &native_ctx,
                    &token,
                    "import.start",
                    &payload,
                    &native_receipt,
                    Some(credential),
                    AdmissionSpec {
                        kind: OperationKind::ImportJob,
                        session: Some(session),
                        transaction_id: None,
                        statements: Vec::new(),
                        batch: None,
                    },
                )
            })
            .await?;
        if admission.deduplicated {
            return Ok(admission);
        }
        let id = admission.operation_id;
        let source = match self
            .files
            .take_source(ctx, session, review.input.source_handle)
        {
            Ok(source) => source,
            Err(error) => {
                let settlement = self.settle_unsent_failure(ctx, &review.scope, id).await;
                return Err(ImportStartError {
                    error: settlement.err().unwrap_or(error),
                    operation_id: Some(id),
                });
            }
        };
        let metadata = MovementMetadata::Import {
            signature_version: 1,
            source_digest: review.source_digest.clone(),
            format: review.input.format,
            object: review.relation.object.clone(),
            target_basis: review.basis.clone(),
            mapping: review.mapping.clone(),
            total_bytes: review.source_bytes.into(),
            total_rows: review.total_rows.into(),
            latest_continuation: None,
            progress: MovementProgress {
                bytes_processed: 0.into(),
                total_bytes: Some(review.source_bytes.into()),
                rows_processed: 0.into(),
                confirmed_committed_rows: 0.into(),
                confirmed_committed_batches: 0.into(),
                unresolved_batches: 0.into(),
                next_batch_index: Some(0),
            },
        };
        let native_ctx = ctx.clone();
        let scope = review.scope.clone();
        let epoch = match self.runtime.get(ctx, session) {
            Ok(session) => session.execution_epoch.value(),
            Err(error) => {
                let settlement = self.settle_unsent_failure(ctx, &review.scope, id).await;
                return Err(ImportStartError {
                    error: settlement.err().unwrap_or(error),
                    operation_id: Some(id),
                });
            }
        };
        let version = admission.version.value();
        let initialized = self
            .jcall(move |journal| {
                let old =
                    journal.update_movement(&native_ctx, &scope, id, version, metadata, None)?;
                let permit = journal.mark_movement_dispatch(
                    &native_ctx,
                    &scope,
                    id,
                    old.version.value(),
                    epoch,
                    &receipt,
                )?;
                journal.transition(
                    &native_ctx,
                    &scope,
                    id,
                    permit.version(),
                    Transition::Running,
                )
            })
            .await;
        let operation = match initialized {
            Ok(operation) => operation,
            Err(error) => {
                let settlement = self.settle_unsent_failure(ctx, &review.scope, id).await;
                return Err(ImportStartError {
                    error: settlement.err().unwrap_or(error),
                    operation_id: Some(id),
                });
            }
        };
        let job = Arc::new(ImportJob {
            context: ctx.clone(),
            scope: review.scope.clone(),
            session,
            cancelled: AtomicBool::new(false),
            active_epoch: AtomicU64::new(0),
            continuation: None,
        });
        self.jobs
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .insert(id, job.clone());
        let version = operation.version;
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let _slot = slot;
            let handle = tokio::runtime::Handle::current();
            let completing = job.clone();
            handle.block_on(this.run_job(job, review, source, operation));
            let _ = this.jobs.lock().map(|mut jobs| {
                if jobs
                    .get(&id)
                    .is_some_and(|job| Arc::ptr_eq(job, &completing))
                {
                    jobs.remove(&id);
                }
            });
        });
        Ok(Admission {
            operation_id: id,
            version,
            deduplicated: false,
        })
    }
    pub fn cancel(&self, ctx: &InvocationContext, id: Uuid) -> Result<bool, WorkbenchError> {
        let jobs = self.jobs.lock().map_err(|_| WorkbenchError::Io)?;
        let Some(job) = jobs.get(&id) else {
            return Ok(false);
        };
        if job.context.principal() != ctx.principal()
            || job.context.window_instance_id() != ctx.window_instance_id()
        {
            return Err(WorkbenchError::Forbidden);
        }
        self.signal_cancel(job);
        Ok(true)
    }
    pub async fn continue_review(
        &self,
        ctx: &InvocationContext,
        session: SessionRef,
        parent: Uuid,
        preview: Uuid,
    ) -> Result<ImportContinuationReview, WorkbenchError> {
        let review = self.review(ctx, session, preview)?;
        let receipt = self
            .runtime
            .binding_receipt(ctx, session, AuthorizePurpose::Mutate)
            .await?;
        same_basis(&review, &receipt)?;
        let native_ctx = ctx.clone();
        let scope = review.scope.clone();
        let (operation, metadata, next, status) = self
            .jcall(move |journal| {
                Ok((
                    journal.get(&native_ctx, &scope, parent)?,
                    journal
                        .movement_metadata(&native_ctx, &scope, parent)?
                        .ok_or(WorkbenchError::StoreUnavailable)?,
                    journal.import_next_unsent(&native_ctx, &scope, parent)?,
                    journal.movement_status(
                        &native_ctx,
                        &scope,
                        parent,
                        PageInput {
                            cursor: None,
                            limit: 1,
                        },
                    )?,
                ))
            })
            .await?;
        if !crate::journal::continuation_basis_matches(&metadata, &review.initial_metadata()) {
            return Err(WorkbenchError::Unsupported);
        }
        if status.progress.unresolved_batches.value() != 0 {
            return Err(WorkbenchError::OutcomeUnknown);
        }
        let next = next.ok_or(WorkbenchError::InvalidInput)?;
        let cached = Arc::new(ContinueReview {
            id: Uuid::new_v4(),
            parent,
            expected: operation.version.value(),
            preview: review.clone(),
            next: next.clone(),
            expires: review.expires.min(now_ms().saturating_add(REVIEW_MS)),
        });
        let mut continuations = self.continuations.lock().map_err(|_| WorkbenchError::Io)?;
        continuations.retain(|_, review| review.expires > now_ms());
        if continuations.len() >= 16 {
            return Err(WorkbenchError::Limit);
        }
        continuations.insert(cached.id, cached.clone());
        Ok(ImportContinuationReview {
            review_id: cached.id,
            parent_operation_id: parent,
            source_digest: review.source_digest.clone(),
            next_unsent: Some(next),
            confirmed_committed_rows: status.progress.confirmed_committed_rows,
            unresolved_batches: status.progress.unresolved_batches,
            warnings: vec![
                "import_independent_batches".into(),
                "import_continuation_unsent_only".into(),
            ],
            expires_at_ms: cached.expires.into(),
        })
    }
    fn continued_review(
        &self,
        ctx: &InvocationContext,
        input: &ImportContinueInput,
    ) -> Result<Arc<ContinueReview>, WorkbenchError> {
        let continued = self
            .continuations
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .get(&input.review_id)
            .cloned()
            .ok_or(WorkbenchError::AttemptExpired)?;
        if continued.parent != input.parent_operation_id {
            return Err(WorkbenchError::IdempotencyConflict);
        }
        if continued.expires <= now_ms() {
            return Err(WorkbenchError::AttemptExpired);
        }
        let current = self.review(ctx, input.session, continued.preview.id)?;
        if !Arc::ptr_eq(&current, &continued.preview) {
            return Err(WorkbenchError::AttemptExpired);
        }
        Ok(continued)
    }
    fn continuation_resolved(review: &ContinueReview, input: &ImportContinueInput) -> Value {
        json!({"input":input,"previewId":review.preview.id,"sourceDigest":review.preview.source_digest,"mapping":review.preview.mapping,"targetBasis":review.preview.basis,"nextUnsent":review.next,"expectedParentVersion":review.expected.to_string(),"descriptorRevision":review.preview.revision,"authorityEpoch":review.preview.authority})
    }
    pub async fn prepare_continue(
        &self,
        ctx: &InvocationContext,
        input: &ImportContinueInput,
    ) -> Result<PreparedAttempt, WorkbenchError> {
        let continued = self.continued_review(ctx, input)?;
        let receipt = self
            .runtime
            .binding_receipt(ctx, input.session, AuthorizePurpose::Mutate)
            .await?;
        same_basis(&continued.preview, &receipt)?;
        let payload = Self::continuation_resolved(&continued, input);
        let context = ctx.clone();
        let session = input.session;
        let parent = input.parent_operation_id;
        self.jcall(move |journal| {
            journal.prepare_continuation_attempt(&context, &receipt, session, parent, &payload)
        })
        .await
    }
    pub async fn continue_import(
        self: &Arc<Self>,
        ctx: &InvocationContext,
        token: &str,
        input: ImportContinueInput,
    ) -> Result<Admission, ImportStartError> {
        let context = ctx.clone();
        let public = serde_json::to_value(&input).map_err(|_| WorkbenchError::InvalidInput)?;
        let lookup_public = public.clone();
        let lookup_token = token.to_string();
        let session = input.session;
        if let Some(admission) = self
            .jcall(move |journal| {
                journal.continuation_status(&context, &lookup_token, &lookup_public, session)
            })
            .await?
        {
            return Ok(admission);
        }
        let continued = self.continued_review(ctx, &input)?;
        if self.certification.is_none() {
            return Err(WorkbenchError::Unsupported.into());
        }
        let slot = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| WorkbenchError::Limit)?;
        let receipt = self
            .runtime
            .binding_receipt(ctx, session, AuthorizePurpose::Mutate)
            .await?;
        same_basis(&continued.preview, &receipt)?;
        let source = self
            .files
            .take_source(ctx, session, continued.preview.input.source_handle)?;
        let context = ctx.clone();
        let token = token.to_string();
        let resolved = Self::continuation_resolved(&continued, &input);
        let basis = continued.preview.initial_metadata();
        let expected = continued.expected;
        let (admission, grant) = self
            .jcall(move |journal| {
                journal.admit_continuation(
                    &context, &token, &public, &resolved, &receipt, session, expected, &basis,
                )
            })
            .await?;
        let Some(grant) = grant else {
            return Ok(admission);
        };
        let id = admission.operation_id;
        let operation = self
            .current(ctx, &continued.preview.scope, id)
            .await
            .map_err(|error| ImportStartError {
                error,
                operation_id: Some(id),
            })?;
        let job = Arc::new(ImportJob {
            context: ctx.clone(),
            scope: continued.preview.scope.clone(),
            session,
            cancelled: AtomicBool::new(false),
            active_epoch: AtomicU64::new(0),
            continuation: Some(grant),
        });
        self.jobs
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .insert(id, job.clone());
        let this = self.clone();
        let review = continued.preview.clone();
        tokio::task::spawn_blocking(move || {
            let _slot = slot;
            let completing = job.clone();
            tokio::runtime::Handle::current()
                .block_on(this.run_job(job, review, source, operation));
            let _ = this.jobs.lock().map(|mut jobs| {
                if jobs
                    .get(&id)
                    .is_some_and(|job| Arc::ptr_eq(job, &completing))
                {
                    jobs.remove(&id);
                }
            });
        });
        Ok(admission)
    }
    pub fn retire_window(&self, ctx: &InvocationContext) -> Result<(), WorkbenchError> {
        for job in self.jobs.lock().map_err(|_| WorkbenchError::Io)?.values() {
            if job.context.principal() == ctx.principal()
                && job.context.window_instance_id() == ctx.window_instance_id()
            {
                self.signal_cancel(job)
            }
        }
        self.reviews
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .retain(|_, review| {
                review.context.principal() != ctx.principal()
                    || review.context.window_instance_id() != ctx.window_instance_id()
            });
        Ok(())
    }
    fn signal_cancel(&self, job: &Arc<ImportJob>) {
        if !job.cancelled.swap(true, Ordering::SeqCst) {
            let epoch = job.active_epoch.load(Ordering::SeqCst);
            if epoch != 0 {
                let runtime = self.runtime.clone();
                let ctx = job.context.clone();
                let session = job.session;
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    handle.spawn(async move {
                        let _ = runtime.request_cancel(&ctx, session, epoch).await;
                    });
                }
            }
        }
    }
    fn active_execution(
        &self,
        job: &Arc<ImportJob>,
    ) -> Result<ActiveImportExecution, WorkbenchError> {
        let epoch = self
            .runtime
            .get(&job.context, job.session)?
            .execution_epoch
            .value();
        job.active_epoch.store(epoch, Ordering::SeqCst);
        Ok(ActiveImportExecution {
            job: job.clone(),
            epoch,
        })
    }
    pub fn expire_memory(&self) -> Result<(), WorkbenchError> {
        let now = now_ms();
        self.continuations
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .retain(|_, review| review.expires > now);
        self.reviews
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .retain(|_, review| review.expires > now);
        Ok(())
    }
    pub fn retire_session(
        &self,
        ctx: &InvocationContext,
        session: SessionRef,
    ) -> Result<(), WorkbenchError> {
        for job in self.jobs.lock().map_err(|_| WorkbenchError::Io)?.values() {
            if job.context.principal() == ctx.principal()
                && job.context.window_instance_id() == ctx.window_instance_id()
                && job.session == session
            {
                self.signal_cancel(job)
            }
        }
        self.continuations
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .retain(|_, review| {
                review.preview.input.session != session
                    || review.preview.context.principal() != ctx.principal()
                    || review.preview.context.window_instance_id() != ctx.window_instance_id()
            });
        self.reviews
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .retain(|_, review| {
                review.input.session != session
                    || review.context.principal() != ctx.principal()
                    || review.context.window_instance_id() != ctx.window_instance_id()
            });
        Ok(())
    }
    async fn run_job(
        self: &Arc<Self>,
        job: Arc<ImportJob>,
        review: Arc<Review>,
        mut source: HeldSource,
        mut parent: Operation,
    ) {
        let _staging = match self.metadata.budget().reserve(STAGING_BYTES) {
            Ok(memory) => memory,
            Err(_) => {
                self.finish_parent(&job, parent.operation_id, Transition::ReadInterrupted)
                    .await;
                return;
            }
        };
        let mut reader = match SourceReader::new(&mut source.selection.file, review.input.format) {
            Ok(reader) => reader,
            Err(_) => {
                self.finish_parent(&job, parent.operation_id, Transition::ReadInterrupted)
                    .await;
                return;
            }
        };
        let mut pending = None;
        let mut ended = false;
        let next = job
            .continuation
            .as_ref()
            .map(|grant| grant.next())
            .unwrap_or(NextUnsent {
                batch_index: 0,
                byte_start: 0.into(),
                row_start: 0.into(),
            });
        let mut byte_start = next.byte_start.value();
        let mut row_start = next.row_start.value();
        let mut batch_index = next.batch_index;
        for skipped in 0..row_start {
            match reader.next() {
                Ok(Some(record)) if skipped + 1 < row_start || record.end == byte_start => {}
                _ => {
                    self.finish_parent(&job, parent.operation_id, Transition::Partial)
                        .await;
                    return;
                }
            }
        }
        loop {
            if job.cancelled.load(Ordering::SeqCst) {
                self.finish_parent(
                    &job,
                    parent.operation_id,
                    if row_start == 0 {
                        Transition::ReadInterrupted
                    } else {
                        Transition::Partial
                    },
                )
                .await;
                return;
            }
            let mut rows = Vec::new();
            let mut bytes = 0usize;
            let mut byte_end = byte_start;
            while rows.len() < 200
                && rows
                    .len()
                    .saturating_add(1)
                    .saturating_mul(review.mapping.len())
                    <= 16384
            {
                let next = if let Some(row) = pending.take() {
                    Some(row)
                } else {
                    match reader.next() {
                        Ok(Some(record)) => {
                            match map_record(&record.fields, &review.mapping, &review.relation) {
                                Ok(row) => Some((row, record.end)),
                                Err(_) => {
                                    self.finish_parent(
                                        &job,
                                        parent.operation_id,
                                        Transition::Partial,
                                    )
                                    .await;
                                    return;
                                }
                            }
                        }
                        Ok(None) => {
                            ended = true;
                            None
                        }
                        Err(_) => {
                            self.finish_parent(&job, parent.operation_id, Transition::Partial)
                                .await;
                            return;
                        }
                    }
                };
                let Some((row, end)) = next else { break };
                let size = match encoded_size(&row, 256 * 1024) {
                    Ok(size) => size,
                    Err(_) => {
                        self.finish_parent(&job, parent.operation_id, Transition::Partial)
                            .await;
                        return;
                    }
                };
                let row_memory = crate::types::cells_memory(&row, row.capacity());
                let current_memory = rows
                    .iter()
                    .map(|row: &Vec<TypedCell>| crate::types::cells_memory(row, row.capacity()))
                    .sum::<usize>();
                if (bytes.saturating_add(size) > 256 * 1024
                    || current_memory.saturating_add(row_memory) > 256 * 1024)
                    && !rows.is_empty()
                {
                    pending = Some((row, end));
                    break;
                }
                if row_memory > 256 * 1024 {
                    self.finish_parent(&job, parent.operation_id, Transition::Partial)
                        .await;
                    return;
                }
                bytes += size;
                byte_end = end;
                rows.push(row);
            }
            if rows.is_empty() {
                if ended && row_start == review.total_rows {
                    let ctx = job.context.clone();
                    let scope = job.scope.clone();
                    let id = parent.operation_id;
                    let _ = self
                        .jcall(move |journal| {
                            let old = journal.get(&ctx, &scope, id)?;
                            let mut metadata = journal
                                .movement_metadata(&ctx, &scope, id)?
                                .ok_or(WorkbenchError::StoreUnavailable)?;
                            if let MovementMetadata::Import { progress, .. } = &mut metadata {
                                progress.bytes_processed = review.source_bytes.into();
                                progress.next_batch_index = None;
                            }
                            journal.update_movement(
                                &ctx,
                                &scope,
                                id,
                                old.version.value(),
                                metadata,
                                Some(Transition::JobCompleted),
                            )
                        })
                        .await;
                } else {
                    self.finish_parent(&job, parent.operation_id, Transition::Partial)
                        .await;
                }
                return;
            }
            if ended {
                byte_end = review.source_bytes;
            }
            let row_end = match row_start.checked_add(rows.len() as u64) {
                Some(end) => end,
                None => {
                    self.finish_parent(&job, parent.operation_id, Transition::Partial)
                        .await;
                    return;
                }
            };
            let child = self
                .execute_batch(
                    &job,
                    &review,
                    parent.operation_id,
                    batch_index,
                    byte_start,
                    byte_end,
                    row_start,
                    row_end,
                    rows,
                )
                .await;
            match child {
                Ok(child) if child.state == OperationState::Committed => {
                    row_start = row_end;
                    byte_start = byte_end;
                    batch_index = match batch_index.checked_add(1) {
                        Some(index) => index,
                        None => {
                            self.finish_parent(&job, parent.operation_id, Transition::Partial)
                                .await;
                            return;
                        }
                    };
                    let ctx = job.context.clone();
                    let scope = job.scope.clone();
                    let id = parent.operation_id;
                    let updated = self
                        .jcall(move |journal| {
                            let old = journal.get(&ctx, &scope, id)?;
                            let mut metadata = journal
                                .movement_metadata(&ctx, &scope, id)?
                                .ok_or(WorkbenchError::StoreUnavailable)?;
                            if let MovementMetadata::Import { progress, .. } = &mut metadata {
                                progress.bytes_processed = byte_end.into();
                                progress.rows_processed = row_end.into();
                                progress.confirmed_committed_rows = row_end.into();
                                progress.confirmed_committed_batches =
                                    u64::from(batch_index).into();
                                progress.next_batch_index = Some(batch_index);
                            }
                            journal.update_movement(
                                &ctx,
                                &scope,
                                id,
                                old.version.value(),
                                metadata,
                                None,
                            )
                        })
                        .await;
                    match updated {
                        Ok(updated) => parent = updated,
                        Err(_) => return,
                    }
                }
                Ok(child) if child.state == OperationState::OutcomeUnknown => {
                    self.finish_parent(&job, parent.operation_id, Transition::EffectsUnknown)
                        .await;
                    return;
                }
                Ok(_) => {
                    self.finish_parent(&job, parent.operation_id, Transition::Partial)
                        .await;
                    return;
                }
                Err(_) => {
                    self.finish_parent(&job, parent.operation_id, Transition::Partial)
                        .await;
                    return;
                }
            }
        }
    }
    #[allow(
        clippy::too_many_arguments,
        reason = "native child range is part of the exact durable import digest"
    )]
    async fn execute_batch(
        self: &Arc<Self>,
        job: &Arc<ImportJob>,
        review: &Arc<Review>,
        parent: Uuid,
        index: u32,
        byte_start: u64,
        byte_end: u64,
        row_start: u64,
        row_end: u64,
        rows: Vec<Vec<TypedCell>>,
    ) -> Result<Operation, WorkbenchError> {
        let certification = self
            .certification
            .as_ref()
            .ok_or(WorkbenchError::Unsupported)?;
        let gate = FreshWriteGate::acquire(
            self.metadata.clone(),
            &job.context,
            job.session,
            &review.relation.object,
            certification,
        )
        .await?;
        let destination = review
            .mapping
            .iter()
            .map(|mapping| mapping.column_id)
            .collect::<Vec<_>>();
        let generated = match prepare_insert(
            gate.relation().clone(),
            &destination,
            &rows,
            NativeInsertLimits::importing(),
        ) {
            Ok(generated) => generated,
            Err(error) => {
                let _ = gate.finish(true).await;
                return Err(error);
            }
        };
        let batch_digest = crate::journal::payload_digest(
            "native.import.batch",
            &json!({"rows":rows,"schemaVersion":review.basis.schema_version,"mapping":review.mapping,"index":index,"byteStart":byte_start.to_string(),"byteEnd":byte_end.to_string(),"rowStart":row_start.to_string(),"rowEnd":row_end.to_string()}),
        )?;
        drop(rows);
        let batch = ImportBatch {
            parent_operation_id: parent,
            batch_index: index,
            source_digest: review.source_digest.clone(),
            batch_digest: batch_digest.clone(),
            byte_start,
            byte_end,
            row_start,
            row_end,
        };
        let payload = json!({"parentOperationId":parent,"batchIndex":index,"sourceDigest":review.source_digest,"batchDigest":batch_digest,"byteStart":byte_start.to_string(),"byteEnd":byte_end.to_string(),"rowStart":row_start.to_string(),"rowEnd":row_end.to_string()});
        let ctx = job.context.clone();
        let receipt = gate.receipt().clone();
        let credential = self.runtime.credential_reference(&ctx, job.session)?;
        let session = job.session;
        let admitted = self
            .jcall(move |journal| {
                let attempt =
                    journal.prepare(&ctx, &receipt, Some(session), "import.continue", &payload)?;
                journal.admit(
                    &ctx,
                    &attempt.attempt_token,
                    "import.continue",
                    &payload,
                    &receipt,
                    Some(credential),
                    AdmissionSpec {
                        kind: OperationKind::ImportBatch,
                        session: Some(session),
                        transaction_id: None,
                        statements: vec![StatementSpec {
                            digest: batch_digest,
                            effect_class: EffectClass::Write,
                            certified_read_only: false,
                            transaction_id: None,
                        }],
                        batch: Some(batch),
                    },
                )
            })
            .await;
        let admission = match admitted {
            Ok(admission) => admission,
            Err(error) => {
                let _ = gate.finish(true).await;
                return Err(error);
            }
        };
        if admission.deduplicated {
            let _ = gate.finish(true).await;
            return self
                .current(&job.context, &job.scope, admission.operation_id)
                .await;
        }
        let id = admission.operation_id;
        let ctx = job.context.clone();
        let scope = job.scope.clone();
        let receipt = gate.receipt().clone();
        let epoch = self.runtime.get(&ctx, job.session)?.execution_epoch.value();
        let running = self
            .jcall(move |journal| {
                let permit = journal.mark_dispatch(
                    &ctx,
                    &scope,
                    id,
                    admission.version.value(),
                    epoch,
                    &receipt,
                )?;
                journal.transition(&ctx, &scope, id, permit.version(), Transition::Running)
            })
            .await;
        let operation = match running {
            Ok(operation) => operation,
            Err(error) => {
                let _ = gate.finish(true).await;
                return Err(error);
            }
        };
        if let Err(error) = gate.begin_transaction().await {
            self.unknown_child(job, id).await;
            let _ = gate.finish(false).await;
            return Err(error);
        }
        if job.cancelled.load(Ordering::SeqCst) {
            return self.rollback_child(job, &gate, id).await;
        }
        let ctx = job.context.clone();
        let scope = job.scope.clone();
        let operation = self
            .jcall(move |journal| {
                journal.record_statement(
                    &ctx,
                    &scope,
                    id,
                    operation.version.value(),
                    StatementUpdate {
                        ordinal: 0,
                        expected_version: 1,
                        outcome: StatementStatus::Sent,
                        effect_evidence: EffectEvidence::None,
                        rows_matched: None,
                        rows_changed: None,
                        error_code: None,
                    },
                )
            })
            .await?;
        let native_gate = gate.clone();
        let executing_owner = self.clone();
        let executing_job = job.clone();
        let expected_rows = row_end - row_start;
        let executed = gate
            .with_connection(move |connection| {
                Box::pin(async move {
                    let _execution = executing_owner.active_execution(&executing_job)?;
                    let mut sink = Discard;
                    generated
                        .execute(
                            connection,
                            &native_gate,
                            QueryLimits {
                                max_rows: 1,
                                max_bytes: 256 * 1024,
                                timeout: Duration::from_secs(30),
                            },
                            &mut sink,
                        )
                        .await
                })
            })
            .await;
        let summary = match executed {
            Ok(summary)
                if summary.transaction_state == DriverTransactionState::Active
                    && !summary.capped
                    && summary.rows_affected == Some(expected_rows) =>
            {
                summary
            }
            Ok(summary)
                if matches!(
                    summary.transaction_state,
                    DriverTransactionState::Active | DriverTransactionState::Failed
                ) =>
            {
                return self
                    .reject_child(job, &gate, operation, summary.transaction_state)
                    .await
            }
            Err(WorkbenchError::StatementRejected(state))
                if state != DriverTransactionState::Unknown =>
            {
                return self.reject_child(job, &gate, operation, state).await
            }
            Ok(_) => {
                self.unknown_child(job, id).await;
                let _ = gate.finish(false).await;
                return Err(WorkbenchError::OutcomeUnknown);
            }
            Err(error) => {
                self.unknown_child(job, id).await;
                let _ = gate.finish(false).await;
                return Err(error);
            }
        };
        let ctx = job.context.clone();
        let scope = job.scope.clone();
        let recorded = self
            .jcall(move |journal| {
                journal.record_statement(
                    &ctx,
                    &scope,
                    id,
                    operation.version.value(),
                    StatementUpdate {
                        ordinal: 0,
                        expected_version: 2,
                        outcome: StatementStatus::Succeeded,
                        effect_evidence: EffectEvidence::PendingTransaction,
                        rows_matched: None,
                        rows_changed: summary.rows_affected,
                        error_code: None,
                    },
                )
            })
            .await;
        let operation = match recorded {
            Ok(operation) => operation,
            Err(error) => {
                self.unknown_child(job, id).await;
                let _ = gate.finish(false).await;
                return Err(error);
            }
        };
        if job.cancelled.load(Ordering::SeqCst) {
            return self.rollback_child(job, &gate, id).await;
        }
        let this = self.clone();
        let committing_job = job.clone();
        let ctx = job.context.clone();
        let scope = job.scope.clone();
        let committed = gate
            .with_commit(move |connection| {
                Box::pin(async move {
                    let _execution = this.active_execution(&committing_job)?;
                    let native_ctx = ctx.clone();
                    let native_scope = scope.clone();
                    let requested = this
                        .jcall(move |journal| {
                            journal.transition(
                                &native_ctx,
                                &native_scope,
                                id,
                                operation.version.value(),
                                Transition::CommitRequested,
                            )
                        })
                        .await?;
                    let cancelled = committing_job.cancelled.load(Ordering::SeqCst);
                    connection
                        .transaction_control(if cancelled {
                            TransactionCommand::Rollback
                        } else {
                            TransactionCommand::Commit
                        })
                        .await?;
                    this.jcall(move |journal| {
                        journal.transition(
                            &ctx,
                            &scope,
                            id,
                            requested.version.value(),
                            if cancelled {
                                Transition::RollbackConfirmed
                            } else {
                                Transition::CommitConfirmed
                            },
                        )
                    })
                    .await
                })
            })
            .await;
        match committed {
            Ok(operation) => {
                let _ = gate.finish(true).await;
                Ok(operation)
            }
            Err(error) => {
                self.unknown_child(job, id).await;
                let _ = gate.finish(false).await;
                Err(error)
            }
        }
    }
    async fn current(
        &self,
        ctx: &InvocationContext,
        scope: &Scope,
        id: Uuid,
    ) -> Result<Operation, WorkbenchError> {
        let ctx = ctx.clone();
        let scope = scope.clone();
        self.jcall(move |journal| journal.get(&ctx, &scope, id))
            .await
    }
    async fn settle_unsent_failure(
        &self,
        ctx: &InvocationContext,
        scope: &Scope,
        id: Uuid,
    ) -> Result<(), WorkbenchError> {
        let ctx = ctx.clone();
        let scope = scope.clone();
        self.jcall(move |journal| {
            let operation = journal.get(&ctx, &scope, id)?;
            if operation.state == OperationState::Accepted {
                journal.transition(
                    &ctx,
                    &scope,
                    id,
                    operation.version.value(),
                    Transition::FailedBeforeSend,
                )?;
            }
            Ok(())
        })
        .await
    }
    async fn reject_child(
        &self,
        job: &ImportJob,
        gate: &FreshWriteGate,
        operation: Operation,
        state: DriverTransactionState,
    ) -> Result<Operation, WorkbenchError> {
        let ctx = job.context.clone();
        let scope = job.scope.clone();
        let id = operation.operation_id;
        let recorded = self
            .jcall(move |journal| {
                journal.record_statement(
                    &ctx,
                    &scope,
                    id,
                    operation.version.value(),
                    StatementUpdate {
                        ordinal: 0,
                        expected_version: 2,
                        outcome: StatementStatus::Failed,
                        effect_evidence: if state == DriverTransactionState::Idle {
                            EffectEvidence::RolledBack
                        } else {
                            EffectEvidence::PendingTransaction
                        },
                        rows_matched: None,
                        rows_changed: None,
                        error_code: Some("WB_STATEMENT_REJECTED".into()),
                    },
                )
            })
            .await;
        if let Err(error) = recorded {
            self.unknown_child(job, id).await;
            let _ = gate.finish(false).await;
            return Err(error);
        }
        if state == DriverTransactionState::Idle {
            let ctx = job.context.clone();
            let scope = job.scope.clone();
            let settled = self
                .jcall(move |journal| {
                    let old = journal.get(&ctx, &scope, id)?;
                    journal.transition(
                        &ctx,
                        &scope,
                        id,
                        old.version.value(),
                        Transition::RollbackConfirmed,
                    )
                })
                .await;
            if settled.is_err() {
                self.unknown_child(job, id).await
            }
            let _ = gate.finish(settled.is_ok()).await;
            settled
        } else {
            self.rollback_child(job, gate, id).await
        }
    }
    async fn rollback_child(
        &self,
        job: &ImportJob,
        gate: &FreshWriteGate,
        id: Uuid,
    ) -> Result<Operation, WorkbenchError> {
        if let Err(error) = gate
            .with_cleanup(|connection| {
                Box::pin(async move {
                    connection
                        .transaction_control(TransactionCommand::Rollback)
                        .await
                })
            })
            .await
        {
            self.unknown_child(job, id).await;
            let _ = gate.finish(false).await;
            return Err(error);
        }
        let ctx = job.context.clone();
        let scope = job.scope.clone();
        let settled = self
            .jcall(move |journal| {
                let operation = journal.get(&ctx, &scope, id)?;
                journal.transition(
                    &ctx,
                    &scope,
                    id,
                    operation.version.value(),
                    Transition::RollbackConfirmed,
                )
            })
            .await;
        if settled.is_err() {
            self.unknown_child(job, id).await;
        }
        let _ = gate.finish(settled.is_ok()).await;
        settled
    }
    async fn unknown_child(&self, job: &ImportJob, id: Uuid) {
        let ctx = job.context.clone();
        let scope = job.scope.clone();
        let _ = self
            .jcall(move |journal| {
                let operation = journal.get(&ctx, &scope, id)?;
                journal.transition(
                    &ctx,
                    &scope,
                    id,
                    operation.version.value(),
                    Transition::EffectsUnknown,
                )
            })
            .await;
    }
    async fn finish_parent(&self, job: &ImportJob, id: Uuid, next: Transition) {
        let ctx = job.context.clone();
        let scope = job.scope.clone();
        let _ = self
            .jcall(move |journal| {
                let operation = journal.get(&ctx, &scope, id)?;
                let mut metadata = journal
                    .movement_metadata(&ctx, &scope, id)?
                    .ok_or(WorkbenchError::StoreUnavailable)?;
                if matches!(next, Transition::EffectsUnknown) {
                    if let MovementMetadata::Import { progress, .. } = &mut metadata {
                        progress.unresolved_batches = 1.into();
                    }
                }
                journal.update_movement(
                    &ctx,
                    &scope,
                    id,
                    operation.version.value(),
                    metadata,
                    Some(next),
                )
            })
            .await;
    }
    async fn jcall<T: Send + 'static>(
        &self,
        action: impl FnOnce(&OperationJournal) -> Result<T, WorkbenchError> + Send + 'static,
    ) -> Result<T, WorkbenchError> {
        let journal = self.journal.clone();
        tokio::task::spawn_blocking(move || action(&journal))
            .await
            .map_err(|_| WorkbenchError::Io)?
    }
}
struct Discard;
#[async_trait]
impl RowSink for Discard {
    async fn columns(&mut self, _: Vec<Column>) -> Result<(), WorkbenchError> {
        Ok(())
    }
    async fn row(&mut self, _: Vec<TypedCell>) -> Result<(), WorkbenchError> {
        Err(WorkbenchError::Unsupported)
    }
}
fn same_basis(review: &Review, receipt: &BindingReceipt) -> Result<(), WorkbenchError> {
    if receipt.scope != review.scope
        || receipt.descriptor_revision != review.revision
        || receipt.authority_epoch != review.authority
    {
        return Err(WorkbenchError::BindingStale);
    }
    Ok(())
}
fn validate_warnings(review: &Review, input: &ImportStartInput) -> Result<(), WorkbenchError> {
    let mut actual = input.confirmed_warnings.clone();
    let mut expected = review.warnings.clone();
    actual.sort();
    expected.sort();
    if actual != expected {
        return Err(WorkbenchError::InvalidInput);
    }
    Ok(())
}
fn resolve_mapping(
    mapping: &BTreeMap<String, Uuid>,
    relation: &RelationMetadata,
) -> Result<Vec<MovementMapping>, WorkbenchError> {
    let mut result = Vec::new();
    let mut used = BTreeSet::new();
    for (key, id) in mapping {
        let ordinal = key
            .strip_prefix('c')
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|value| format!("c{value}") == *key && *value < 512)
            .ok_or(WorkbenchError::InvalidInput)?;
        let column = relation
            .columns
            .iter()
            .find(|column| column.column.column_id == *id)
            .ok_or(WorkbenchError::CursorInvalid)?;
        if !used.insert(*id) {
            return Err(WorkbenchError::InvalidInput);
        }
        result.push(MovementMapping {
            source_key: key.clone(),
            source_ordinal: ordinal,
            column_id: *id,
            target_ordinal: column.ordinal as u32,
        });
    }
    result.sort_by_key(|item| item.target_ordinal);
    Ok(result)
}
fn map_record(
    fields: &[SourceScalar],
    mapping: &[MovementMapping],
    relation: &RelationMetadata,
) -> Result<Vec<TypedCell>, WorkbenchError> {
    mapping
        .iter()
        .map(|item| {
            let column = relation
                .columns
                .iter()
                .find(|column| column.column.column_id == item.column_id)
                .ok_or(WorkbenchError::CursorInvalid)?;
            let source = fields
                .get(item.source_ordinal as usize)
                .ok_or(WorkbenchError::InvalidInput)?;
            let value = convert(source, column)?;
            validate_column_cell(relation.engine, column, &value)?;
            Ok(value)
        })
        .collect()
}
fn convert(source: &SourceScalar, column: &ColumnMetadata) -> Result<TypedCell, WorkbenchError> {
    if let SourceScalar::Typed(value) = source {
        return Ok(value.clone());
    }
    if matches!(source, SourceScalar::Null) {
        return Ok(TypedCell::Null);
    }
    let text = match source {
        SourceScalar::Text(value) | SourceScalar::Number(value) => value.as_str(),
        SourceScalar::Bool(value) => return Ok(TypedCell::Bool { value: *value }),
        _ => return Err(WorkbenchError::Unsupported),
    };
    let type_name = column.column.type_name.to_ascii_lowercase();
    if matches!(type_name.as_str(), "bool" | "boolean") {
        return match text {
            "true" => Ok(TypedCell::Bool { value: true }),
            "false" => Ok(TypedCell::Bool { value: false }),
            _ => Err(WorkbenchError::InvalidInput),
        };
    }
    if type_name.contains("int") {
        if column.type_detail.to_ascii_lowercase().contains("unsigned") {
            let value = text
                .parse::<u64>()
                .map_err(|_| WorkbenchError::InvalidInput)?;
            if value.to_string() != text {
                return Err(WorkbenchError::InvalidInput);
            }
            return Ok(TypedCell::U64 { value: text.into() });
        }
        let value = text
            .parse::<i64>()
            .map_err(|_| WorkbenchError::InvalidInput)?;
        if value.to_string() != text {
            return Err(WorkbenchError::InvalidInput);
        }
        return Ok(TypedCell::I64 { value: text.into() });
    }
    if type_name.contains("numeric") || type_name.contains("decimal") {
        crate::types::validate_decimal(text, None)?;
        return Ok(TypedCell::Decimal {
            value: text.into(),
            scale: None,
        });
    }
    if matches!(source, SourceScalar::Number(_)) {
        return Err(WorkbenchError::Unsupported);
    }
    Ok(TypedCell::Text { value: text.into() })
}
fn preview_page(
    review: &Review,
    start: usize,
    limit: usize,
    budget: Arc<NativeBudget>,
) -> Result<ImportPreviewReply, WorkbenchError> {
    if start > review.sample.len() {
        return Err(WorkbenchError::CursorInvalid);
    }
    let end = start.saturating_add(limit).min(review.sample.len());
    let memory = budget.reserve(2 * 1024 * 1024)?;
    let reply = ImportPreviewReply {
        _memory: memory,
        preview_id: review.id,
        columns: review
            .mapping
            .iter()
            .map(|item| {
                review
                    .relation
                    .columns
                    .iter()
                    .find(|column| column.column.column_id == item.column_id)
                    .map(|column| column.column.clone())
                    .ok_or(WorkbenchError::CursorInvalid)
            })
            .collect::<Result<_, _>>()?,
        rows: Page {
            items: review.sample[start..end].to_vec(),
            next_cursor: (end < review.sample.len()).then(|| format!("{}:{end}", review.id)),
            completeness: if review.sample.len() as u64 == review.total_rows {
                Completeness::Complete
            } else {
                Completeness::Limited
            },
            count: Count::Exact {
                value: review.total_rows.into(),
            },
            as_of_ms: now_ms().into(),
        },
        warnings: review.warnings.clone(),
    };
    encoded_size(&reply, 512 * 1024)?;
    Ok(reply)
}
