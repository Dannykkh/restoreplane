//! Immutable native edit review, optimistic row verification and one commit.
use crate::browse::{BrowseService, SourceRow};
use crate::driver::{
    DriverConnection, DriverSummary, DriverTransactionState, QueryLimits, RowSink,
    TransactionCommand,
};
use crate::generated_sql::{
    self, FreshWriteGate, GeneratedMutation, NativeInsertLimits, WriteCertification,
};
use crate::journal::{AdmissionSpec, OperationJournal, StatementSpec, StatementUpdate, Transition};
use crate::metadata::{MetadataCatalog, RelationMetadata};
use crate::result_stream::{encoded_size, MemoryReservation, NativeBudget};
use crate::types::{now_ms, placeholder, quote_identifier};
use crate::{InvocationContext, WorkbenchError, WorkbenchState};
use restoreplane_domain::workbench::*;
use restoreplane_domain::workbench_edit::*;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use uuid::Uuid;

struct Review {
    owner: String,
    window: Uuid,
    session: SessionRef,
    receipt: BindingReceipt,
    relation: Arc<RelationMetadata>,
    wire: EditReview,
    sources: Vec<Option<Arc<SourceRow>>>,
    digest: String,
    created: Instant,
    unknown: AtomicBool,
    _memory: MemoryReservation,
}
struct Conflict {
    review: Arc<Review>,
    wire: ConflictObservation,
    created: Instant,
    _memory: MemoryReservation,
}
pub struct EditService {
    runtime: Arc<WorkbenchState>,
    metadata: Arc<MetadataCatalog>,
    browse: Arc<BrowseService>,
    journal: Arc<OperationJournal>,
    budget: Arc<NativeBudget>,
    certification: Option<WriteCertification>,
    reviews: Mutex<HashMap<Uuid, Arc<Review>>>,
    conflicts: Mutex<HashMap<Uuid, Conflict>>,
}
fn refuse(reason: &str) -> WorkbenchError {
    WorkbenchError::EditRefused(reason.into())
}
fn change_values(change: &EditChange) -> &[EditValue] {
    match change {
        EditChange::Insert { values } | EditChange::Update { values, .. } => values,
        EditChange::Delete { .. } => &[],
    }
}
fn handle(change: &EditChange) -> Option<Uuid> {
    match change {
        EditChange::Insert { .. } => None,
        EditChange::Update { row_handle, .. } | EditChange::Delete { row_handle } => {
            Some(*row_handle)
        }
    }
}
fn value_digest(value: &impl serde::Serialize) -> Result<String, WorkbenchError> {
    Ok(
        blake3::hash(&serde_json::to_vec(value).map_err(|_| WorkbenchError::InvalidInput)?)
            .to_hex()
            .to_string(),
    )
}
fn same_receipt(first: &BindingReceipt, next: &BindingReceipt) -> Result<(), WorkbenchError> {
    if first.scope != next.scope
        || first.descriptor != next.descriptor
        || first.descriptor_revision != next.descriptor_revision
        || first.authority_epoch != next.authority_epoch
    {
        Err(WorkbenchError::BindingStale)
    } else {
        Ok(())
    }
}
impl EditService {
    pub fn new(
        runtime: Arc<WorkbenchState>,
        metadata: Arc<MetadataCatalog>,
        browse: Arc<BrowseService>,
        journal: Arc<OperationJournal>,
        certification: Option<WriteCertification>,
    ) -> Self {
        Self {
            runtime,
            metadata: metadata.clone(),
            browse,
            journal,
            budget: metadata.budget(),
            certification,
            reviews: Mutex::new(HashMap::new()),
            conflicts: Mutex::new(HashMap::new()),
        }
    }
    fn context(&self, ctx: &InvocationContext) -> Result<(), WorkbenchError> {
        ctx.principal().ensure_standard()?;
        if ctx.principal() != self.runtime.principal() {
            return Err(WorkbenchError::Forbidden);
        }
        Ok(())
    }
    async fn jcall<
        T: Send + 'static,
        F: FnOnce(&OperationJournal) -> Result<T, WorkbenchError> + Send + 'static,
    >(
        &self,
        work: F,
    ) -> Result<T, WorkbenchError> {
        let journal = self.journal.clone();
        tokio::task::spawn_blocking(move || work(&journal))
            .await
            .map_err(|_| WorkbenchError::Io)?
    }
    pub async fn prepare(
        &self,
        ctx: &InvocationContext,
        input: EditPrepareInput,
    ) -> Result<EditReview, WorkbenchError> {
        self.context(ctx)?;
        if input.changes.is_empty() || input.changes.len() > 100 {
            return Err(WorkbenchError::Limit);
        }
        encoded_size(&input.changes, 512 * 1024)?;
        let view = self.runtime.get(ctx, input.session)?;
        if view.mode == Mode::ReadOnly {
            return Err(refuse("read_only_mode"));
        }
        let cert = self
            .certification
            .as_ref()
            .ok_or_else(|| refuse("verified_editing_not_certified"))?;
        let facts = self
            .runtime
            .with_connection(ctx, input.session, AuthorizePurpose::Read, |connection| {
                Box::pin(async move { Ok(connection.facts().clone()) })
            })
            .await
            .map_err(|error| {
                if error == WorkbenchError::TargetBusy {
                    refuse("manual_transaction_active")
                } else {
                    error
                }
            })?;
        if !cert.supports(&facts) {
            return Err(refuse("verified_editing_not_certified"));
        }
        let receipt = self
            .runtime
            .binding_receipt(ctx, input.session, AuthorizePurpose::Mutate)
            .await?;
        let relation = self
            .metadata
            .resolve_relation(ctx, input.session, &input.object)
            .await?;
        if relation.kind != "table" {
            return Err(refuse("view"));
        }
        if matches!(facts.engine, Engine::Mysql | Engine::Mariadb)
            && relation.schema != facts.context.database
        {
            return Err(refuse("source_changed"));
        }
        let key = relation.stable_key().ok_or_else(|| {
            refuse(if relation.keys.iter().any(|key| key.nullable) {
                "nullable_key"
            } else if relation.keys.iter().any(|key| key.partial) {
                "partial_key"
            } else if relation.keys.iter().any(|key| key.expression) {
                "expression_key"
            } else {
                "keyless"
            })
        })?;
        if relation.columns.iter().any(|column| column.generated) {
            return Err(refuse("generated_column"));
        }
        if matches!(relation.engine, Engine::Mysql | Engine::Mariadb)
            && !relation
                .storage_engine
                .as_deref()
                .is_some_and(|engine| engine.eq_ignore_ascii_case("InnoDB"))
        {
            return Err(refuse("non_transactional_storage"));
        }
        if relation
            .columns
            .iter()
            .any(|column| column.defaulted && !column.null_default)
        {
            return Err(refuse("default_effects_unproven"));
        }
        let checked_relation = relation.clone();
        let budget = self.budget.clone();
        self.runtime
            .with_connection(
                ctx,
                input.session,
                AuthorizePurpose::Read,
                move |connection| {
                    Box::pin(async move {
                        Ok(generated_sql::verify_transactional_table(
                            connection,
                            &budget,
                            &checked_relation,
                        )
                        .await)
                    })
                },
            )
            .await?
            .map_err(|error| {
                if error == WorkbenchError::Unsupported {
                    refuse("transactional_effects_unproven")
                } else {
                    error
                }
            })?;
        let mut seen_handles = HashSet::new();
        let mut seen_keys = HashSet::new();
        let mut sources = Vec::new();
        let mut changes = Vec::new();
        let mut previews = Vec::new();
        let mut parameters = Vec::new();
        for change in &input.changes {
            let values = change_values(change);
            let mut columns = HashSet::new();
            for value in values {
                if !columns.insert(value.column_id) {
                    return Err(WorkbenchError::InvalidInput);
                }
                let column = relation
                    .columns
                    .iter()
                    .find(|c| c.column.column_id == value.column_id)
                    .ok_or(WorkbenchError::InvalidInput)?;
                generated_sql::validate_column_cell(relation.engine, column, &value.value)
                    .map_err(|_| refuse("unsupported_type"))?;
            }
            let source = if let Some(row_handle) = handle(change) {
                if !seen_handles.insert(row_handle) {
                    return Err(WorkbenchError::InvalidInput);
                }
                let source = self.browse.original(ctx, input.session, row_handle)?;
                if !source.single_table || source.object != input.object {
                    return Err(refuse("join_origin_unproven"));
                }
                if !source.complete_values || source.original.len() != relation.columns.len() {
                    return Err(refuse("origin_and_key_not_certified"));
                }
                if source.key_columns != key.columns
                    || source
                        .key_values
                        .iter()
                        .any(|v| !generated_sql::exact_identity_cell(v))
                {
                    return Err(refuse("comparison_unproven"));
                }
                if !seen_keys.insert(value_digest(&source.key_values)?) {
                    return Err(WorkbenchError::InvalidInput);
                }
                for original in &source.original {
                    let column = relation
                        .columns
                        .iter()
                        .find(|c| c.column.column_id == original.column_id)
                        .ok_or(WorkbenchError::InvalidInput)?;
                    generated_sql::validate_column_cell(relation.engine, column, &original.value)
                        .map_err(|_| refuse("unsupported_type"))?;
                }
                Some(source)
            } else {
                None
            };
            let originals = source
                .as_ref()
                .map(|row| {
                    row.original
                        .iter()
                        .map(|v| EditValue {
                            column_id: v.column_id,
                            value: v.value.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            let (kind, sql, bound) = match change {
                EditChange::Insert { values } => {
                    let generated = generated_sql::prepare_insert(
                        relation.clone(),
                        &values.iter().map(|v| v.column_id).collect::<Vec<_>>(),
                        &[values.iter().map(|v| v.value.clone()).collect()],
                        NativeInsertLimits::editing(),
                    )?;
                    (
                        "insert",
                        generated.preview().to_owned(),
                        generated.parameters().to_vec(),
                    )
                }
                EditChange::Update { values, .. } => {
                    let source = source.as_ref().ok_or(WorkbenchError::InvalidInput)?;
                    let generated = generated_sql::prepare_mutation(
                        relation.clone(),
                        &values
                            .iter()
                            .map(|v| (v.column_id, v.value.clone()))
                            .collect::<Vec<_>>(),
                        &source.key_values,
                        false,
                    )?;
                    ("update", generated.sql, generated.parameters)
                }
                EditChange::Delete { .. } => {
                    let source = source.as_ref().ok_or(WorkbenchError::InvalidInput)?;
                    let generated = generated_sql::prepare_mutation(
                        relation.clone(),
                        &[],
                        &source.key_values,
                        true,
                    )?;
                    ("delete", generated.sql, generated.parameters)
                }
            };
            let omitted = if kind == "insert" {
                relation
                    .columns
                    .iter()
                    .filter(|c| !columns.contains(&c.column.column_id))
                    .map(|c| c.column.column_id)
                    .collect()
            } else {
                Vec::new()
            };
            previews.push(sql);
            parameters.extend(bound);
            changes.push(ReviewedChange {
                kind: kind.into(),
                row_handle: handle(change),
                original: originals,
                draft: values.to_vec(),
                omitted_default_column_ids: omitted,
            });
            sources.push(source);
        }
        let wire = EditReview {
            draft_id: Uuid::new_v4(),
            review_id: Uuid::new_v4(),
            target: TargetContext {
                scope: view.scope.clone(),
                connection_alias: view.scope.connection_profile_id.as_str().into(),
                actual_endpoint: view.actual_context.endpoint,
                database_name: view.actual_context.database,
                database_role: view.actual_context.role,
                purpose: view.actual_context.purpose,
            },
            object: input.object,
            statement_previews: previews,
            parameter_preview: parameters,
            changes,
            warnings: vec![
                "standalone_transaction".into(),
                "optimistic_basis_not_history".into(),
                "transactional_effects_only".into(),
            ],
            expires_at_ms: now_ms().saturating_add(60_000).into(),
        };
        let bytes = encoded_size(&wire, 1024 * 1024)?;
        let memory = self
            .budget
            .reserve(bytes.saturating_mul(3) + input.changes.len() * 4096)?;
        let digest = value_digest(&wire)?;
        let review = Arc::new(Review {
            owner: ctx.principal().owner_id().into(),
            window: ctx.window_instance_id(),
            session: input.session,
            receipt,
            relation,
            wire: wire.clone(),
            sources,
            digest,
            created: Instant::now(),
            unknown: AtomicBool::new(false),
            _memory: memory,
        });
        let mut reviews = self.reviews.lock().map_err(|_| WorkbenchError::Io)?;
        reviews.retain(|_, review| review.created.elapsed() < Duration::from_secs(60));
        if reviews.len() >= 64 {
            return Err(WorkbenchError::Limit);
        }
        reviews.insert(wire.review_id, review);
        Ok(wire)
    }
    fn review(
        &self,
        ctx: &InvocationContext,
        input: &EditApplyInput,
    ) -> Result<Arc<Review>, WorkbenchError> {
        self.context(ctx)?;
        let review = self
            .reviews
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .get(&input.review_id)
            .cloned()
            .ok_or(WorkbenchError::ResultExpired)?;
        if review.owner != ctx.principal().owner_id()
            || review.window != ctx.window_instance_id()
            || review.session != input.session
            || review.wire.draft_id != input.draft_id
        {
            return Err(WorkbenchError::Forbidden);
        }
        if review.unknown.load(Ordering::SeqCst) {
            return Err(WorkbenchError::OutcomeUnknown);
        }
        if review.created.elapsed() >= Duration::from_secs(60) {
            return Err(WorkbenchError::ResultExpired);
        }
        let mut warnings = input.confirmed_warnings.clone();
        warnings.sort();
        warnings.dedup();
        let mut required = review.wire.warnings.clone();
        required.sort();
        if warnings != required {
            return Err(WorkbenchError::InvalidInput);
        }
        Ok(review)
    }
    fn resolved(review: &Review, input: &EditApplyInput) -> Value {
        json!({"session":input.session,"draftId":input.draft_id,"reviewId":input.review_id,"reviewDigest":review.digest,"object":review.wire.object,"scope":review.receipt.scope,"descriptorRevision":review.receipt.descriptor_revision,"authorityEpoch":review.receipt.authority_epoch})
    }
    fn public_payload(input: &EditApplyInput) -> Result<Value, WorkbenchError> {
        if input.confirmed_warnings.len() > 64
            || input
                .confirmed_warnings
                .iter()
                .any(|warning| warning.len() > 128)
        {
            return Err(WorkbenchError::InvalidInput);
        }
        let mut normalized = input.clone();
        normalized.confirmed_warnings.sort();
        normalized.confirmed_warnings.dedup();
        serde_json::to_value(normalized).map_err(|_| WorkbenchError::InvalidInput)
    }
    pub async fn prepare_apply(
        &self,
        ctx: &InvocationContext,
        input: EditApplyInput,
    ) -> Result<PreparedAttempt, WorkbenchError> {
        let public = Self::public_payload(&input)?;
        let review = self.review(ctx, &input)?;
        let receipt = self
            .runtime
            .binding_receipt(ctx, input.session, AuthorizePurpose::Mutate)
            .await?;
        same_receipt(&review.receipt, &receipt)?;
        let payload = Self::resolved(&review, &input);
        let ctx = ctx.clone();
        self.jcall(move |journal| {
            journal.prepare_edit_attempt(&ctx, &receipt, input.session, &payload, &public)
        })
        .await
    }
    pub async fn apply(
        self: &Arc<Self>,
        ctx: &InvocationContext,
        token: String,
        input: EditApplyInput,
    ) -> Result<Admission, WorkbenchError> {
        self.context(ctx)?;
        let public = Self::public_payload(&input)?;
        let lookup_ctx = ctx.clone();
        let lookup_token = token.clone();
        if let Some(admission) = self
            .jcall(move |journal| {
                journal.edit_status(&lookup_ctx, &lookup_token, &public, input.session)
            })
            .await?
        {
            // Status is authenticated by the original native claims. It does
            // not need a volatile review, a live connection, or another send.
            return Ok(admission);
        }
        let review = self.review(ctx, &input)?;
        let receipt = self
            .runtime
            .binding_receipt(ctx, input.session, AuthorizePurpose::Mutate)
            .await?;
        same_receipt(&review.receipt, &receipt)?;
        let spec = AdmissionSpec {
            kind: OperationKind::VerifiedApply,
            session: Some(input.session),
            transaction_id: None,
            statements: review
                .wire
                .changes
                .iter()
                .enumerate()
                .map(|(ordinal, change)| {
                    Ok(StatementSpec {
                        digest: value_digest(&(ordinal, change))?,
                        effect_class: EffectClass::Write,
                        certified_read_only: false,
                        transaction_id: None,
                    })
                })
                .collect::<Result<_, WorkbenchError>>()?,
            batch: None,
        };
        let payload = Self::resolved(&review, &input);
        let credential = self.runtime.credential_reference(ctx, input.session)?;
        let native_ctx = ctx.clone();
        let native_receipt = receipt.clone();
        let admission = self
            .jcall(move |journal| {
                journal.admit(
                    &native_ctx,
                    &token,
                    "edit.apply",
                    &payload,
                    &native_receipt,
                    Some(credential),
                    spec,
                )
            })
            .await?;
        if admission.deduplicated {
            return Ok(admission);
        }
        let gate = match FreshWriteGate::acquire(
            self.metadata.clone(),
            ctx,
            input.session,
            &review.wire.object,
            self.certification
                .as_ref()
                .ok_or_else(|| refuse("verified_editing_not_certified"))?,
        )
        .await
        {
            Ok(gate) => gate,
            Err(error) => {
                let ctx = ctx.clone();
                let scope = receipt.scope.clone();
                let id = admission.operation_id;
                let version = admission.version.value();
                self.jcall(move |journal| {
                    journal.transition(&ctx, &scope, id, version, Transition::FailedBeforeSend)
                })
                .await?;
                if error == WorkbenchError::CursorInvalid {
                    let id = self.capture_conflict(
                        review.clone(),
                        id,
                        unavailable(&review, "schema_changed"),
                        None,
                    )?;
                    return Err(WorkbenchError::EditConflict(id));
                }
                return Err(error);
            }
        };
        let native_ctx = ctx.clone();
        let scope = receipt.scope.clone();
        let id = admission.operation_id;
        let version = admission.version.value();
        let epoch = self
            .runtime
            .get(ctx, input.session)?
            .execution_epoch
            .value();
        let dispatched = self
            .jcall(move |journal| {
                let permit =
                    journal.mark_dispatch(&native_ctx, &scope, id, version, epoch, &receipt)?;
                journal.transition(
                    &native_ctx,
                    &scope,
                    id,
                    permit.version(),
                    Transition::Running,
                )
            })
            .await;
        let operation = match dispatched {
            Ok(operation) => operation,
            Err(error) => {
                let _ = gate.finish(true).await;
                return Err(error);
            }
        };
        let outcome = match gate.begin_transaction().await {
            Ok(()) => {
                let this = self.clone();
                let ctx = ctx.clone();
                let native_review = review.clone();
                let native_gate = gate.clone();
                gate.with_connection(move |connection| {
                    Box::pin(async move {
                        this.apply_rows(connection, &native_gate, &ctx, &native_review, operation)
                            .await
                    })
                })
                .await
            }
            Err(error) => Err(error),
        };
        match outcome {
            Ok(ApplyRows::Ready(operation)) => {
                let this = self.clone();
                let native_ctx = ctx.clone();
                let scope = review.receipt.scope.clone();
                let id = operation.operation_id;
                let committed = gate
                    .with_commit(move |connection| {
                        Box::pin(async move {
                            let version = operation.version.value();
                            let ctx = native_ctx.clone();
                            let native_scope = scope.clone();
                            let requested = this
                                .jcall(move |journal| {
                                    journal.transition(
                                        &ctx,
                                        &native_scope,
                                        id,
                                        version,
                                        Transition::CommitRequested,
                                    )
                                })
                                .await?;
                            connection
                                .transaction_control(TransactionCommand::Commit)
                                .await?;
                            let version = requested.version.value();
                            this.jcall(move |journal| {
                                journal.transition(
                                    &native_ctx,
                                    &scope,
                                    id,
                                    version,
                                    Transition::CommitConfirmed,
                                )
                            })
                            .await
                        })
                    })
                    .await;
                match committed {
                    Ok(operation) => {
                        gate.finish(true).await?;
                        Ok(Admission {
                            operation_id: operation.operation_id,
                            version: operation.version,
                            deduplicated: false,
                        })
                    }
                    Err(error) => {
                        self.unknown(ctx, &review, id).await;
                        let _ = gate.finish(false).await;
                        Err(error)
                    }
                }
            }
            Ok(ApplyRows::Conflict(rows, schema)) => {
                let rollback = self
                    .rollback(ctx, &gate, admission.operation_id, &review)
                    .await;
                match rollback {
                    Ok(()) => {
                        let conflict =
                            self.capture_conflict(review, admission.operation_id, rows, schema)?;
                        Err(WorkbenchError::EditConflict(conflict))
                    }
                    Err(error) => Err(error),
                }
            }
            Ok(ApplyRows::Rejected(error)) => {
                self.rollback(ctx, &gate, admission.operation_id, &review)
                    .await?;
                Err(error)
            }
            Err(error) => {
                self.unknown(ctx, &review, admission.operation_id).await;
                let _ = gate.finish(false).await;
                Err(error)
            }
        }
    }
    async fn unknown(&self, ctx: &InvocationContext, review: &Review, id: Uuid) {
        review.unknown.store(true, Ordering::SeqCst);
        let ctx = ctx.clone();
        let scope = review.receipt.scope.clone();
        let _ = self
            .jcall(move |journal| {
                let old = journal.get(&ctx, &scope, id)?;
                journal.transition(
                    &ctx,
                    &scope,
                    id,
                    old.version.value(),
                    Transition::EffectsUnknown,
                )
            })
            .await;
    }
    async fn rollback(
        &self,
        ctx: &InvocationContext,
        gate: &FreshWriteGate,
        id: Uuid,
        review: &Review,
    ) -> Result<(), WorkbenchError> {
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
            self.unknown(ctx, review, id).await;
            let _ = gate.finish(false).await;
            return Err(error);
        }
        let native = ctx.clone();
        let scope = review.receipt.scope.clone();
        let settled = self
            .jcall(move |journal| {
                let old = journal.get(&native, &scope, id)?;
                journal.transition(
                    &native,
                    &scope,
                    id,
                    old.version.value(),
                    Transition::RollbackConfirmed,
                )
            })
            .await;
        if let Err(error) = settled {
            self.unknown(ctx, review, id).await;
            let _ = gate.finish(false).await;
            return Err(error);
        }
        gate.finish(true).await
    }
    async fn apply_rows(
        &self,
        connection: &mut dyn DriverConnection,
        gate: &FreshWriteGate,
        ctx: &InvocationContext,
        review: &Review,
        mut operation: Operation,
    ) -> Result<ApplyRows, WorkbenchError> {
        if let Err(error) = gate.validate_physical(connection).await {
            return if error == WorkbenchError::CursorInvalid {
                Ok(ApplyRows::Conflict(
                    unavailable(review, "schema_changed"),
                    None,
                ))
            } else {
                Err(error)
            };
        }
        let mut observations: Vec<Option<ConflictRow>> = vec![None; review.sources.len()];
        let mut indices = (0..review.sources.len())
            .filter(|index| review.sources[*index].is_some())
            .collect::<Vec<_>>();
        indices.sort_by_key(|index| {
            value_digest(
                &review.sources[*index]
                    .as_ref()
                    .expect("selected source")
                    .key_values,
            )
            .unwrap_or_default()
        });
        for index in indices {
            let source = review.sources[index]
                .as_ref()
                .ok_or(WorkbenchError::InvalidInput)?;
            let current =
                locked_current(connection, &self.budget, &review.relation, source).await?;
            let row = ConflictRow {
                change_index: index as u32,
                row_handle: review.wire.changes[index].row_handle,
                current: current.clone(),
            };
            observations[index] = Some(row);
            let equal = match &current {
                ObservedCurrent::Present { values } => {
                    same_values(&review.wire.changes[index].original, values)
                }
                _ => false,
            };
            if !equal {
                let rows = observations
                    .into_iter()
                    .enumerate()
                    .map(|(index, row)| {
                        row.unwrap_or(ConflictRow {
                            change_index: index as u32,
                            row_handle: review.wire.changes[index].row_handle,
                            current: ObservedCurrent::Unavailable {
                                reason: "not_observed".into(),
                            },
                        })
                    })
                    .collect();
                return Ok(ApplyRows::Conflict(
                    rows,
                    Some(review.wire.object.schema_version.clone()),
                ));
            }
        }
        for (index, change) in review.wire.changes.iter().enumerate() {
            let noop = change.kind == "update"
                && change.draft.iter().all(|draft| {
                    change
                        .original
                        .iter()
                        .find(|v| v.column_id == draft.column_id)
                        .is_some_and(|original| same_cell(&original.value, &draft.value))
                });
            let previous = operation
                .statements
                .get(index)
                .ok_or(WorkbenchError::StoreUnavailable)?;
            if !noop {
                let update = StatementUpdate {
                    ordinal: index as u32,
                    expected_version: previous.version.value(),
                    outcome: StatementStatus::Sent,
                    effect_evidence: previous.effect_evidence,
                    rows_matched: None,
                    rows_changed: None,
                    error_code: None,
                };
                operation = self
                    .record(ctx, &review.receipt.scope, operation, update)
                    .await?;
            }
            let summary = if noop {
                DriverSummary {
                    rows_affected: Some(0),
                    transaction_state: DriverTransactionState::Active,
                    ..DriverSummary::default()
                }
            } else {
                let result = if change.kind == "insert" {
                    let insert = generated_sql::prepare_insert(
                        review.relation.clone(),
                        &change.draft.iter().map(|v| v.column_id).collect::<Vec<_>>(),
                        &[change.draft.iter().map(|v| v.value.clone()).collect()],
                        NativeInsertLimits::editing(),
                    )?;
                    insert
                        .execute(connection, gate, QueryLimits::default(), &mut NoRows)
                        .await
                } else {
                    let source = review.sources[index]
                        .as_ref()
                        .ok_or(WorkbenchError::InvalidInput)?;
                    let mutation: GeneratedMutation = generated_sql::prepare_mutation(
                        review.relation.clone(),
                        &change
                            .draft
                            .iter()
                            .map(|v| (v.column_id, v.value.clone()))
                            .collect::<Vec<_>>(),
                        &source.key_values,
                        change.kind == "delete",
                    )?;
                    mutation
                        .execute(connection, gate, QueryLimits::default(), &mut NoRows)
                        .await
                };
                match result {
                    Ok(summary) => summary,
                    Err(error @ WorkbenchError::StatementRejected(_)) => {
                        let previous = operation
                            .statements
                            .get(index)
                            .ok_or(WorkbenchError::StoreUnavailable)?;
                        let update = StatementUpdate {
                            ordinal: index as u32,
                            expected_version: previous.version.value(),
                            outcome: StatementStatus::Failed,
                            effect_evidence: EffectEvidence::PendingTransaction,
                            rows_matched: None,
                            rows_changed: None,
                            error_code: Some(error.code().into()),
                        };
                        self.record(ctx, &review.receipt.scope, operation, update)
                            .await?;
                        return Ok(ApplyRows::Rejected(error));
                    }
                    Err(error) => return Err(error),
                }
            };
            if summary.transaction_state != DriverTransactionState::Active
                || (!noop && summary.rows_affected != Some(1))
            {
                if !noop {
                    let previous = operation
                        .statements
                        .get(index)
                        .ok_or(WorkbenchError::StoreUnavailable)?;
                    let update = StatementUpdate {
                        ordinal: index as u32,
                        expected_version: previous.version.value(),
                        outcome: StatementStatus::Failed,
                        effect_evidence: EffectEvidence::PendingTransaction,
                        rows_matched: summary.rows_affected,
                        rows_changed: summary.rows_affected,
                        error_code: Some("WB_CAPABILITY_UNSUPPORTED".into()),
                    };
                    self.record(ctx, &review.receipt.scope, operation, update)
                        .await?;
                }
                return Ok(ApplyRows::Rejected(refuse("multiple_matches")));
            }
            let previous = operation
                .statements
                .get(index)
                .ok_or(WorkbenchError::StoreUnavailable)?;
            let update = StatementUpdate {
                ordinal: index as u32,
                expected_version: previous.version.value(),
                outcome: StatementStatus::Succeeded,
                effect_evidence: EffectEvidence::PendingTransaction,
                rows_matched: Some(1),
                rows_changed: Some(if noop { 0 } else { 1 }),
                error_code: None,
            };
            operation = self
                .record(ctx, &review.receipt.scope, operation, update)
                .await?;
        }
        Ok(ApplyRows::Ready(operation))
    }
    async fn record(
        &self,
        ctx: &InvocationContext,
        scope: &Scope,
        operation: Operation,
        update: StatementUpdate,
    ) -> Result<Operation, WorkbenchError> {
        let ctx = ctx.clone();
        let scope = scope.clone();
        self.jcall(move |journal| {
            journal.record_statement(
                &ctx,
                &scope,
                operation.operation_id,
                operation.version.value(),
                update,
            )
        })
        .await
    }
    fn capture_conflict(
        &self,
        review: Arc<Review>,
        operation: Uuid,
        rows: Vec<ConflictRow>,
        schema: Option<String>,
    ) -> Result<Uuid, WorkbenchError> {
        let mut indices = HashSet::new();
        for row in &rows {
            let change = review
                .wire
                .changes
                .get(row.change_index as usize)
                .ok_or(WorkbenchError::InvalidInput)?;
            if !indices.insert(row.change_index) || row.row_handle != change.row_handle {
                return Err(WorkbenchError::InvalidInput);
            }
        }
        let id = Uuid::new_v4();
        let wire = ConflictObservation {
            conflict_id: id,
            draft_id: review.wire.draft_id,
            review_id: review.wire.review_id,
            operation_id: operation,
            basis_object: review.wire.object.clone(),
            current_schema_version: schema,
            observed_at_ms: now_ms().into(),
            rows,
        };
        let bytes = encoded_size(&wire, 512 * 1024)?;
        let memory = self.budget.reserve(bytes * 3 + 4096)?;
        let mut conflicts = self.conflicts.lock().map_err(|_| WorkbenchError::Io)?;
        conflicts.retain(|_, c| c.created.elapsed() < Duration::from_secs(60));
        if conflicts.len() >= 64 {
            return Err(WorkbenchError::Limit);
        }
        conflicts.insert(
            id,
            Conflict {
                review,
                wire,
                created: Instant::now(),
                _memory: memory,
            },
        );
        Ok(id)
    }
    pub fn conflict_get(
        &self,
        ctx: &InvocationContext,
        session: SessionRef,
        id: Uuid,
    ) -> Result<ConflictObservation, WorkbenchError> {
        self.context(ctx)?;
        let conflicts = self.conflicts.lock().map_err(|_| WorkbenchError::Io)?;
        let conflict = conflicts.get(&id).ok_or(WorkbenchError::ResultExpired)?;
        if conflict.review.owner != ctx.principal().owner_id()
            || conflict.review.window != ctx.window_instance_id()
            || conflict.review.session != session
        {
            return Err(WorkbenchError::Forbidden);
        }
        if conflict.created.elapsed() >= Duration::from_secs(60) {
            return Err(WorkbenchError::ResultExpired);
        }
        if self.runtime.get(ctx, session)?.state == SessionState::Retiring {
            return Err(WorkbenchError::SessionStale);
        }
        Ok(conflict.wire.clone())
    }
    pub fn discard(&self, ctx: &InvocationContext, draft: Uuid) -> Result<bool, WorkbenchError> {
        self.context(ctx)?;
        let mut reviews = self.reviews.lock().map_err(|_| WorkbenchError::Io)?;
        if reviews.values().any(|r| {
            r.wire.draft_id == draft
                && (r.owner != ctx.principal().owner_id() || r.window != ctx.window_instance_id())
        }) {
            return Err(WorkbenchError::Forbidden);
        }
        let before = reviews.len();
        reviews.retain(|_, r| r.wire.draft_id != draft);
        let discarded = before != reviews.len();
        drop(reviews);
        self.conflicts
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .retain(|_, conflict| {
                conflict.review.wire.draft_id != draft
                    || conflict.review.owner != ctx.principal().owner_id()
                    || conflict.review.window != ctx.window_instance_id()
            });
        Ok(discarded)
    }
    pub fn expire_memory(&self) -> Result<(), WorkbenchError> {
        self.reviews
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .retain(|_, review| review.created.elapsed() < Duration::from_secs(60));
        self.conflicts
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .retain(|_, conflict| conflict.created.elapsed() < Duration::from_secs(60));
        Ok(())
    }
    pub fn retire_session(
        &self,
        ctx: &InvocationContext,
        session: SessionRef,
    ) -> Result<(), WorkbenchError> {
        self.retire(ctx, Some(session))
    }
    pub fn retire_window(&self, ctx: &InvocationContext) -> Result<(), WorkbenchError> {
        self.retire(ctx, None)
    }
    fn retire(
        &self,
        ctx: &InvocationContext,
        session: Option<SessionRef>,
    ) -> Result<(), WorkbenchError> {
        self.context(ctx)?;
        let matches = |r: &Review| {
            r.owner == ctx.principal().owner_id()
                && r.window == ctx.window_instance_id()
                && session.is_none_or(|s| s == r.session)
        };
        self.reviews
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .retain(|_, r| !matches(r));
        self.conflicts
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .retain(|_, c| !matches(&c.review));
        Ok(())
    }
}
enum ApplyRows {
    Ready(Operation),
    Conflict(Vec<ConflictRow>, Option<String>),
    Rejected(WorkbenchError),
}
fn unavailable(review: &Review, reason: &str) -> Vec<ConflictRow> {
    review
        .wire
        .changes
        .iter()
        .enumerate()
        .map(|(index, change)| ConflictRow {
            change_index: index as u32,
            row_handle: change.row_handle,
            current: ObservedCurrent::Unavailable {
                reason: reason.into(),
            },
        })
        .collect()
}
fn same_cell(left: &TypedCell, right: &TypedCell) -> bool {
    match (left, right) {
        (TypedCell::Decimal { value: a, .. }, TypedCell::Decimal { value: b, .. }) => {
            decimal_canonical(a) == decimal_canonical(b)
        }
        _ => left == right,
    }
}
fn decimal_canonical(value: &str) -> String {
    let negative = value.starts_with('-');
    let body = value.trim_start_matches('-');
    let (integer, fraction) = body.split_once('.').unwrap_or((body, ""));
    let integer = integer.trim_start_matches('0');
    let fraction = fraction.trim_end_matches('0');
    let sign = if negative && (!integer.is_empty() || !fraction.is_empty()) {
        "-"
    } else {
        ""
    };
    format!(
        "{sign}{}.{}",
        if integer.is_empty() { "0" } else { integer },
        fraction
    )
}
fn same_values(original: &[EditValue], current: &[EditValue]) -> bool {
    original.len() == current.len()
        && original.iter().all(|original| {
            current
                .iter()
                .find(|v| v.column_id == original.column_id)
                .is_some_and(|v| same_cell(&original.value, &v.value))
        })
}
async fn locked_current(
    connection: &mut dyn DriverConnection,
    budget: &Arc<NativeBudget>,
    relation: &RelationMetadata,
    source: &SourceRow,
) -> Result<ObservedCurrent, WorkbenchError> {
    let mut predicates = Vec::new();
    let key = relation.stable_key().ok_or(WorkbenchError::Unsupported)?;
    for (index, id) in key.columns.iter().enumerate() {
        let column = relation
            .columns
            .iter()
            .find(|c| c.column.column_id == *id)
            .ok_or(WorkbenchError::InvalidInput)?;
        let operator = if relation.engine == Engine::Postgresql {
            " OPERATOR(pg_catalog.=) "
        } else {
            "="
        };
        predicates.push(format!(
            "{}{operator}{}",
            quote_identifier(relation.engine, &column.column.name)?,
            placeholder(relation.engine, index + 1)
        ));
    }
    let columns = relation
        .columns
        .iter()
        .map(|c| quote_identifier(relation.engine, &c.column.name))
        .collect::<Result<Vec<_>, _>>()?
        .join(",");
    let lock = if relation.engine == Engine::Sqlite {
        ""
    } else {
        " FOR UPDATE"
    };
    let sql = format!(
        "SELECT {columns} FROM {} WHERE {} LIMIT 2{lock}",
        relation.qualified_name()?,
        predicates.join(" AND ")
    );
    let rows = crate::types::read(connection, budget, &sql, &source.key_values, 2).await?;
    if rows.rows.is_empty() {
        return Ok(ObservedCurrent::Missing);
    }
    if rows.rows.len() != 1 {
        return Ok(ObservedCurrent::Unavailable {
            reason: "multiple_matches".into(),
        });
    }
    let row = &rows.rows[0];
    if row.len() != relation.columns.len() {
        return Err(WorkbenchError::Unsupported);
    }
    let mut values = Vec::new();
    for (column, value) in relation.columns.iter().zip(row) {
        if generated_sql::validate_column_cell(relation.engine, column, value).is_err() {
            return Ok(ObservedCurrent::Unavailable {
                reason: "unsupported_type".into(),
            });
        }
        values.push(EditValue {
            column_id: column.column.column_id,
            value: value.clone(),
        });
    }
    Ok(ObservedCurrent::Present { values })
}
struct NoRows;
#[async_trait::async_trait]
impl RowSink for NoRows {
    async fn columns(&mut self, columns: Vec<Column>) -> Result<(), WorkbenchError> {
        if columns.is_empty() {
            Ok(())
        } else {
            Err(WorkbenchError::Unsupported)
        }
    }
    async fn row(&mut self, _: Vec<TypedCell>) -> Result<(), WorkbenchError> {
        Err(WorkbenchError::Unsupported)
    }
}
