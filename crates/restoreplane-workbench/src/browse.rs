//! Generated single-relation reads and volatile original-row provenance.
use crate::driver::QueryLimits;
use crate::metadata::{MetadataCatalog, RelationMetadata, ScopeKey};
use crate::result_stream::{
    encoded_size, MemoryReservation, ResultNoticeEmitter, ResultStreams, StreamSpec,
};
use crate::session::{ConnectionLease, ConnectionLeaseKind};
use crate::types::{
    exact_key_cell, now_ms, placeholder, quote_identifier, validate_cells, BoundedRows,
};
use crate::{InvocationContext, WorkbenchError};
use restoreplane_domain::workbench::{AuthorizePurpose, Engine, SessionRef, TypedCell};
use restoreplane_domain::workbench_metadata::{
    Comparison, Completeness, Consistency, Count, Direction, Filter, Limits, NullOrder, ObjectRef,
    Order, Page, ResultPage, Row,
};
use restoreplane_domain::workbench_stream::{Completion, ResultEvent, StreamIdentity};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

pub struct BrowseInput {
    pub object: ObjectRef,
    pub filter: Filter,
    pub order: Vec<Order>,
    pub snapshot_id: Option<Uuid>,
    pub limits: Limits,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowseReply {
    pub result: ResultPage,
    pub stream: Option<StreamIdentity>,
    #[serde(skip)]
    _memory: MemoryReservation,
}
pub struct BrowsePage {
    result: ResultPage,
    _memory: MemoryReservation,
}
impl BrowsePage {
    pub fn result(&self) -> &ResultPage {
        &self.result
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct OriginalValue {
    pub column_id: Uuid,
    pub value: TypedCell,
}
pub struct SourceRow {
    stream: Option<StreamIdentity>,
    pub object: ObjectRef,
    pub result_set_id: Uuid,
    pub session: SessionRef,
    pub original: Vec<OriginalValue>,
    pub key_columns: Vec<Uuid>,
    pub key_values: Vec<TypedCell>,
    pub single_table: bool,
    pub complete_values: bool,
    pub consistency: Consistency,
    scope: ScopeKey,
    created: Instant,
    _memory: MemoryReservation,
}
struct Cursor {
    scope: ScopeKey,
    object: ObjectRef,
    filter: Filter,
    order: Vec<Order>,
    last: Vec<TypedCell>,
    limits: Limits,
    snapshot_id: Option<Uuid>,
    result_set_id: Uuid,
    next_row: u64,
    stream: Option<StreamIdentity>,
    created: Instant,
    _memory: MemoryReservation,
}
struct Snapshot {
    scope: ScopeKey,
    lease: Arc<ConnectionLease>,
    consistency: Consistency,
}
/// Only native composition may load a matrix backed by executed build evidence.
/// A frontend cannot deserialize or assert certification.
#[derive(Clone)]
pub struct SnapshotCertification {
    evidence_id: String,
    versions: Vec<(Engine, String)>,
}
impl SnapshotCertification {
    pub fn from_verified_build(
        evidence_id: String,
        versions: Vec<(Engine, String)>,
    ) -> Result<Self, WorkbenchError> {
        if evidence_id.is_empty()
            || evidence_id.len() > 256
            || versions.len() > 4
            || versions
                .iter()
                .any(|(_, version)| version.is_empty() || version.len() > 512)
        {
            return Err(WorkbenchError::InvalidInput);
        }
        Ok(Self {
            evidence_id,
            versions,
        })
    }
    pub(crate) fn supports(&self, engine: Engine, version: &str) -> bool {
        !self.evidence_id.is_empty()
            && self
                .versions
                .iter()
                .any(|(candidate, observed)| *candidate == engine && observed == version)
    }
}
pub struct BrowseService {
    metadata: Arc<MetadataCatalog>,
    streams: Arc<ResultStreams>,
    cursors: Mutex<HashMap<Uuid, Arc<Cursor>>>,
    originals: Mutex<HashMap<Uuid, Arc<SourceRow>>>,
    snapshots: Mutex<HashMap<Uuid, Arc<Snapshot>>>,
    certification: Option<SnapshotCertification>,
}
impl BrowseService {
    pub async fn retire_session(
        &self,
        context: &InvocationContext,
        session: SessionRef,
    ) -> Result<(), WorkbenchError> {
        self.retire_scope(context, Some(session)).await
    }
    pub async fn retire_window(&self, context: &InvocationContext) -> Result<(), WorkbenchError> {
        self.retire_scope(context, None).await
    }
    async fn retire_scope(
        &self,
        context: &InvocationContext,
        session: Option<SessionRef>,
    ) -> Result<(), WorkbenchError> {
        if context.principal() != self.metadata.runtime.principal() {
            return Err(WorkbenchError::Forbidden);
        }
        let matches = |scope: &ScopeKey| {
            scope.matches_window(context)
                && session.is_none_or(|session| scope.matches(context, session))
        };
        let snapshots = {
            let snapshots = self.snapshots.lock().map_err(|_| WorkbenchError::Io)?;
            snapshots
                .iter()
                .filter(|(_, snapshot)| matches(&snapshot.scope))
                .map(|(id, snapshot)| (*id, snapshot.lease.session()))
                .collect::<Vec<_>>()
        };
        for (id, session) in snapshots {
            let _ = self.snapshot_end(context, session, id).await;
        }
        self.cursors
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .retain(|_, cursor| !matches(&cursor.scope));
        self.originals
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .retain(|_, row| !matches(&row.scope));
        Ok(())
    }
    pub fn new(metadata: Arc<MetadataCatalog>, streams: Arc<ResultStreams>) -> Self {
        Self {
            metadata,
            streams,
            cursors: Mutex::new(HashMap::new()),
            originals: Mutex::new(HashMap::new()),
            snapshots: Mutex::new(HashMap::new()),
            certification: None,
        }
    }
    pub fn with_snapshot_certification(
        metadata: Arc<MetadataCatalog>,
        streams: Arc<ResultStreams>,
        certification: SnapshotCertification,
    ) -> Self {
        Self {
            certification: Some(certification),
            ..Self::new(metadata, streams)
        }
    }
    pub async fn open(
        &self,
        context: &InvocationContext,
        session: SessionRef,
        input: BrowseInput,
        emitter: Option<Arc<ResultNoticeEmitter>>,
    ) -> Result<BrowseReply, WorkbenchError> {
        validate_limits(&input.limits)?;
        let mut filter_nodes = 0;
        validate_filter_shape(&input.filter, 0, &mut filter_nodes)?;
        encoded_size(&input.filter, 64 * 1024)?;
        let scope = ScopeKey::current(&self.metadata.runtime, context, session)?;
        let snapshot = self.snapshot(context, session, input.snapshot_id)?;
        let relation = self
            .metadata
            .resolve_with_lease(
                context,
                session,
                &input.object,
                snapshot.as_ref().map(|value| value.lease.as_ref()),
            )
            .await?;
        if let Some(snapshot) = &snapshot {
            let checked = relation.clone();
            let budget = self.metadata.budget.clone();
            self.metadata
                .runtime
                .with_lease(
                    context,
                    &snapshot.lease,
                    AuthorizePurpose::Read,
                    move |connection| {
                        Box::pin(async move {
                            Ok(crate::generated_sql::verify_base_origin(
                                connection, &budget, &checked, false,
                            )
                            .await)
                        })
                    },
                )
                .await??;
        }
        let order = complete_order(&relation, &input.order)?;
        let result_set_id = Uuid::new_v4();
        let consistency = snapshot
            .as_ref()
            .map(|value| value.consistency.clone())
            .unwrap_or(Consistency::LivePerPage);
        let stream = if let Some(emitter) = emitter {
            let identity = self.streams.create(
                context,
                session,
                StreamSpec {
                    query_run_id: Uuid::new_v4(),
                    result_set_id,
                    schema_version: relation.object.schema_version.clone(),
                    columns: relation
                        .columns
                        .iter()
                        .map(|value| value.column.clone())
                        .collect(),
                    consistency: consistency.clone(),
                },
            )?;
            self.streams
                .set_notice_emitter(context, &identity, emitter)?;
            self.streams
                .publish_registered(
                    context,
                    &identity,
                    ResultEvent::Schema {
                        columns: relation
                            .columns
                            .iter()
                            .map(|value| value.column.clone())
                            .collect(),
                        consistency: consistency.clone(),
                    },
                )
                .await?;
            Some(identity)
        } else {
            None
        };
        let cursor_memory = self
            .metadata
            .budget
            .reserve(encoded_size(&input.filter, 64 * 1024)? * 4 + 4096)?;
        let cursor = Cursor {
            scope,
            object: input.object,
            filter: input.filter,
            order,
            last: Vec::new(),
            limits: input.limits,
            snapshot_id: input.snapshot_id,
            result_set_id,
            next_row: 0,
            stream: stream.clone(),
            created: Instant::now(),
            _memory: cursor_memory,
        };
        let result = self
            .fetch(
                context,
                session,
                &cursor,
                &relation,
                snapshot.as_deref(),
                cursor.limits.max_rows,
            )
            .await?;
        Ok(BrowseReply {
            result: result.result,
            stream,
            _memory: result._memory,
        })
    }
    pub async fn page(
        &self,
        context: &InvocationContext,
        session: SessionRef,
        cursor: String,
        limit: usize,
    ) -> Result<BrowsePage, WorkbenchError> {
        if limit == 0 || limit > 200 {
            return Err(WorkbenchError::Limit);
        }
        let id = Uuid::parse_str(&cursor).map_err(|_| WorkbenchError::CursorInvalid)?;
        let cursor = self
            .cursors
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .get(&id)
            .cloned()
            .ok_or(WorkbenchError::CursorInvalid)?;
        let scope = ScopeKey::current(&self.metadata.runtime, context, session)?;
        if scope != cursor.scope || cursor.created.elapsed() > Duration::from_secs(120) {
            return Err(WorkbenchError::CursorInvalid);
        }
        let snapshot = self.snapshot(context, session, cursor.snapshot_id)?;
        let relation = match self
            .metadata
            .resolve_with_lease(
                context,
                session,
                &cursor.object,
                snapshot.as_ref().map(|value| value.lease.as_ref()),
            )
            .await
        {
            Ok(value) => value,
            Err(error) => {
                self.retire_result(context, &cursor);
                return Err(error);
            }
        };
        self.fetch(
            context,
            session,
            &cursor,
            &relation,
            snapshot.as_deref(),
            limit.min(cursor.limits.max_rows),
        )
        .await
    }
    async fn fetch(
        &self,
        context: &InvocationContext,
        session: SessionRef,
        cursor: &Cursor,
        relation: &Arc<RelationMetadata>,
        snapshot: Option<&Snapshot>,
        limit: usize,
    ) -> Result<BrowsePage, WorkbenchError> {
        let consistency = snapshot
            .map(|value| value.consistency.clone())
            .unwrap_or(Consistency::LivePerPage);
        // Use canonical catalog columns rather than the driver's sometimes
        // shorter type labels. Cursor UUID and decimal timestamp use their
        // maximum wire lengths; each row is charged including its handle.
        let page_header_bytes = encoded_size(
            &ResultPage {
                page: Page {
                    items: Vec::new(),
                    next_cursor: Some(Uuid::nil().to_string()),
                    completeness: Completeness::Complete,
                    count: Count::Unknown,
                    as_of_ms: u64::MAX.into(),
                },
                result_set_id: cursor.result_set_id,
                columns: relation
                    .columns
                    .iter()
                    .map(|column| column.column.clone())
                    .collect(),
                consistency: consistency.clone(),
                schema_version: relation.object.schema_version.clone(),
            },
            cursor.limits.max_bytes,
        )? + 64;
        let (sql, params) = select_sql(
            relation,
            &cursor.filter,
            &cursor.order,
            &cursor.last,
            limit + 1,
        )?;
        let budget = self.metadata.budget.clone();
        let limits = QueryLimits {
            max_rows: limit + 1,
            // Existing driver row cap is 256KiB. This finite generated LIMIT
            // may drain <=201 such rows without a driver byte-cap skipping a
            // large row and subsequently keeping a smaller one. The sink still
            // retains <=512KiB, including ResultPage wrapper headroom.
            max_bytes: (limit + 1) * 256 * 1024,
            timeout: Duration::from_millis(cursor.limits.statement_timeout_ms),
        };
        let max_bytes = cursor.limits.max_bytes;
        let raw = if let Some(snapshot) = snapshot {
            let checked_relation = relation.clone();
            self.metadata
                .runtime
                .with_lease(
                    context,
                    &snapshot.lease,
                    AuthorizePurpose::Read,
                    move |connection| {
                        Box::pin(async move {
                            crate::generated_sql::verify_base_origin(
                                connection,
                                &budget,
                                &checked_relation,
                                false,
                            )
                            .await?;
                            let mut sink = BoundedRows::prefix(
                                &budget,
                                limit + 1,
                                max_bytes,
                                page_header_bytes,
                            )?;
                            let summary = connection
                                .execute_bound(&sql, &params, limits, &mut sink)
                                .await?;
                            let capped = summary.capped || sink.truncated;
                            Ok((sink, capped))
                        })
                    },
                )
                .await?
        } else {
            self.metadata
                .runtime
                .with_connection(
                    context,
                    session,
                    AuthorizePurpose::Read,
                    move |connection| {
                        Box::pin(async move {
                            let mut sink = BoundedRows::prefix(
                                &budget,
                                limit + 1,
                                max_bytes,
                                page_header_bytes,
                            )?;
                            let summary = connection
                                .execute_bound(&sql, &params, limits, &mut sink)
                                .await?;
                            let capped = summary.capped || sink.truncated;
                            Ok((sink, capped))
                        })
                    },
                )
                .await?
        };
        let (mut raw, byte_capped) = raw;
        let has_more = raw.rows.len() > limit || byte_capped;
        raw.rows.truncate(limit);
        if raw.columns.len() != relation.columns.len()
            || raw
                .columns
                .iter()
                .zip(&relation.columns)
                .any(|(actual, expected)| actual.name != expected.column.name)
        {
            return Err(WorkbenchError::CursorInvalid);
        }
        let key = relation.stable_key();
        let mut pageable = key.is_some() && !cursor.order.is_empty();
        let mut rows = Vec::with_capacity(raw.rows.len());
        for values in &raw.rows {
            let original = relation
                .columns
                .iter()
                .zip(values)
                .map(|(column, value)| OriginalValue {
                    column_id: column.column.column_id,
                    value: value.clone(),
                })
                .collect::<Vec<_>>();
            let key_columns = key.map(|key| key.columns.clone()).unwrap_or_default();
            let key_values = key_columns
                .iter()
                .map(|id| {
                    original
                        .iter()
                        .find(|value| value.column_id == *id)
                        .map(|value| value.value.clone())
                        .ok_or(WorkbenchError::Unsupported)
                })
                .collect::<Result<Vec<_>, _>>()?;
            pageable &= key_values
                .iter()
                .all(|value| pageable_cell(relation.engine, value));
            let handle = Uuid::new_v4();
            let memory = self
                .metadata
                .budget
                .reserve(encoded_size(&original, 512 * 1024)? * 4 + 2048)?;
            let complete_values = values.iter().all(|value| {
                !matches!(
                    value,
                    TypedCell::Large { .. } | TypedCell::Unsupported { .. }
                )
            });
            let record = Arc::new(SourceRow {
                stream: cursor.stream.clone(),
                object: relation.object.clone(),
                result_set_id: cursor.result_set_id,
                session,
                original,
                key_columns,
                key_values,
                single_table: relation.kind == "table",
                complete_values,
                consistency: consistency.clone(),
                scope: cursor.scope.clone(),
                created: Instant::now(),
                _memory: memory,
            });
            let mut originals = self.originals.lock().map_err(|_| WorkbenchError::Io)?;
            originals.retain(|_, row| row.created.elapsed() < Duration::from_secs(120));
            if originals.len() >= 8192 || self.metadata.budget.retained_bytes() > 48 * 1024 * 1024 {
                let mut oldest = originals
                    .iter()
                    .map(|(id, row)| (*id, row.created))
                    .collect::<Vec<_>>();
                oldest.sort_unstable_by_key(|(_, created)| *created);
                for (id, _) in oldest.into_iter().take(1024) {
                    originals.remove(&id);
                }
            }
            originals.insert(handle, record);
            rows.push(Row {
                row_handle: handle,
                cells: values.clone(),
            });
        }
        let last = if pageable {
            if let Some(row) = raw.rows.last() {
                cursor
                    .order
                    .iter()
                    .map(|order| {
                        relation
                            .columns
                            .iter()
                            .position(|column| column.column.column_id == order.column_id)
                            .map(|index| row[index].clone())
                            .ok_or(WorkbenchError::InvalidInput)
                    })
                    .collect::<Result<Vec<_>, _>>()?
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
        pageable &= last
            .iter()
            .all(|value| matches!(value, TypedCell::Null) || pageable_cell(relation.engine, value));
        let next = if has_more && pageable && !last.is_empty() {
            let id = Uuid::new_v4();
            let mut cursors = self.cursors.lock().map_err(|_| WorkbenchError::Io)?;
            cursors.retain(|_, cursor| cursor.created.elapsed() < Duration::from_secs(120));
            if cursors.len() >= 1024 {
                return Err(WorkbenchError::Limit);
            }
            let memory = self.metadata.budget.reserve(
                encoded_size(&last, 256 * 1024)? * 3
                    + encoded_size(&cursor.filter, 128 * 1024)? * 3
                    + 4096,
            )?;
            cursors.insert(
                id,
                Arc::new(Cursor {
                    scope: cursor.scope.clone(),
                    object: cursor.object.clone(),
                    filter: cursor.filter.clone(),
                    order: cursor.order.clone(),
                    last,
                    limits: Limits {
                        max_rows: cursor.limits.max_rows,
                        max_bytes: cursor.limits.max_bytes,
                        statement_timeout_ms: cursor.limits.statement_timeout_ms,
                        lock_wait_ms: cursor.limits.lock_wait_ms,
                    },
                    snapshot_id: cursor.snapshot_id,
                    result_set_id: cursor.result_set_id,
                    next_row: cursor.next_row + rows.len() as u64,
                    stream: cursor.stream.clone(),
                    created: Instant::now(),
                    _memory: memory,
                }),
            );
            Some(id.to_string())
        } else {
            None
        };
        let result = ResultPage {
            page: Page {
                items: rows,
                next_cursor: next,
                completeness: if has_more || !pageable {
                    Completeness::Limited
                } else {
                    Completeness::Complete
                },
                count: Count::Unknown,
                as_of_ms: now_ms().into(),
            },
            result_set_id: cursor.result_set_id,
            columns: relation
                .columns
                .iter()
                .map(|column| column.column.clone())
                .collect(),
            consistency,
            schema_version: relation.object.schema_version.clone(),
        };
        let encoded = encoded_size(&result, 512 * 1024)?;
        let memory = self.metadata.budget.reserve(
            crate::types::rows_memory(&result.page.items, result.page.items.capacity()) * 2
                + crate::types::columns_memory(&result.columns, result.columns.capacity())
                + encoded * 2
                + 4096,
        )?;
        if let Some(identity) = &cursor.stream {
            self.publish_page(context, identity, cursor.next_row, &result)
                .await?;
        }
        Ok(BrowsePage {
            result,
            _memory: memory,
        })
    }
    async fn publish_page(
        &self,
        context: &InvocationContext,
        identity: &StreamIdentity,
        start: u64,
        page: &ResultPage,
    ) -> Result<(), WorkbenchError> {
        let mut batch = Vec::new();
        let mut offset = start;
        for row in &page.page.items {
            let bytes = encoded_size(row, 256 * 1024)?;
            if !batch.is_empty()
                && encoded_size(&batch, 256 * 1024)?.saturating_add(bytes) > 240 * 1024
            {
                let count = batch.len();
                self.streams
                    .publish_registered(
                        context,
                        identity,
                        ResultEvent::Rows {
                            start_row: offset.into(),
                            rows: std::mem::take(&mut batch),
                        },
                    )
                    .await?;
                offset += count as u64;
            }
            batch.push(row.clone());
        }
        if !batch.is_empty() {
            self.streams
                .publish_registered(
                    context,
                    identity,
                    ResultEvent::Rows {
                        start_row: offset.into(),
                        rows: batch,
                    },
                )
                .await?;
        }
        if page.page.next_cursor.is_none() {
            self.streams
                .publish_registered(
                    context,
                    identity,
                    ResultEvent::Terminal {
                        completeness: if page.page.completeness == Completeness::Complete {
                            Completion::Complete
                        } else {
                            Completion::Capped
                        },
                        rows_seen: (start + page.page.items.len() as u64).into(),
                        operation_version: 0.into(),
                        error_code: None,
                    },
                )
                .await?;
        }
        Ok(())
    }
    pub fn original(
        &self,
        context: &InvocationContext,
        session: SessionRef,
        handle: Uuid,
    ) -> Result<Arc<SourceRow>, WorkbenchError> {
        let scope = ScopeKey::current(&self.metadata.runtime, context, session)?;
        let originals = self.originals.lock().map_err(|_| WorkbenchError::Io)?;
        let row = originals
            .get(&handle)
            .ok_or(WorkbenchError::ResultExpired)?;
        if row.scope != scope || row.created.elapsed() > Duration::from_secs(120) {
            return Err(WorkbenchError::ResultExpired);
        }
        if let Some(identity) = &row.stream {
            if !self.streams.is_retained(context, identity)? {
                return Err(WorkbenchError::ResultExpired);
            }
        }
        Ok(row.clone())
    }
    fn retire_result(&self, context: &InvocationContext, cursor: &Cursor) {
        if let Ok(mut rows) = self.originals.lock() {
            rows.retain(|_, row| row.result_set_id != cursor.result_set_id);
        }
        if let Some(identity) = &cursor.stream {
            let _ = self.streams.retire(context, identity);
        }
    }
    fn snapshot(
        &self,
        context: &InvocationContext,
        session: SessionRef,
        id: Option<Uuid>,
    ) -> Result<Option<Arc<Snapshot>>, WorkbenchError> {
        let Some(id) = id else { return Ok(None) };
        let snapshots = self.snapshots.lock().map_err(|_| WorkbenchError::Io)?;
        let snapshot = snapshots.get(&id).ok_or(WorkbenchError::ResultExpired)?;
        if !snapshot.scope.matches(context, session) {
            return Err(WorkbenchError::Forbidden);
        }
        if snapshot.lease.expired() {
            return Err(WorkbenchError::ResultExpired);
        }
        Ok(Some(snapshot.clone()))
    }
    /// Native-only borrowed physical pin. Callers must keep the registry's
    /// scope/expiry semantics and cannot manufacture a snapshot from an ID.
    pub(crate) fn snapshot_lease(
        &self,
        context: &InvocationContext,
        session: SessionRef,
        id: Option<Uuid>,
    ) -> Result<Option<Arc<ConnectionLease>>, WorkbenchError> {
        self.snapshot(context, session, id)
            .map(|snapshot| snapshot.map(|value| value.lease.clone()))
    }
    pub async fn snapshot_begin(
        &self,
        context: &InvocationContext,
        session: SessionRef,
        isolation: &str,
        max_lifetime_ms: u64,
    ) -> Result<(Uuid, u64), WorkbenchError> {
        if max_lifetime_ms == 0
            || max_lifetime_ms > 60_000
            || !matches!(isolation, "repeatable_read" | "serializable")
        {
            return Err(WorkbenchError::Unsupported);
        }
        let scope = ScopeKey::current(&self.metadata.runtime, context, session)?;
        if self.metadata.runtime.get(context, session)?.mode
            != restoreplane_domain::workbench::Mode::ReadOnly
        {
            return Err(WorkbenchError::Unsupported);
        }
        let matrix = self
            .certification
            .as_ref()
            .ok_or(WorkbenchError::Unsupported)?;
        let observed = self
            .metadata
            .runtime
            .with_connection(context, session, AuthorizePurpose::Read, |connection| {
                Box::pin(async move {
                    Ok((
                        connection.facts().engine,
                        connection.facts().server_version.clone(),
                    ))
                })
            })
            .await?;
        if !matrix.supports(observed.0, &observed.1) {
            return Err(WorkbenchError::Unsupported);
        }
        let lease = Arc::new(
            self.metadata
                .runtime
                .pin_connection(
                    context,
                    session,
                    ConnectionLeaseKind::Snapshot,
                    Duration::from_millis(max_lifetime_ms),
                )
                .await?,
        );
        let budget = self.metadata.budget.clone();
        let isolation = isolation.to_owned();
        let result=self.metadata.runtime.with_lease(context,&lease,AuthorizePurpose::Read,move|connection|Box::pin(async move {
            let engine=connection.facts().engine;
            connection.transaction_control(crate::driver::TransactionCommand::SnapshotBegin {serializable:isolation=="serializable"}).await?;
            match engine {
                Engine::Postgresql=>{let row=crate::types::read(connection,&budget,"SELECT pg_catalog.current_setting('transaction_read_only'),pg_catalog.current_setting('transaction_isolation')",&[],1).await?;if crate::types::text(&row.rows[0][0])?!="on" || crate::types::text(&row.rows[0][1])?!=isolation.replace('_'," "){return Err(WorkbenchError::Unsupported);}},
                Engine::Sqlite=>{crate::types::read(connection,&budget,"SELECT name FROM sqlite_schema LIMIT 1",&[],1).await?;},
                _=>{let row=crate::types::read(connection,&budget,if engine==Engine::Mariadb {"SELECT @@session.tx_read_only"}else{"SELECT @@session.transaction_read_only"},&[],1).await?;if !crate::types::boolean(&row.rows[0][0])? {return Err(WorkbenchError::Unsupported);}}
            }Ok(())
        })).await;
        if let Err(error) = result {
            let _ = self
                .metadata
                .runtime
                .finish_lease(context, &lease, false)
                .await;
            return Err(error);
        }
        let id = lease.id();
        let expires = lease.expires_at_ms();
        let snapshot = Arc::new(Snapshot {
            scope,
            lease,
            consistency: Consistency::TransactionSnapshot {
                snapshot_id: id,
                expires_at_ms: expires.into(),
            },
        });
        let mut snapshots = self.snapshots.lock().map_err(|_| WorkbenchError::Io)?;
        if snapshots.len() >= 8 {
            return Err(WorkbenchError::Limit);
        }
        snapshots.insert(id, snapshot);
        Ok((id, expires))
    }
    pub async fn snapshot_end(
        &self,
        context: &InvocationContext,
        session: SessionRef,
        id: Uuid,
    ) -> Result<bool, WorkbenchError> {
        let snapshot = {
            let mut snapshots = self.snapshots.lock().map_err(|_| WorkbenchError::Io)?;
            let Some(snapshot) = snapshots.get(&id) else {
                return Ok(false);
            };
            if !snapshot.scope.matches(context, session) {
                return Err(WorkbenchError::Forbidden);
            }
            snapshots.remove(&id).ok_or(WorkbenchError::ResultExpired)?
        };
        let settled = self
            .metadata
            .runtime
            .with_lease_cleanup(context, &snapshot.lease, move |connection| {
                Box::pin(async move {
                    connection
                        .transaction_control(crate::driver::TransactionCommand::Rollback)
                        .await
                })
            })
            .await;
        self.metadata
            .runtime
            .finish_lease(context, &snapshot.lease, settled.is_ok())
            .await?;
        settled?;
        Ok(true)
    }
    pub async fn expire_snapshots(&self) -> Result<usize, WorkbenchError> {
        let expired = {
            let snapshots = self.snapshots.lock().map_err(|_| WorkbenchError::Io)?;
            snapshots
                .iter()
                .filter(|(_, snapshot)| snapshot.lease.expired())
                .map(|(id, snapshot)| (*id, snapshot.lease.session(), snapshot.scope.clone()))
                .collect::<Vec<_>>()
        };
        for (id, session, scope) in &expired {
            let context = InvocationContext::new(
                self.metadata.runtime.principal().clone(),
                scope.window_id(),
            );
            let _ = self.snapshot_end(&context, *session, *id).await;
        }
        Ok(expired.len())
    }
}
fn validate_limits(limits: &Limits) -> Result<(), WorkbenchError> {
    if limits.max_rows == 0
        || limits.max_rows > 200
        || limits.max_bytes == 0
        || limits.max_bytes > 512 * 1024
        || limits.statement_timeout_ms == 0
        || limits.statement_timeout_ms > 30_000
        || limits.lock_wait_ms != 2000
    {
        return Err(WorkbenchError::Limit);
    }
    Ok(())
}
fn validate_filter_shape(
    filter: &Filter,
    depth: usize,
    nodes: &mut usize,
) -> Result<(), WorkbenchError> {
    *nodes += 1;
    if depth > 8 || *nodes > 64 {
        return Err(WorkbenchError::Limit);
    }
    match filter {
        Filter::And { items } | Filter::Or { items } => {
            if items.len() > 32 {
                return Err(WorkbenchError::Limit);
            }
            for item in items {
                validate_filter_shape(item, depth + 1, nodes)?;
            }
        }
        Filter::Compare {
            value: Some(value), ..
        } => validate_cells(std::slice::from_ref(value))?,
        _ => {}
    }
    Ok(())
}
fn complete_order(
    relation: &RelationMetadata,
    requested: &[Order],
) -> Result<Vec<Order>, WorkbenchError> {
    if requested.len() > 16 {
        return Err(WorkbenchError::Limit);
    }
    for order in requested {
        if !relation
            .columns
            .iter()
            .any(|column| column.column.column_id == order.column_id)
        {
            return Err(WorkbenchError::InvalidInput);
        }
    }
    let mut order = requested.to_vec();
    if let Some(key) = relation.stable_key() {
        for id in &key.columns {
            if !order.iter().any(|order| order.column_id == *id) {
                order.push(Order {
                    column_id: *id,
                    direction: Direction::Asc,
                    nulls: NullOrder::Last,
                });
            }
        }
    }
    if order.len() > 32 {
        return Err(WorkbenchError::Limit);
    }
    Ok(order)
}
fn select_sql(
    relation: &RelationMetadata,
    filter: &Filter,
    order: &[Order],
    last: &[TypedCell],
    limit: usize,
) -> Result<(String, Vec<TypedCell>), WorkbenchError> {
    let mut params = Vec::new();
    let mut nodes = 0;
    let condition = filter_sql(relation, filter, &mut params, 0, &mut nodes)?;
    let mut conditions = vec![condition];
    if !last.is_empty() {
        if last.len() != order.len() {
            return Err(WorkbenchError::CursorInvalid);
        }
        let mut branches = Vec::new();
        for (index, item) in order.iter().enumerate() {
            let mut branch = Vec::new();
            for previous in 0..index {
                let name = column_sql(relation, order[previous].column_id)?;
                if matches!(last[previous], TypedCell::Null) {
                    branch.push(format!("{name} IS NULL"));
                } else {
                    params.push(last[previous].clone());
                    branch.push(format!(
                        "{name}={}",
                        parameter_sql(
                            relation.engine,
                            params.len(),
                            params.last().ok_or(WorkbenchError::InvalidInput)?
                        )?
                    ));
                }
            }
            let name = column_sql(relation, item.column_id)?;
            let after = if matches!(last[index], TypedCell::Null) {
                if item.nulls == NullOrder::First {
                    format!("{name} IS NOT NULL")
                } else {
                    "0=1".into()
                }
            } else {
                params.push(last[index].clone());
                let p = parameter_sql(
                    relation.engine,
                    params.len(),
                    params.last().ok_or(WorkbenchError::InvalidInput)?,
                )?;
                let comparison = if item.direction == Direction::Asc {
                    ">"
                } else {
                    "<"
                };
                let comparison = format!("({name} IS NOT NULL AND {name}{comparison}{p})");
                if item.nulls == NullOrder::Last {
                    format!("({name} IS NULL OR {comparison})")
                } else {
                    comparison
                }
            };
            branch.push(after);
            branches.push(format!("({})", branch.join(" AND ")));
        }
        conditions.push(format!("({})", branches.join(" OR ")));
    }
    let columns = relation
        .columns
        .iter()
        .map(|column| quote_identifier(relation.engine, &column.column.name))
        .collect::<Result<Vec<_>, _>>()?
        .join(",");
    let ordering = order
        .iter()
        .map(|item| {
            let name = column_sql(relation, item.column_id)?;
            let null = if item.nulls == NullOrder::First { 0 } else { 1 };
            let nonnull = 1 - null;
            Ok(format!(
                "CASE WHEN {name} IS NULL THEN {null} ELSE {nonnull} END ASC,{name} {}",
                if item.direction == Direction::Asc {
                    "ASC"
                } else {
                    "DESC"
                }
            ))
        })
        .collect::<Result<Vec<_>, WorkbenchError>>()?
        .join(",");
    Ok((
        format!(
            "SELECT {columns} FROM {} WHERE {}{} LIMIT {limit}",
            relation.qualified_name()?,
            conditions.join(" AND "),
            if ordering.is_empty() {
                String::new()
            } else {
                format!(" ORDER BY {ordering}")
            }
        ),
        params,
    ))
}
fn column_sql(relation: &RelationMetadata, id: Uuid) -> Result<String, WorkbenchError> {
    let column = relation
        .columns
        .iter()
        .find(|column| column.column.column_id == id)
        .ok_or(WorkbenchError::InvalidInput)?;
    quote_identifier(relation.engine, &column.column.name)
}
fn filter_sql(
    relation: &RelationMetadata,
    filter: &Filter,
    params: &mut Vec<TypedCell>,
    depth: usize,
    nodes: &mut usize,
) -> Result<String, WorkbenchError> {
    *nodes += 1;
    if depth > 8 || *nodes > 64 {
        return Err(WorkbenchError::Limit);
    }
    match filter {
        Filter::And { items } | Filter::Or { items } => {
            if items.len() > 32 {
                return Err(WorkbenchError::Limit);
            }
            if items.is_empty() {
                return Ok(if matches!(filter, Filter::And { .. }) {
                    "1=1"
                } else {
                    "0=1"
                }
                .into());
            }
            let parts = items
                .iter()
                .map(|item| filter_sql(relation, item, params, depth + 1, nodes))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(format!(
                "({})",
                parts.join(if matches!(filter, Filter::And { .. }) {
                    " AND "
                } else {
                    " OR "
                })
            ))
        }
        Filter::Compare {
            column_id,
            operator,
            value,
        } => {
            let column = column_sql(relation, *column_id)?;
            if matches!(operator, Comparison::IsNull | Comparison::IsNotNull) {
                if value.is_some() {
                    return Err(WorkbenchError::InvalidInput);
                }
                return Ok(format!(
                    "{column} IS {}NULL",
                    if *operator == Comparison::IsNotNull {
                        "NOT "
                    } else {
                        ""
                    }
                ));
            }
            let value = value.as_ref().ok_or(WorkbenchError::InvalidInput)?;
            validate_cells(std::slice::from_ref(value))?;
            if matches!(
                value,
                TypedCell::Null | TypedCell::Large { .. } | TypedCell::Unsupported { .. }
            ) {
                return Err(WorkbenchError::Unsupported);
            }
            let value = if *operator == Comparison::Contains {
                let TypedCell::Text { value } = value else {
                    return Err(WorkbenchError::Unsupported);
                };
                TypedCell::Text {
                    value: format!(
                        "%{}%",
                        value
                            .replace('!', "!!")
                            .replace('%', "!%")
                            .replace('_', "!_")
                    ),
                }
            } else {
                value.clone()
            };
            params.push(value);
            let p = parameter_sql(
                relation.engine,
                params.len(),
                params.last().ok_or(WorkbenchError::InvalidInput)?,
            )?;
            let op = match operator {
                Comparison::Eq => "=",
                Comparison::Ne => "<>",
                Comparison::Lt => "<",
                Comparison::Le => "<=",
                Comparison::Gt => ">",
                Comparison::Ge => ">=",
                Comparison::Contains => "LIKE",
                _ => return Err(WorkbenchError::InvalidInput),
            };
            Ok(format!(
                "{column} {op} {p}{}",
                if *operator == Comparison::Contains {
                    " ESCAPE '!'"
                } else {
                    ""
                }
            ))
        }
    }
}

fn parameter_sql(engine: Engine, index: usize, cell: &TypedCell) -> Result<String, WorkbenchError> {
    let p = placeholder(engine, index);
    if engine == Engine::Postgresql
        && matches!(cell,TypedCell::Decimal {value,..} if value.len()>16*1024)
    {
        return Err(WorkbenchError::Unsupported);
    }
    if matches!(engine, Engine::Mysql | Engine::Mariadb) {
        if let TypedCell::Decimal { value, scale } = cell {
            crate::types::validate_decimal(value, *scale)?;
            let unsigned = value.strip_prefix('-').unwrap_or(value);
            let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
            if fraction.len() > 30 || whole.trim_start_matches('0').len() + fraction.len() > 65 {
                return Err(WorkbenchError::Unsupported);
            }
            return Ok(format!("CAST({p} AS DECIMAL(65,{}))", fraction.len()));
        }
    }
    Ok(p)
}
fn pageable_cell(engine: Engine, cell: &TypedCell) -> bool {
    exact_key_cell(cell) && parameter_sql(engine, 1, cell).is_ok()
}
