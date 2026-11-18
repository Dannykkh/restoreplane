//! Exact reviewed bytes, native admission and one physical execution barrier.
mod export_read;
use crate::driver::{
    DriverConnection, DriverFacts, DriverTransactionState, QueryLimits, RowSink,
    TransactionCommand, TransactionIsolation,
};
use crate::journal::{AdmissionSpec, OperationJournal, StatementSpec, StatementUpdate, Transition};
use crate::policy::{AnalyzedScript, PolicyContext, SqlPolicy, VerifiedLexicalSettings};
use crate::result_stream::{encoded_size, MemoryReservation, ResultStreams, StreamSpec};
use crate::session::{
    ConnectionLease, ConnectionLeaseKind, ExecutionReservation, RestoredSessionCheck,
};
use crate::types::now_ms;
use crate::{InvocationContext, WorkbenchError, WorkbenchState};
pub use export_read::{ApprovedExportRead, ExportReadOutcome};
use restoreplane_domain::workbench::*;
use restoreplane_domain::workbench_metadata::{
    Completeness, Consistency, Count, Limits, Page, Row,
};
use restoreplane_domain::workbench_query::*;
use restoreplane_domain::workbench_stream::{Completion, ResultEvent, StreamIdentity};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};
use uuid::Uuid;
use zeroize::Zeroizing;

struct Review {
    owner: String,
    window: Uuid,
    session: SessionRef,
    scope: Scope,
    receipt: BindingReceipt,
    mode: Mode,
    manual_read_only: bool,
    sql: Zeroizing<String>,
    policy: AnalyzedScript,
    lexical: VerifiedLexicalSettings,
    session_basis: String,
    facts: DriverFacts,
    purpose: QueryPurpose,
    manual: Option<Uuid>,
    public: QueryReview,
    created: Instant,
    consumed: AtomicBool,
    _memory: MemoryReservation,
}

struct QuerySink {
    streams: Arc<ResultStreams>,
    context: InvocationContext,
    run: Arc<Run>,
    identity: Option<StreamIdentity>,
    rows: usize,
    bytes: usize,
    max_rows: usize,
    max_bytes: usize,
    capped: bool,
    operation_version: u64,
    next_row: u64,
    batch: Vec<Row>,
    batch_memory: Vec<MemoryReservation>,
    batch_bytes: usize,
}
impl QuerySink {
    fn new(
        _runtime: Arc<WorkbenchState>,
        streams: Arc<ResultStreams>,
        context: InvocationContext,
        run: Arc<Run>,
        max_rows: usize,
        max_bytes: usize,
        operation_version: u64,
    ) -> Self {
        Self {
            streams,
            context,
            run,
            identity: None,
            rows: 0,
            bytes: 0,
            max_rows,
            max_bytes,
            capped: false,
            operation_version,
            next_row: 0,
            batch: Vec::new(),
            batch_memory: Vec::new(),
            batch_bytes: 0,
        }
    }
    async fn flush(&mut self) -> Result<(), WorkbenchError> {
        if self.batch.is_empty() {
            return Ok(());
        }
        let identity = self.identity.as_ref().ok_or(WorkbenchError::InvalidInput)?;
        let rows = std::mem::take(&mut self.batch);
        let count = rows.len();
        self.streams
            .publish_registered(
                &self.context,
                identity,
                ResultEvent::Rows {
                    start_row: self.next_row.into(),
                    rows,
                },
            )
            .await?;
        self.next_row += count as u64;
        self.batch_memory.clear();
        self.batch_bytes = 0;
        Ok(())
    }
    async fn finish(
        &mut self,
        completeness: Completion,
        version: u64,
        error: Option<String>,
    ) -> Result<(), WorkbenchError> {
        self.flush().await?;
        if let Some(identity) = self.identity.take() {
            self.streams
                .publish_registered(
                    &self.context,
                    &identity,
                    ResultEvent::Terminal {
                        completeness,
                        rows_seen: self.next_row.into(),
                        operation_version: version.into(),
                        error_code: error,
                    },
                )
                .await?;
            if let Some(result) = self
                .run
                .results
                .lock()
                .map_err(|_| WorkbenchError::Io)?
                .iter_mut()
                .find(|result| result.identity == identity)
            {
                result.state = QueryResultState::Terminal;
                result.completeness = Some(completeness);
            }
        }
        Ok(())
    }
}
#[async_trait::async_trait]
impl RowSink for QuerySink {
    async fn columns(&mut self, columns: Vec<Column>) -> Result<(), WorkbenchError> {
        self.finish(
            if self.capped {
                Completion::Capped
            } else {
                Completion::Complete
            },
            self.operation_version,
            None,
        )
        .await?;
        if columns.is_empty() {
            return Ok(());
        }
        if columns.len() > 512 {
            return Err(WorkbenchError::Limit);
        }
        if self
            .run
            .results
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .len()
            >= 64
        {
            return Err(WorkbenchError::Limit);
        }
        encoded_size(&columns, 255 * 1024)?;
        let schema_version =
            blake3::hash(&serde_json::to_vec(&columns).map_err(|_| WorkbenchError::Io)?)
                .to_hex()
                .to_string();
        let identity = self.streams.create(
            &self.context,
            self.run.session,
            StreamSpec {
                query_run_id: self.run.id,
                result_set_id: Uuid::new_v4(),
                schema_version,
                columns: columns.clone(),
                consistency: Consistency::LivePerPage,
            },
        )?;
        self.streams
            .publish_registered(
                &self.context,
                &identity,
                ResultEvent::Schema {
                    columns,
                    consistency: Consistency::LivePerPage,
                },
            )
            .await?;
        self.run
            .results
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .push(QueryResult {
                identity: identity.clone(),
                state: QueryResultState::Streaming,
                completeness: None,
            });
        self.identity = Some(identity);
        self.next_row = 0;
        Ok(())
    }
    async fn row(&mut self, cells: Vec<TypedCell>) -> Result<(), WorkbenchError> {
        crate::types::validate_cells(&cells)?;
        if self.capped {
            return Ok(());
        }
        let row = Row {
            row_handle: Uuid::new_v4(),
            cells,
        };
        let bytes = encoded_size(&row, 240 * 1024)?;
        if self.rows >= self.max_rows || self.bytes.saturating_add(bytes) > self.max_bytes {
            self.capped = true;
            return Ok(());
        }
        if self.identity.is_none() {
            return Err(WorkbenchError::Unsupported);
        }
        if !self.batch.is_empty()
            && (self.batch.len() >= 200 || self.batch_bytes + bytes > 220 * 1024)
        {
            self.flush().await?;
        }
        let reservation = self
            .streams
            .budget()
            .reserve(crate::types::cells_memory(&row.cells, row.cells.capacity()) + bytes + 1024)?;
        self.rows += 1;
        self.bytes += bytes;
        self.batch_bytes += bytes;
        self.batch.push(row);
        self.batch_memory.push(reservation);
        Ok(())
    }
}
fn native_purpose(purpose: QueryPurpose) -> crate::policy::QueryPurpose {
    match purpose {
        QueryPurpose::Run => crate::policy::QueryPurpose::Run,
        QueryPurpose::ExplainPlan => crate::policy::QueryPurpose::ExplainPlan,
        QueryPurpose::ExplainAnalyze => crate::policy::QueryPurpose::ExplainAnalyze,
    }
}
fn purpose_for(review: &Review) -> AuthorizePurpose {
    if review.mode == Mode::ReadOnly || review.manual_read_only {
        AuthorizePurpose::Read
    } else {
        AuthorizePurpose::Mutate
    }
}
fn same_basis(review: &BindingReceipt, fresh: &BindingReceipt) -> Result<(), WorkbenchError> {
    if review.scope != fresh.scope
        || review.descriptor != fresh.descriptor
        || review.descriptor_revision != fresh.descriptor_revision
        || review.authority_epoch != fresh.authority_epoch
    {
        return Err(WorkbenchError::BindingStale);
    }
    Ok(())
}
fn wire_digest(input: &QueryRunInput) -> Result<String, WorkbenchError> {
    let mut input = input.clone();
    input.confirmed_warnings.sort();
    input.confirmed_warnings.dedup();
    Ok(
        blake3::hash(&serde_json::to_vec(&input).map_err(|_| WorkbenchError::InvalidInput)?)
            .to_hex()
            .to_string(),
    )
}
fn public_query_payload(input: &QueryRunInput) -> Result<Value, WorkbenchError> {
    let mut input = input.clone();
    input.confirmed_warnings.sort();
    input.confirmed_warnings.dedup();
    serde_json::to_value(input).map_err(|_| WorkbenchError::InvalidInput)
}
fn resolved_query(review: &Review, input: &QueryRunInput) -> Result<Value, WorkbenchError> {
    let statements=review.policy.statements.iter().map(|statement|json!({"digest":statement.digest,"start":statement.source_range.start,"end":statement.source_range.end,"effect":statement.effect,"boundaryVerified":statement.boundary_verified})).collect::<Vec<_>>();
    let mut warnings = input.confirmed_warnings.clone();
    warnings.sort();
    warnings.dedup();
    Ok(
        json!({"session":input.session,"reviewId":input.review_id,"sourceDigest":review.policy.digest,"purpose":review.purpose,"mode":review.mode,"scope":review.scope,"descriptorRevision":review.receipt.descriptor_revision,"authorityEpoch":review.receipt.authority_epoch,"engine":review.facts.engine,"serverVersion":review.facts.server_version,"serverIdentity":review.facts.server_identity,"actualContext":review.facts.context,"lexicalSettings":lexical_value(&review.lexical),"sessionBasis":review.session_basis,"transactionId":review.manual,"statements":statements,"confirmedWarnings":warnings,"limits":input.limits}),
    )
}
fn lexical_value(settings: &VerifiedLexicalSettings) -> Value {
    match settings {
        VerifiedLexicalSettings::Unknown => json!({"kind":"unknown"}),
        VerifiedLexicalSettings::Sqlite => json!({"kind":"sqlite"}),
        VerifiedLexicalSettings::Postgresql {
            standard_conforming_strings,
            client_encoding,
        } => {
            json!({"kind":"postgresql","standardConformingStrings":standard_conforming_strings,"clientEncoding":client_encoding})
        }
        VerifiedLexicalSettings::Mysql {
            ansi_quotes,
            no_backslash_escapes,
            character_set_client,
        } => {
            json!({"kind":"mysql","ansiQuotes":ansi_quotes,"noBackslashEscapes":no_backslash_escapes,"characterSetClient":character_set_client})
        }
    }
}
async fn lexical_settings(
    connection: &mut dyn DriverConnection,
    budget: &Arc<crate::result_stream::NativeBudget>,
) -> Result<VerifiedLexicalSettings, WorkbenchError> {
    match connection.facts().engine {
        Engine::Sqlite => Ok(VerifiedLexicalSettings::Sqlite),
        Engine::Postgresql => {
            let rows=crate::types::read(connection,budget,"SELECT pg_catalog.current_setting('standard_conforming_strings'),pg_catalog.current_setting('client_encoding')",&[],1).await?;
            let row = rows.rows.first().ok_or(WorkbenchError::Unsupported)?;
            Ok(VerifiedLexicalSettings::Postgresql {
                standard_conforming_strings: crate::types::text(&row[0])? == "on",
                client_encoding: crate::types::text(&row[1])?.into(),
            })
        }
        _ => {
            let rows = crate::types::read(
                connection,
                budget,
                "SELECT @@session.sql_mode,@@session.character_set_client",
                &[],
                1,
            )
            .await?;
            let row = rows.rows.first().ok_or(WorkbenchError::Unsupported)?;
            let mode = crate::types::text(&row[0])?;
            Ok(VerifiedLexicalSettings::Mysql {
                ansi_quotes: mode.split(',').any(|value| value == "ANSI_QUOTES"),
                no_backslash_escapes: mode.split(',').any(|value| value == "NO_BACKSLASH_ESCAPES"),
                character_set_client: crate::types::text(&row[1])?.into(),
            })
        }
    }
}
async fn native_session_basis(
    connection: &mut dyn DriverConnection,
    budget: &Arc<crate::result_stream::NativeBudget>,
) -> Result<String, WorkbenchError> {
    // Fully qualified observation only. The catalog helper deliberately changes
    // search_path temporarily, so observe this setting without that guard.
    let sql=match connection.facts().engine {
        Engine::Postgresql=>"SELECT pg_catalog.current_database()::pg_catalog.text,current_user::pg_catalog.text,pg_catalog.current_setting('search_path'),pg_catalog.current_setting('TimeZone'),pg_catalog.current_setting('DateStyle'),pg_catalog.current_setting('default_transaction_read_only')",
        Engine::Mysql=>"SELECT DATABASE(),CURRENT_USER(),@@session.time_zone,@@session.autocommit,@@session.transaction_read_only",
        Engine::Mariadb=>"SELECT DATABASE(),CURRENT_USER(),@@session.time_zone,@@session.autocommit,@@session.tx_read_only",
        Engine::Sqlite=>"PRAGMA query_only",
    };
    let rows = crate::types::read_unscoped(connection, budget, sql, &[], 1).await?;
    if rows.rows.len() != 1 {
        return Err(WorkbenchError::Unsupported);
    }
    Ok(
        blake3::hash(&serde_json::to_vec(&rows.rows[0]).map_err(|_| WorkbenchError::Io)?)
            .to_hex()
            .to_string(),
    )
}
async fn observed_isolation(
    connection: &mut dyn DriverConnection,
    budget: &Arc<crate::result_stream::NativeBudget>,
) -> Result<String, WorkbenchError> {
    let sql = match connection.facts().engine {
        Engine::Sqlite => return Ok("serializable".into()),
        Engine::Postgresql => "SELECT pg_catalog.current_setting('transaction_isolation')",
        Engine::Mysql => "SELECT @@session.transaction_isolation",
        Engine::Mariadb => "SELECT @@session.tx_isolation",
    };
    let result = crate::types::read(connection, budget, sql, &[], 1).await?;
    Ok(crate::types::text(&result.rows[0][0])?
        .to_ascii_lowercase()
        .replace([' ', '-'], "_"))
}
async fn verify_manual_effects(
    connection: &mut dyn DriverConnection,
    budget: &Arc<crate::result_stream::NativeBudget>,
    statement: &crate::policy::StatementPolicy,
) -> Result<(), WorkbenchError> {
    if matches!(
        statement.effect,
        EffectClass::Ddl | EffectClass::Unknown | EffectClass::Transaction
    ) {
        return Err(WorkbenchError::Unsupported);
    }
    if !matches!(connection.facts().engine, Engine::Mysql | Engine::Mariadb) {
        return Ok(());
    }
    if statement.has_function_calls {
        return Err(WorkbenchError::Unsupported);
    }
    let database = connection.facts().context.database.clone();
    let mut names = Vec::new();
    for relation in &statement.relations {
        let name = match relation.as_slice() {
            [name] => name,
            [schema, name] if schema == &database => name,
            _ => return Err(WorkbenchError::Unsupported),
        };
        if name.is_empty() || name.len() > 256 {
            return Err(WorkbenchError::Unsupported);
        }
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    if names.is_empty() {
        return if statement.effect == EffectClass::Read {
            Ok(())
        } else {
            Err(WorkbenchError::Unsupported)
        };
    }
    if names.len() > 64 {
        return Err(WorkbenchError::Limit);
    }
    for name in &names {
        crate::generated_sql::verify_mysql_metadata_visibility(connection, budget, &database, name)
            .await?;
    }
    let mut parameters = vec![TypedCell::Text {
        value: database.clone(),
    }];
    parameters.extend(names.iter().map(|name| TypedCell::Text {
        value: name.clone(),
    }));
    let placeholders = vec!["?"; names.len()].join(",");
    let default_guard = if connection.facts().engine == Engine::Mariadb {
        " OR LOCATE('(',COALESCE(c.COLUMN_DEFAULT,''))>0"
    } else {
        ""
    };
    let sql=format!("SELECT t.TABLE_NAME,t.TABLE_TYPE,t.ENGINE,(SELECT COUNT(*) FROM information_schema.TRIGGERS g WHERE BINARY g.EVENT_OBJECT_SCHEMA=BINARY t.TABLE_SCHEMA AND BINARY g.EVENT_OBJECT_TABLE=BINARY t.TABLE_NAME),(SELECT COUNT(*) FROM information_schema.COLUMNS c WHERE BINARY c.TABLE_SCHEMA=BINARY t.TABLE_SCHEMA AND BINARY c.TABLE_NAME=BINARY t.TABLE_NAME AND (COALESCE(c.GENERATION_EXPRESSION,'')<>'' OR c.EXTRA LIKE '%DEFAULT_GENERATED%'{default_guard})) FROM information_schema.TABLES t WHERE BINARY t.TABLE_SCHEMA=BINARY ? AND BINARY t.TABLE_NAME IN({placeholders}) LIMIT 65");
    let result = crate::types::read(connection, budget, &sql, &parameters, 65).await?;
    if result.rows.len() != names.len() {
        return Err(WorkbenchError::Unsupported);
    }
    for row in &result.rows {
        if crate::types::text(&row[1])? != "BASE TABLE"
            || !crate::types::text(&row[2])?.eq_ignore_ascii_case("InnoDB")
            || crate::types::integer(&row[3])? != 0
            || crate::types::integer(&row[4])? != 0
        {
            return Err(WorkbenchError::Unsupported);
        }
    }
    Ok(())
}
fn check_run(
    context: &InvocationContext,
    session: SessionRef,
    run: &Run,
) -> Result<(), WorkbenchError> {
    if run.owner != context.principal().owner_id()
        || run.window != context.window_instance_id()
        || run.session != session
    {
        return Err(WorkbenchError::Forbidden);
    }
    Ok(())
}
fn validate_limits(limits: &Limits) -> Result<(), WorkbenchError> {
    QueryLimits {
        max_rows: limits.max_rows,
        max_bytes: limits.max_bytes,
        timeout: Duration::from_millis(limits.statement_timeout_ms),
    }
    .validate()?;
    if limits.lock_wait_ms == 0 || limits.lock_wait_ms > 30_000 {
        return Err(WorkbenchError::Limit);
    }
    Ok(())
}
struct PreparedRun {
    review: Arc<Review>,
    resolved: Value,
    wire_digest: String,
    created: Instant,
}
struct Run {
    owner: String,
    window: Uuid,
    session: SessionRef,
    scope: Scope,
    id: Uuid,
    operation_id: Uuid,
    reservation: Arc<ExecutionReservation>,
    results: Mutex<Vec<QueryResult>>,
    finished: AtomicBool,
    cancel_pending: AtomicBool,
    created: Instant,
    credential: Uuid,
    mode: Mode,
    restored_check: Option<RestoredSessionCheck>,
    _memory: MemoryReservation,
}
struct Manual {
    owner: String,
    window: Uuid,
    scope: Scope,
    lease: Arc<ConnectionLease>,
    transaction: Mutex<Transaction>,
    finalizing: AtomicBool,
    created: Instant,
    credential: Uuid,
    mode: Mode,
    restored_check: Option<RestoredSessionCheck>,
}
struct PendingCancel(Arc<Run>);
impl Drop for PendingCancel {
    fn drop(&mut self) {
        self.0.cancel_pending.store(false, Ordering::SeqCst);
    }
}
struct ResultsCursor {
    run: Uuid,
    window: Uuid,
    index: usize,
    created: Instant,
}
pub struct QueryCoordinator {
    runtime: Arc<WorkbenchState>,
    journal: Arc<OperationJournal>,
    streams: Arc<ResultStreams>,
    reviews: Mutex<HashMap<Uuid, Arc<Review>>>,
    prepared: Mutex<HashMap<String, Arc<PreparedRun>>>,
    runs: Mutex<HashMap<Uuid, Arc<Run>>>,
    transactions: Mutex<HashMap<Uuid, Arc<Manual>>>,
    cursors: Mutex<HashMap<Uuid, ResultsCursor>>,
    idle_timeout: Duration,
    admission_slots: Arc<tokio::sync::Semaphore>,
    admission_lock: tokio::sync::Mutex<()>,
    run_capacity: usize,
}
impl QueryCoordinator {
    pub fn new(
        runtime: Arc<WorkbenchState>,
        journal: Arc<OperationJournal>,
        streams: Arc<ResultStreams>,
    ) -> Self {
        Self {
            runtime,
            journal,
            streams,
            reviews: Mutex::new(HashMap::new()),
            prepared: Mutex::new(HashMap::new()),
            runs: Mutex::new(HashMap::new()),
            transactions: Mutex::new(HashMap::new()),
            cursors: Mutex::new(HashMap::new()),
            idle_timeout: Duration::from_secs(30),
            admission_slots: Arc::new(tokio::sync::Semaphore::new(32)),
            admission_lock: tokio::sync::Mutex::new(()),
            run_capacity: 64,
        }
    }
    pub fn with_idle_timeout(mut self, lifetime: Duration) -> Result<Self, WorkbenchError> {
        if lifetime.is_zero() || lifetime > Duration::from_secs(30) {
            return Err(WorkbenchError::Limit);
        }
        self.idle_timeout = lifetime;
        Ok(self)
    }
    /// Native test/composition may lower the cache bound; it can never exceed64.
    pub fn with_run_cache_capacity(mut self, capacity: usize) -> Result<Self, WorkbenchError> {
        if capacity == 0 || capacity > 64 {
            return Err(WorkbenchError::Limit);
        }
        self.run_capacity = capacity;
        Ok(self)
    }
    fn reserve_run_capacity(&self, context: &InvocationContext) -> Result<(), WorkbenchError> {
        let mut runs = self.runs.lock().map_err(|_| WorkbenchError::Io)?;
        if runs.len() < self.run_capacity {
            return Ok(());
        }
        let candidate = runs
            .iter()
            .filter(|(_, run)| {
                run.finished.load(Ordering::SeqCst) && !run.cancel_pending.load(Ordering::SeqCst)
            })
            .filter(|(_, run)| {
                let native = InvocationContext::new(context.principal().clone(), run.window);
                run.results.lock().is_ok_and(|results| {
                    results.iter().all(|result| {
                        self.streams
                            .cache_evictable(&native, &result.identity)
                            .unwrap_or(false)
                    })
                })
            })
            .min_by_key(|(_, run)| run.created)
            .map(|(id, _)| *id);
        if let Some(id) = candidate {
            let run = runs.remove(&id).ok_or(WorkbenchError::Io)?;
            let native = InvocationContext::new(context.principal().clone(), run.window);
            if let Ok(results) = run.results.lock() {
                for result in results.iter() {
                    let _ = self.streams.retire(&native, &result.identity);
                }
            };
        }
        if runs.len() >= self.run_capacity {
            Err(WorkbenchError::Limit)
        } else {
            Ok(())
        }
    }
    fn context(&self, context: &InvocationContext) -> Result<(), WorkbenchError> {
        context.principal().ensure_standard()?;
        if context.principal() != self.runtime.principal() {
            return Err(WorkbenchError::Forbidden);
        }
        Ok(())
    }
    fn manual(
        &self,
        context: &InvocationContext,
        session: SessionRef,
    ) -> Result<Option<Arc<Manual>>, WorkbenchError> {
        self.context(context)?;
        let transactions = self.transactions.lock().map_err(|_| WorkbenchError::Io)?;
        let found = transactions.values().find(|tx| {
            tx.window == context.window_instance_id()
                && tx.lease.session() == session
                && tx.owner == context.principal().owner_id()
                && matches!(
                    tx.transaction.lock().map(|tx| tx.state),
                    Ok(TransactionState::Active
                        | TransactionState::Opening
                        | TransactionState::FailedNeedsRollback
                        | TransactionState::CommitRequested
                        | TransactionState::OutcomeUnknown)
                )
        });
        Ok(found.cloned())
    }
    pub async fn review(
        self: &Arc<Self>,
        context: &InvocationContext,
        input: QueryReviewInput,
    ) -> Result<QueryReview, WorkbenchError> {
        self.context(context)?;
        if input.sql.is_empty() || input.sql.len() > crate::policy::MAX_SQL_BYTES {
            return Err(WorkbenchError::Limit);
        }
        let view = self.runtime.get(context, input.session)?;
        let manual = self.manual(context, input.session)?;
        if let Some(tx) = &manual {
            if tx.finalizing.load(Ordering::SeqCst)
                || tx.transaction.lock().map_err(|_| WorkbenchError::Io)?.state
                    != TransactionState::Active
            {
                return Err(WorkbenchError::TargetBusy);
            }
        }
        let receipt = self
            .runtime
            .binding_receipt(context, input.session, AuthorizePurpose::Read)
            .await?;
        let budget = self.streams.budget();
        let mut observed = if let Some(tx) = &manual {
            self.runtime
                .with_lease(
                    context,
                    &tx.lease,
                    AuthorizePurpose::Read,
                    move |connection| {
                        Box::pin(async move {
                            Ok((
                                connection.facts().clone(),
                                lexical_settings(connection, &budget).await?,
                                native_session_basis(connection, &budget).await?,
                            ))
                        })
                    },
                )
                .await?
        } else {
            self.runtime
                .with_connection(
                    context,
                    input.session,
                    AuthorizePurpose::Read,
                    move |connection| {
                        Box::pin(async move {
                            Ok((
                                connection.facts().clone(),
                                lexical_settings(connection, &budget).await?,
                                native_session_basis(connection, &budget).await?,
                            ))
                        })
                    },
                )
                .await?
        };
        observed.0.context.purpose = view.actual_context.purpose;
        let engine = observed.0.engine;
        let mode = view.mode;
        let manual_read_only = manual
            .as_ref()
            .is_some_and(|tx| tx.transaction.lock().is_ok_and(|tx| tx.read_only));
        let policy_mode = if manual_read_only {
            Mode::ReadOnly
        } else {
            mode
        };
        let settings = observed.1.clone();
        let purpose = input.purpose;
        let in_manual = manual.is_some();
        let sql = Zeroizing::new(input.sql);
        let (sql, mut policy) = tokio::task::spawn_blocking(move || {
            let policy = SqlPolicy::analyze(
                engine,
                policy_mode,
                &sql,
                &PolicyContext {
                    purpose: native_purpose(purpose),
                    manual_transaction: in_manual,
                    lexical_settings: settings,
                },
            )?;
            Ok::<_, WorkbenchError>((sql, policy))
        })
        .await
        .map_err(|_| WorkbenchError::Io)??;
        if manual.is_some()
            && !manual_read_only
            && policy
                .statements
                .iter()
                .any(|statement| statement.has_function_calls)
        {
            return Err(WorkbenchError::Unsupported);
        }
        if mode == Mode::ReadOnly && manual.is_none() {
            policy
                .warnings
                .push("statement_scoped_readonly_control".into());
        }
        policy.warnings.sort();
        policy.warnings.dedup();
        let id = Uuid::new_v4();
        let public = QueryReview {
            review_id: id,
            digest: policy.digest.clone(),
            required_mode: policy.required_mode,
            statement_count: policy.statements.len(),
            warnings: policy.warnings.clone(),
            expires_at_ms: now_ms().saturating_add(60_000).into(),
        };
        let memory = self
            .streams
            .budget()
            .reserve(sql.len() * 2 + policy.statements.len() * 2048 + 4096)?;
        let review = Arc::new(Review {
            owner: context.principal().owner_id().into(),
            window: context.window_instance_id(),
            session: input.session,
            scope: view.scope,
            receipt,
            mode,
            manual_read_only,
            sql,
            policy,
            lexical: observed.1,
            session_basis: observed.2,
            facts: observed.0,
            purpose,
            manual: manual.as_ref().map(|tx| tx.lease.id()),
            public: public.clone(),
            created: Instant::now(),
            consumed: AtomicBool::new(false),
            _memory: memory,
        });
        let mut reviews = self.reviews.lock().map_err(|_| WorkbenchError::Io)?;
        reviews.retain(|_, review| review.created.elapsed() < Duration::from_secs(60));
        if reviews.len() >= 64 {
            let oldest = reviews
                .iter()
                .filter(|(_, review)| review.consumed.load(Ordering::SeqCst))
                .min_by_key(|(_, review)| review.created)
                .map(|(id, _)| *id);
            if let Some(id) = oldest {
                reviews.remove(&id);
            }
        }
        if reviews.len() >= 64
            || reviews
                .values()
                .map(|review| review.sql.len())
                .sum::<usize>()
                + review.sql.len()
                > 8 * 1024 * 1024
        {
            return Err(WorkbenchError::Limit);
        }
        reviews.insert(id, review);
        Ok(public)
    }
    fn resolve(
        &self,
        context: &InvocationContext,
        input: &QueryRunInput,
    ) -> Result<Arc<Review>, WorkbenchError> {
        self.context(context)?;
        validate_limits(&input.limits)?;
        let review = self
            .reviews
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .get(&input.review_id)
            .cloned()
            .ok_or(WorkbenchError::AttemptExpired)?;
        if review.owner != context.principal().owner_id()
            || review.window != context.window_instance_id()
            || review.session != input.session
        {
            return Err(WorkbenchError::Forbidden);
        }
        if review.created.elapsed() >= Duration::from_secs(60) {
            return Err(WorkbenchError::AttemptExpired);
        }
        let current = self.runtime.get(context, input.session)?;
        if current.mode != review.mode
            || current.scope != review.scope
            || current.descriptor_revision != review.receipt.descriptor_revision
        {
            return Err(WorkbenchError::BindingStale);
        }
        let manual = self.manual(context, input.session)?;
        if manual.as_ref().map(|tx| tx.lease.id()) != review.manual
            || manual
                .as_ref()
                .is_some_and(|tx| tx.finalizing.load(Ordering::SeqCst))
        {
            return Err(WorkbenchError::TargetBusy);
        }
        let mut confirmed = input.confirmed_warnings.clone();
        if confirmed.len() > 64 || confirmed.iter().any(|warning| warning.len() > 128) {
            return Err(WorkbenchError::Limit);
        }
        confirmed.sort();
        confirmed.dedup();
        if confirmed != review.policy.warnings {
            return Err(WorkbenchError::InvalidInput);
        }
        if matches!(review.facts.engine, Engine::Mysql | Engine::Mariadb)
            && !input.limits.lock_wait_ms.is_multiple_of(1000)
        {
            return Err(WorkbenchError::Unsupported);
        }
        review.policy.verify_source(&review.sql)?;
        Ok(review)
    }
    pub async fn prepare_query(
        self: &Arc<Self>,
        context: &InvocationContext,
        input: QueryRunInput,
    ) -> Result<PreparedAttempt, WorkbenchError> {
        let review = self.resolve(context, &input)?;
        let receipt = self
            .runtime
            .binding_receipt(context, input.session, purpose_for(&review))
            .await?;
        same_basis(&review.receipt, &receipt)?;
        let resolved = resolved_query(&review, &input)?;
        let payload = resolved.clone();
        let native_context = context.clone();
        let native_receipt = receipt.clone();
        let session = input.session;
        let public = public_query_payload(&input)?;
        let mut token = self
            .jcall(move |journal| {
                journal.prepare_query_attempt(
                    &native_context,
                    &native_receipt,
                    session,
                    &payload,
                    &public,
                )
            })
            .await?;
        token.expires_at_ms = token
            .expires_at_ms
            .value()
            .min(review.public.expires_at_ms.value())
            .into();
        let mut prepared = self.prepared.lock().map_err(|_| WorkbenchError::Io)?;
        prepared.retain(|_, entry| entry.created.elapsed() < Duration::from_secs(60));
        if prepared.len() >= 1024 {
            return Err(WorkbenchError::Limit);
        }
        prepared.insert(
            token.attempt_token.clone(),
            Arc::new(PreparedRun {
                review,
                resolved,
                wire_digest: wire_digest(&input)?,
                created: Instant::now(),
            }),
        );
        Ok(token)
    }
    pub async fn run(
        self: &Arc<Self>,
        context: &InvocationContext,
        token: String,
        input: QueryRunInput,
    ) -> Result<QueryAdmission, WorkbenchError> {
        self.context(context)?;
        let _request = self
            .admission_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| WorkbenchError::Limit)?;
        // One bounded admission writer reserves map capacity through durable
        // admission+epoch binding+publication. A duplicate cannot observe epoch0
        // between those phases. This lock is never held across target SQL.
        let _writer = self.admission_lock.lock().await;
        let public = public_query_payload(&input)?;
        let native_context = context.clone();
        let native_token = token.clone();
        let session = input.session;
        if let Some((mut admission, scope, epoch)) = self
            .jcall(move |journal| {
                journal.query_status(&native_context, &native_token, &public, session)
            })
            .await?
        {
            let cached = self
                .runs
                .lock()
                .map_err(|_| WorkbenchError::Io)?
                .contains_key(&admission.operation_id);
            if !cached {
                // A dropped invocation can leave a durable accepted row before
                // publication. Under this writer barrier, no worker exists for
                // that row; CAS cleanup proves unsent without launching a run.
                let native = context.clone();
                let id = admission.operation_id;
                let operation = self
                    .jcall(move |journal| {
                        let old = journal.get(&native, &scope, id)?;
                        if old.state == OperationState::Accepted {
                            match journal.transition(
                                &native,
                                &scope,
                                id,
                                old.version.value(),
                                Transition::FailedBeforeSend,
                            ) {
                                Ok(operation) => Ok(operation),
                                Err(WorkbenchError::IdempotencyConflict) => {
                                    journal.get(&native, &scope, id)
                                }
                                Err(error) => Err(error),
                            }
                        } else {
                            Ok(old)
                        }
                    })
                    .await?;
                admission.version = operation.version;
            }
            return Ok(QueryAdmission {
                query_run_id: admission.operation_id,
                execution_epoch: epoch.into(),
                admission,
            });
        }
        let digest = wire_digest(&input)?;
        let prepared = self
            .prepared
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .get(&token)
            .cloned()
            .ok_or(WorkbenchError::AttemptExpired)?;
        if prepared.wire_digest != digest || prepared.created.elapsed() >= Duration::from_secs(60) {
            return Err(WorkbenchError::IdempotencyConflict);
        }
        let review = self.resolve(context, &input)?;
        if !Arc::ptr_eq(&prepared.review, &review)
            || resolved_query(&review, &input)? != prepared.resolved
        {
            return Err(WorkbenchError::IdempotencyConflict);
        }
        let manual = self.manual(context, input.session)?;
        self.reserve_run_capacity(context)?;
        let memory = self.streams.budget().reserve(4096 + 64 * 512)?;
        let reservation = Arc::new(self.runtime.reserve_execution(
            context,
            input.session,
            manual.as_ref().map(|tx| tx.lease.as_ref()),
        )?);
        let receipt = self
            .runtime
            .binding_receipt(context, input.session, purpose_for(&review))
            .await?;
        same_basis(&review.receipt, &receipt)?;
        let read_hold = review.mode == Mode::ReadOnly
            || manual
                .as_ref()
                .is_some_and(|tx| tx.transaction.lock().is_ok_and(|tx| tx.read_only));
        let transaction_id = manual
            .as_ref()
            .map(|tx| {
                tx.transaction
                    .lock()
                    .map(|tx| tx.transaction_id)
                    .map_err(|_| WorkbenchError::Io)
            })
            .transpose()?;
        let spec = AdmissionSpec {
            kind: OperationKind::Query,
            session: Some(input.session),
            transaction_id,
            statements: review
                .policy
                .statements
                .iter()
                .map(|statement| StatementSpec {
                    digest: statement.digest.clone(),
                    effect_class: if !read_hold
                        && statement.effect == EffectClass::Read
                        && statement.has_function_calls
                    {
                        EffectClass::Unknown
                    } else {
                        statement.effect
                    },
                    certified_read_only: read_hold && statement.read_only_candidate,
                    transaction_id,
                })
                .collect(),
            batch: None,
        };
        let credential = self.runtime.credential_reference(context, input.session)?;
        let restored_check = self.runtime.restored_check(context, input.session)?;
        let native = context.clone();
        let payload = prepared.resolved.clone();
        let native_token = token.clone();
        let native_receipt = receipt.clone();
        let mut admission = self
            .jcall(move |journal| {
                journal.admit(
                    &native,
                    &native_token,
                    "query.run",
                    &payload,
                    &native_receipt,
                    Some(credential),
                    spec,
                )
            })
            .await?;
        if admission.deduplicated {
            // Generic journal admission may race a trusted native caller. Never
            // create a second volatile run/ticket for the same durable operation.
            let native = context.clone();
            let scope = review.scope.clone();
            let id = admission.operation_id;
            let epoch = self
                .jcall(move |journal| journal.execution_epoch(&native, &scope, id))
                .await?;
            return Ok(QueryAdmission {
                query_run_id: id,
                execution_epoch: epoch.into(),
                admission,
            });
        }
        let native = context.clone();
        let scope = review.scope.clone();
        let id = admission.operation_id;
        let version = admission.version.value();
        let epoch = reservation.epoch();
        let bound = self
            .jcall(move |journal| journal.bind_execution_epoch(&native, &scope, id, version, epoch))
            .await;
        let bound = match bound {
            Ok(bound) => bound,
            Err(error) => {
                self.cleanup_unsent(context, &review.scope, id).await;
                return Err(error);
            }
        };
        admission.version = bound.version;
        let run = Arc::new(Run {
            owner: context.principal().owner_id().into(),
            window: context.window_instance_id(),
            session: input.session,
            scope: review.scope.clone(),
            id,
            operation_id: id,
            reservation: reservation.clone(),
            results: Mutex::new(Vec::new()),
            finished: AtomicBool::new(false),
            cancel_pending: AtomicBool::new(false),
            created: Instant::now(),
            credential,
            mode: review.mode,
            restored_check,
            _memory: memory,
        });
        let inserted = {
            match self.runs.lock() {
                Ok(mut runs) if runs.len() < self.run_capacity && !runs.contains_key(&id) => {
                    runs.insert(id, run.clone());
                    true
                }
                _ => false,
            }
        };
        if !inserted {
            self.cleanup_unsent(context, &review.scope, id).await;
            return Err(WorkbenchError::Limit);
        }
        let reply = QueryAdmission {
            admission: admission.clone(),
            query_run_id: id,
            execution_epoch: reservation.epoch().into(),
        };
        let coordinator = self.clone();
        let context = context.clone();
        tokio::spawn(async move {
            coordinator
                .execute(context, run, review, manual, input.limits, admission)
                .await;
        });
        self.prepared
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .remove(&token);
        Ok(reply)
    }
    async fn cleanup_unsent(&self, context: &InvocationContext, scope: &Scope, id: Uuid) {
        let native = context.clone();
        let scope = scope.clone();
        let _ = self
            .jcall(move |journal| {
                let old = journal.get(&native, &scope, id)?;
                if old.state == OperationState::Accepted {
                    journal.transition(
                        &native,
                        &scope,
                        id,
                        old.version.value(),
                        Transition::FailedBeforeSend,
                    )
                } else {
                    Ok(old)
                }
            })
            .await;
    }
    async fn jcall<
        T: Send + 'static,
        F: FnOnce(Arc<OperationJournal>) -> Result<T, WorkbenchError> + Send + 'static,
    >(
        &self,
        work: F,
    ) -> Result<T, WorkbenchError> {
        let journal = self.journal.clone();
        tokio::task::spawn_blocking(move || work(journal))
            .await
            .map_err(|_| WorkbenchError::StoreUnavailable)?
    }
    async fn statement_outcome(
        &self,
        context: &InvocationContext,
        run: &Run,
        operation: &Operation,
        ordinal: usize,
    ) -> Result<StatementOutcome, WorkbenchError> {
        if let Some(statement) = operation
            .statements
            .iter()
            .find(|statement| statement.ordinal as usize == ordinal)
        {
            return Ok(statement.clone());
        }
        let ctx = context.clone();
        let scope = run.scope.clone();
        let id = run.operation_id;
        self.jcall(move |journal| {
            journal
                .statements(&ctx, &scope, id, (ordinal as u32).checked_sub(1), 1)?
                .into_iter()
                .find(|statement| statement.ordinal as usize == ordinal)
                .ok_or(WorkbenchError::StoreUnavailable)
        })
        .await
    }
    pub async fn results(
        &self,
        context: &InvocationContext,
        input: QueryResultsInput,
    ) -> Result<Page<QueryResult>, WorkbenchError> {
        self.context(context)?;
        let run = self
            .runs
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .get(&input.query_run_id)
            .cloned()
            .ok_or(WorkbenchError::ResultExpired)?;
        check_run(context, input.session, &run)?;
        let limit = if input.page.limit == 0 {
            100
        } else {
            input.page.limit
        };
        if limit > 200 {
            return Err(WorkbenchError::Limit);
        }
        let index = if let Some(cursor) = input.page.cursor {
            let id = Uuid::parse_str(&cursor).map_err(|_| WorkbenchError::CursorInvalid)?;
            let cursors = self.cursors.lock().map_err(|_| WorkbenchError::Io)?;
            let cursor = cursors.get(&id).ok_or(WorkbenchError::CursorInvalid)?;
            if cursor.run != run.id
                || cursor.window != context.window_instance_id()
                || cursor.created.elapsed() > Duration::from_secs(120)
            {
                return Err(WorkbenchError::CursorInvalid);
            }
            cursor.index
        } else {
            0
        };
        let results = run.results.lock().map_err(|_| WorkbenchError::Io)?;
        if index > results.len() {
            return Err(WorkbenchError::CursorInvalid);
        }
        let end = (index + limit).min(results.len());
        let items = results[index..end].to_vec();
        let next = if end < results.len() {
            let mut cursors = self.cursors.lock().map_err(|_| WorkbenchError::Io)?;
            cursors.retain(|_, cursor| cursor.created.elapsed() < Duration::from_secs(120));
            if cursors.len() >= 1024 {
                return Err(WorkbenchError::Limit);
            }
            let id = Uuid::new_v4();
            cursors.insert(
                id,
                ResultsCursor {
                    run: run.id,
                    window: context.window_instance_id(),
                    index: end,
                    created: Instant::now(),
                },
            );
            Some(id.to_string())
        } else {
            None
        };
        let complete = run.finished.load(Ordering::SeqCst) && next.is_none();
        Ok(Page {
            items,
            next_cursor: next,
            completeness: if complete {
                Completeness::Complete
            } else {
                Completeness::Limited
            },
            count: if run.finished.load(Ordering::SeqCst) {
                Count::Exact {
                    value: (results.len() as u64).into(),
                }
            } else {
                Count::Unknown
            },
            as_of_ms: now_ms().into(),
        })
    }
    pub async fn cancel(
        &self,
        context: &InvocationContext,
        input: QueryCancelInput,
    ) -> Result<QueryCancelResult, WorkbenchError> {
        self.context(context)?;
        let run = self
            .runs
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .get(&input.query_run_id)
            .cloned()
            .ok_or(WorkbenchError::ResultExpired)?;
        check_run(context, input.session, &run)?;
        if input.execution_epoch.value() != run.reservation.epoch() {
            return Err(WorkbenchError::SessionStale);
        }
        if run.finished.load(Ordering::SeqCst) {
            return Ok(QueryCancelResult {
                cancel_requested: false,
                settled: true,
            });
        }
        if run.reservation.cancel_before_start() {
            return Ok(QueryCancelResult {
                cancel_requested: true,
                settled: true,
            });
        }
        run.cancel_pending.store(true, Ordering::SeqCst);
        let _cancel = PendingCancel(run.clone());
        let _ = self
            .runtime
            .request_cancel(context, input.session, input.execution_epoch.value())
            .await;
        Ok(QueryCancelResult {
            cancel_requested: true,
            settled: false,
        })
    }
    async fn execute(
        self: Arc<Self>,
        context: InvocationContext,
        run: Arc<Run>,
        review: Arc<Review>,
        manual: Option<Arc<Manual>>,
        limits: Limits,
        admission: Admission,
    ) {
        let coordinator = self.clone();
        let native_context = context.clone();
        let native_run = run.clone();
        let native_review = review.clone();
        let native_manual = manual.clone();
        let result = self
            .runtime
            .with_execution(
                &context,
                &run.reservation,
                manual.as_ref().map(|tx| tx.lease.as_ref()),
                purpose_for(&review),
                move |connection| {
                    Box::pin(async move {
                        coordinator
                            .execute_on_connection(
                                connection,
                                &native_context,
                                &native_run,
                                &native_review,
                                native_manual.as_ref(),
                                limits,
                                admission,
                            )
                            .await
                    })
                },
            )
            .await;
        if let Err(error) = &result {
            let failed_held = matches!(
                error,
                WorkbenchError::StatementRejected(
                    DriverTransactionState::Active | DriverTransactionState::Failed
                )
            );
            let ctx = context.clone();
            let scope = run.scope.clone();
            let id = run.operation_id;
            let cancelled = run.reservation.cancelled();
            let _ = self
                .jcall(move |journal| {
                    let old = journal.get(&ctx, &scope, id)?;
                    if old.state == OperationState::Accepted {
                        journal.transition(
                            &ctx,
                            &scope,
                            id,
                            old.version.value(),
                            if cancelled {
                                Transition::CancelledBeforeSend
                            } else {
                                Transition::FailedBeforeSend
                            },
                        )
                    } else if matches!(
                        old.state,
                        OperationState::Dispatching
                            | OperationState::Running
                            | OperationState::CommitRequested
                    ) {
                        let readonly = old
                            .statements
                            .iter()
                            .all(|statement| statement.effect_evidence == EffectEvidence::ReadOnly);
                        journal.transition(
                            &ctx,
                            &scope,
                            id,
                            old.version.value(),
                            if readonly {
                                Transition::ReadInterrupted
                            } else {
                                Transition::EffectsUnknown
                            },
                        )
                    } else {
                        Ok(old)
                    }
                })
                .await;
            if let Some(manual) = manual.filter(|_| run.reservation.started()) {
                let id = {
                    let mut tx = match manual.transaction.lock() {
                        Ok(tx) => tx,
                        Err(_) => {
                            run.reservation.finish();
                            run.finished.store(true, Ordering::SeqCst);
                            return;
                        }
                    };
                    tx.state = if failed_held {
                        TransactionState::FailedNeedsRollback
                    } else {
                        TransactionState::OutcomeUnknown
                    };
                    tx.transaction_id
                };
                let scope = manual.scope.clone();
                let ctx = context.clone();
                if !failed_held {
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
            }
        }
        // An expired consumer may already have a status-only terminal. Never
        // overwrite it or fabricate sent rows/ACK evidence.
        if let Ok(mut results) = run.results.lock() {
            for result in results.iter_mut() {
                if matches!(result.state, QueryResultState::Streaming) {
                    result.state = QueryResultState::Terminal;
                    result.completeness = Some(Completion::Interrupted);
                }
            }
        }
        run.reservation.finish();
        run.finished.store(true, Ordering::SeqCst);
        review.consumed.store(true, Ordering::SeqCst);
    }
    #[allow(
        clippy::too_many_arguments,
        reason = "one physical execution barrier keeps native context, immutable review, lease, limits and durable admission explicit"
    )]
    async fn execute_on_connection(
        &self,
        connection: &mut dyn DriverConnection,
        context: &InvocationContext,
        run: &Arc<Run>,
        review: &Review,
        manual: Option<&Arc<Manual>>,
        limits: Limits,
        admission: Admission,
    ) -> Result<(), WorkbenchError> {
        connection
            .configure_limits(Duration::from_millis(limits.lock_wait_ms))
            .await?;
        let budget = self.streams.budget();
        let mut operation = None;
        let mut rows_left = limits.max_rows;
        let mut bytes_left = limits.max_bytes;
        let mut capped = false;
        for (ordinal, statement) in review.policy.statements.iter().enumerate() {
            if run.reservation.cancelled() {
                return Err(WorkbenchError::CancelUnsettled);
            }
            let fresh = self
                .runtime
                .binding_receipt(context, run.session, purpose_for(review))
                .await?;
            same_basis(&review.receipt, &fresh)?;
            if lexical_settings(connection, &budget).await? != review.lexical {
                return Err(WorkbenchError::BindingStale);
            }
            if native_session_basis(connection, &budget).await? != review.session_basis {
                return Err(WorkbenchError::BindingStale);
            }
            let scoped_readonly = manual.is_none() && review.mode == Mode::ReadOnly;
            if scoped_readonly {
                connection
                    .transaction_control(TransactionCommand::ReadOnlyStatementBegin)
                    .await?;
            }
            let expected_readonly = review.mode == Mode::ReadOnly
                || manual.is_some_and(|tx| tx.transaction.lock().is_ok_and(|tx| tx.read_only));
            if expected_readonly && !connection.transaction_read_only().await? {
                return Err(WorkbenchError::Unsupported);
            }
            if let Some(manual) = manual {
                if !expected_readonly {
                    verify_manual_effects(connection, &budget, statement).await?;
                }
                self.runtime
                    .refresh_manual_idle(context, &manual.lease, self.idle_timeout)?;
            }
            if operation.is_none() {
                if run.reservation.cancelled() {
                    if scoped_readonly {
                        let _ = connection
                            .transaction_control(TransactionCommand::Rollback)
                            .await;
                    }
                    return Err(WorkbenchError::CancelUnsettled);
                }
                let ctx = context.clone();
                let scope = run.scope.clone();
                let id = run.operation_id;
                let receipt = fresh.clone();
                let epoch = run.reservation.epoch();
                let version = admission.version.value();
                let permit = self
                    .jcall(move |journal| {
                        journal.mark_dispatch(&ctx, &scope, id, version, epoch, &receipt)
                    })
                    .await?;
                let ctx = context.clone();
                let scope = run.scope.clone();
                let id = run.operation_id;
                operation = Some(
                    self.jcall(move |journal| {
                        journal.transition(&ctx, &scope, id, permit.version(), Transition::Running)
                    })
                    .await?,
                );
            }
            let old = operation.take().ok_or(WorkbenchError::StoreUnavailable)?;
            let prior = self.statement_outcome(context, run, &old, ordinal).await?;
            let ctx = context.clone();
            let scope = run.scope.clone();
            let id = run.operation_id;
            let update = StatementUpdate {
                ordinal: ordinal as u32,
                expected_version: prior.version.value(),
                outcome: StatementStatus::Sent,
                effect_evidence: prior.effect_evidence,
                rows_matched: None,
                rows_changed: None,
                error_code: None,
            };
            let sent = self
                .jcall(move |journal| {
                    journal.record_statement(&ctx, &scope, id, old.version.value(), update)
                })
                .await?;
            if run.reservation.cancelled() {
                return Err(WorkbenchError::CancelUnsettled);
            }
            let mut sink = QuerySink::new(
                self.runtime.clone(),
                self.streams.clone(),
                context.clone(),
                run.clone(),
                rows_left,
                bytes_left,
                sent.version.value(),
            );
            let source = statement.source(&review.sql)?;
            let autocommit_rejection_certified = if manual.is_none()
                && !expected_readonly
                && matches!(connection.facts().engine, Engine::Mysql | Engine::Mariadb)
                && statement.effect == EffectClass::Write
                && !statement.has_function_calls
            {
                match verify_manual_effects(connection, &budget, statement).await {
                    Ok(()) => true,
                    Err(WorkbenchError::Unsupported) => false,
                    Err(error) => return Err(error),
                }
            } else {
                false
            };
            let outcome = connection
                .execute(
                    source,
                    QueryLimits {
                        max_rows: rows_left.max(1),
                        max_bytes: bytes_left.max(1),
                        timeout: Duration::from_millis(limits.statement_timeout_ms),
                    },
                    &mut sink,
                )
                .await;
            let summary = match outcome {
                Ok(summary) => summary,
                Err(error) => {
                    let _ = sink
                        .finish(
                            Completion::Interrupted,
                            sent.version.value(),
                            Some(error.code().into()),
                        )
                        .await;
                    let known_held = manual.is_some()
                        && statement.effect != EffectClass::Unknown
                        && !statement.has_function_calls
                        && matches!(
                            error,
                            WorkbenchError::StatementRejected(
                                DriverTransactionState::Active | DriverTransactionState::Failed
                            )
                        );
                    let known_idle = manual.is_none()
                        && matches!(
                            error,
                            WorkbenchError::StatementRejected(DriverTransactionState::Idle)
                        )
                        && statement.effect != EffectClass::Unknown
                        && !statement.has_function_calls
                        && (connection.facts().engine == Engine::Postgresql
                            || autocommit_rejection_certified);
                    let known = (expected_readonly
                        && matches!(error, WorkbenchError::StatementRejected(_)))
                        || known_held
                        || known_idle;
                    if scoped_readonly {
                        connection
                            .transaction_control(TransactionCommand::Rollback)
                            .await?;
                    }
                    let ctx = context.clone();
                    let scope = run.scope.clone();
                    let id = run.operation_id;
                    let previous = self.statement_outcome(context, run, &sent, ordinal).await?;
                    let update = StatementUpdate {
                        ordinal: ordinal as u32,
                        expected_version: previous.version.value(),
                        outcome: if known || expected_readonly {
                            StatementStatus::Failed
                        } else {
                            StatementStatus::OutcomeUnknown
                        },
                        effect_evidence: if expected_readonly {
                            EffectEvidence::ReadOnly
                        } else if known_held {
                            EffectEvidence::PendingTransaction
                        } else if known_idle {
                            EffectEvidence::RolledBack
                        } else {
                            EffectEvidence::OutcomeUnknown
                        },
                        rows_matched: None,
                        rows_changed: None,
                        error_code: Some(error.code().into()),
                    };
                    let recorded = self
                        .jcall(move |journal| {
                            journal.record_statement(&ctx, &scope, id, sent.version.value(), update)
                        })
                        .await?;
                    if known {
                        let ctx = context.clone();
                        let scope = run.scope.clone();
                        let id = run.operation_id;
                        self.jcall(move |journal| {
                            journal.transition(
                                &ctx,
                                &scope,
                                id,
                                recorded.version.value(),
                                Transition::StatementGroupFailed,
                            )
                        })
                        .await?;
                    }
                    return Err(error);
                }
            };
            rows_left = rows_left.saturating_sub(sink.rows);
            bytes_left = bytes_left.saturating_sub(sink.bytes);
            capped |= summary.capped || sink.capped;
            let evidence = if expected_readonly {
                EffectEvidence::ReadOnly
            } else if statement.effect == EffectClass::Unknown {
                EffectEvidence::OutcomeUnknown
            } else if manual.is_some()
                && summary.transaction_state == DriverTransactionState::Active
            {
                EffectEvidence::PendingTransaction
            } else if manual.is_none() && summary.transaction_state == DriverTransactionState::Idle
            {
                EffectEvidence::Committed
            } else {
                EffectEvidence::OutcomeUnknown
            };
            let ctx = context.clone();
            let scope = run.scope.clone();
            let id = run.operation_id;
            let previous = self.statement_outcome(context, run, &sent, ordinal).await?;
            let update = StatementUpdate {
                ordinal: ordinal as u32,
                expected_version: previous.version.value(),
                outcome: StatementStatus::Succeeded,
                effect_evidence: evidence,
                rows_matched: None,
                rows_changed: None,
                error_code: None,
            };
            let recorded = self
                .jcall(move |journal| {
                    journal.record_statement(&ctx, &scope, id, sent.version.value(), update)
                })
                .await?;
            sink.finish(
                if summary.capped || sink.capped {
                    Completion::Capped
                } else {
                    Completion::Complete
                },
                recorded.version.value(),
                None,
            )
            .await?;
            if scoped_readonly {
                connection
                    .transaction_control(TransactionCommand::Rollback)
                    .await?;
            }
            if evidence == EffectEvidence::OutcomeUnknown {
                return Err(WorkbenchError::OutcomeUnknown);
            }
            if let Some(manual) = manual {
                self.runtime
                    .refresh_manual_idle(context, &manual.lease, self.idle_timeout)?;
            }
            operation = Some(recorded);
        }
        let old = operation.ok_or(WorkbenchError::InvalidInput)?;
        let ctx = context.clone();
        let scope = run.scope.clone();
        let id = run.operation_id;
        let complete = if capped {
            ResultCompleteness::Capped
        } else {
            ResultCompleteness::Complete
        };
        self.jcall(move |journal| {
            journal.transition(
                &ctx,
                &scope,
                id,
                old.version.value(),
                Transition::StatementGroupCompleted(complete),
            )
        })
        .await?;
        if review.policy.retire_after_run {
            return Err(WorkbenchError::SessionStale);
        }
        Ok(())
    }
    pub async fn transaction_get(
        &self,
        context: &InvocationContext,
        session: SessionRef,
    ) -> Result<Option<Transaction>, WorkbenchError> {
        self.context(context)?;
        let Some(manual) = self.manual(context, session)? else {
            return Ok(None);
        };
        if self
            .runtime
            .get(context, session)
            .map_or(true, |session| session.state == SessionState::Retiring)
        {
            self.mark_manual_unknown(context, &manual).await?;
        }
        let mut transaction = manual
            .transaction
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .clone();
        transaction.idle_expires_at_ms = manual.lease.expires_at_ms().into();
        Ok(Some(transaction))
    }
    pub async fn transaction_begin(
        self: &Arc<Self>,
        context: &InvocationContext,
        input: TransactionBeginInput,
    ) -> Result<Transaction, WorkbenchError> {
        self.context(context)?;
        {
            let mut transactions = self.transactions.lock().map_err(|_| WorkbenchError::Io)?;
            transactions.retain(|_, tx| {
                tx.created.elapsed() < Duration::from_secs(120)
                    || tx.transaction.lock().is_ok_and(|tx| {
                        matches!(
                            tx.state,
                            TransactionState::Active
                                | TransactionState::Opening
                                | TransactionState::FailedNeedsRollback
                                | TransactionState::CommitRequested
                                | TransactionState::RollbackRequested
                        )
                    })
            });
            if transactions.len() >= 128 {
                return Err(WorkbenchError::Limit);
            }
        }
        if self.manual(context, input.session)?.is_some() {
            return Err(WorkbenchError::TargetBusy);
        }
        let view = self.runtime.get(context, input.session)?;
        if view.mode == Mode::ReadOnly && !input.read_only {
            return Err(WorkbenchError::Forbidden);
        }
        let isolation = TransactionIsolation::parse(&input.isolation)?;
        let purpose = if input.read_only {
            AuthorizePurpose::Read
        } else {
            AuthorizePurpose::Mutate
        };
        let receipt = self
            .runtime
            .binding_receipt(context, input.session, purpose)
            .await?;
        let lease = Arc::new(
            self.runtime
                .pin_connection(
                    context,
                    input.session,
                    ConnectionLeaseKind::ManualTransaction,
                    self.idle_timeout,
                )
                .await?,
        );
        let intent =
            json!({"session":input.session,"isolation":input.isolation,"readOnly":input.read_only});
        let begin_digest =
            blake3::hash(&serde_json::to_vec(&intent).map_err(|_| WorkbenchError::InvalidInput)?)
                .to_hex()
                .to_string();
        let ctx = context.clone();
        let native_receipt = receipt.clone();
        let payload = intent.clone();
        let session = input.session;
        let credential = self.runtime.credential_reference(context, session)?;
        let token = self
            .jcall(move |journal| {
                journal.prepare(
                    &ctx,
                    &native_receipt,
                    Some(session),
                    "transaction.begin",
                    &payload,
                )
            })
            .await?;
        let spec = AdmissionSpec {
            kind: OperationKind::ManualTransaction,
            session: Some(session),
            transaction_id: None,
            statements: vec![StatementSpec {
                digest: begin_digest,
                effect_class: EffectClass::Transaction,
                certified_read_only: false,
                transaction_id: None,
            }],
            batch: None,
        };
        let ctx = context.clone();
        let native_receipt = receipt.clone();
        let payload = intent.clone();
        let admission = self
            .jcall(move |journal| {
                journal.admit(
                    &ctx,
                    &token.attempt_token,
                    "transaction.begin",
                    &payload,
                    &native_receipt,
                    Some(credential),
                    spec,
                )
            })
            .await?;
        let this = self.clone();
        let ctx = context.clone();
        let native_lease = lease.clone();
        let native_input = input.clone();
        let scope = receipt.scope.clone();
        let id = admission.operation_id;
        let manual = Arc::new(Manual {
            owner: context.principal().owner_id().into(),
            window: context.window_instance_id(),
            scope: view.scope.clone(),
            lease: lease.clone(),
            transaction: Mutex::new(Transaction {
                transaction_id: id,
                session: input.session,
                state: TransactionState::Opening,
                isolation: input.isolation.clone(),
                read_only: input.read_only,
                idle_expires_at_ms: lease.expires_at_ms().into(),
            }),
            finalizing: AtomicBool::new(false),
            created: Instant::now(),
            credential,
            mode: view.mode,
            restored_check: self.runtime.restored_check(context, input.session)?,
        });
        self.transactions
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .insert(id, manual.clone());
        let opened = self
            .runtime
            .with_lease(context, &lease, purpose, move |connection| {
                Box::pin(async move {
                    let fresh = this
                        .runtime
                        .binding_receipt(&ctx, native_input.session, purpose)
                        .await?;
                    same_basis(&receipt, &fresh)?;
                    let epoch = this
                        .runtime
                        .get(&ctx, native_input.session)?
                        .execution_epoch
                        .value();
                    let jctx = ctx.clone();
                    let jscope = scope.clone();
                    let version = admission.version.value();
                    let permit = this
                        .jcall(move |journal| {
                            journal.mark_dispatch(&jctx, &jscope, id, version, epoch, &fresh)
                        })
                        .await?;
                    let jctx = ctx.clone();
                    let jscope = scope.clone();
                    let running = this
                        .jcall(move |journal| {
                            journal.transition(
                                &jctx,
                                &jscope,
                                id,
                                permit.version(),
                                Transition::Running,
                            )
                        })
                        .await?;
                    let prior = running
                        .statements
                        .first()
                        .ok_or(WorkbenchError::StoreUnavailable)?;
                    let update = StatementUpdate {
                        ordinal: 0,
                        expected_version: prior.version.value(),
                        outcome: StatementStatus::Sent,
                        effect_evidence: prior.effect_evidence,
                        rows_matched: None,
                        rows_changed: None,
                        error_code: None,
                    };
                    let jctx = ctx.clone();
                    let jscope = scope.clone();
                    let sent = this
                        .jcall(move |journal| {
                            journal.record_statement(
                                &jctx,
                                &jscope,
                                id,
                                running.version.value(),
                                update,
                            )
                        })
                        .await?;
                    if native_input.read_only {
                        connection.set_read_only(true).await?;
                    }
                    connection
                        .transaction_control(TransactionCommand::ManualBegin {
                            isolation,
                            read_only: native_input.read_only,
                        })
                        .await?;
                    if native_input.read_only && !connection.transaction_read_only().await? {
                        return Err(WorkbenchError::Unsupported);
                    }
                    let actual = if isolation == TransactionIsolation::Default {
                        observed_isolation(connection, &this.streams.budget()).await?
                    } else {
                        native_input.isolation.clone()
                    };
                    let prior = sent
                        .statements
                        .first()
                        .ok_or(WorkbenchError::StoreUnavailable)?;
                    let update = StatementUpdate {
                        ordinal: 0,
                        expected_version: prior.version.value(),
                        outcome: StatementStatus::Succeeded,
                        effect_evidence: EffectEvidence::PendingTransaction,
                        rows_matched: None,
                        rows_changed: None,
                        error_code: None,
                    };
                    let jctx = ctx.clone();
                    let jscope = scope.clone();
                    this.jcall(move |journal| {
                        journal.record_statement(&jctx, &jscope, id, sent.version.value(), update)
                    })
                    .await?;
                    Ok(Transaction {
                        transaction_id: id,
                        session: native_input.session,
                        state: TransactionState::Active,
                        isolation: actual,
                        read_only: native_input.read_only,
                        idle_expires_at_ms: native_lease.expires_at_ms().into(),
                    })
                })
            })
            .await;
        match opened {
            Ok(transaction) => {
                *manual.transaction.lock().map_err(|_| WorkbenchError::Io)? = transaction.clone();
                Ok(transaction)
            }
            Err(error) => {
                manual
                    .transaction
                    .lock()
                    .map_err(|_| WorkbenchError::Io)?
                    .state = TransactionState::OutcomeUnknown;
                let _ = self.runtime.finish_lease(context, &lease, false).await;
                let ctx = context.clone();
                let scope = view.scope;
                let _ = self
                    .jcall(move |journal| {
                        let old = journal.get(&ctx, &scope, id)?;
                        journal.transition(
                            &ctx,
                            &scope,
                            id,
                            old.version.value(),
                            if old.state == OperationState::Accepted {
                                Transition::FailedBeforeSend
                            } else {
                                Transition::EffectsUnknown
                            },
                        )
                    })
                    .await;
                Err(error)
            }
        }
    }
    fn transaction(
        &self,
        context: &InvocationContext,
        input: &TransactionInput,
    ) -> Result<Arc<Manual>, WorkbenchError> {
        self.context(context)?;
        let tx = self
            .transactions
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .get(&input.transaction_id)
            .cloned()
            .ok_or(WorkbenchError::ResultExpired)?;
        if tx.owner != context.principal().owner_id()
            || tx.window != context.window_instance_id()
            || tx.lease.session() != input.session
        {
            return Err(WorkbenchError::Forbidden);
        }
        Ok(tx)
    }
    pub async fn prepare_commit(
        self: &Arc<Self>,
        context: &InvocationContext,
        input: TransactionInput,
    ) -> Result<PreparedAttempt, WorkbenchError> {
        let tx = self.transaction(context, &input)?;
        let purpose = if tx
            .transaction
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .read_only
        {
            AuthorizePurpose::Read
        } else {
            AuthorizePurpose::Mutate
        };
        if tx.transaction.lock().map_err(|_| WorkbenchError::Io)?.state != TransactionState::Active
            || tx.finalizing.load(Ordering::SeqCst)
        {
            return Err(WorkbenchError::TargetBusy);
        }
        let fresh = self
            .runtime
            .binding_receipt(context, input.session, purpose)
            .await?;
        let payload = serde_json::to_value(&input).map_err(|_| WorkbenchError::InvalidInput)?;
        let ctx = context.clone();
        self.jcall(move |journal| {
            journal.prepare(
                &ctx,
                &fresh,
                Some(input.session),
                "transaction.commit",
                &payload,
            )
        })
        .await
    }
    pub async fn transaction_commit(
        self: &Arc<Self>,
        context: &InvocationContext,
        token: String,
        input: TransactionInput,
    ) -> Result<Admission, WorkbenchError> {
        let tx = self.transaction(context, &input)?;
        let purpose = if tx
            .transaction
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .read_only
        {
            AuthorizePurpose::Read
        } else {
            AuthorizePurpose::Mutate
        };
        let state = tx.transaction.lock().map_err(|_| WorkbenchError::Io)?.state;
        if matches!(
            state,
            TransactionState::Committed
                | TransactionState::OutcomeUnknown
                | TransactionState::CommitRequested
        ) {
            let fresh = self
                .runtime
                .authorize_scope(context, &tx.scope, purpose)
                .await?;
            let payload = serde_json::to_value(&input).map_err(|_| WorkbenchError::InvalidInput)?;
            let ctx = context.clone();
            let scope = tx.scope.clone();
            let id = input.transaction_id;
            let status = self
                .jcall(move |journal| {
                    let old = journal.get(&ctx, &scope, id)?;
                    journal.request_commit(&ctx, &token, &payload, &fresh, old.version.value())
                })
                .await?;
            if status.permit.is_some() {
                return Err(WorkbenchError::StoreUnavailable);
            }
            return Ok(status.admission);
        }
        if state != TransactionState::Active || tx.finalizing.swap(true, Ordering::SeqCst) {
            return Err(WorkbenchError::TargetBusy);
        }
        // Fail an already-expired authority check before touching the physical
        // finalization barrier; native rollback remains available to its owner.
        if let Err(error) = self
            .runtime
            .binding_receipt(context, input.session, purpose)
            .await
        {
            tx.finalizing.store(false, Ordering::SeqCst);
            return Err(error);
        }
        let coordinator = self.clone();
        let ctx = context.clone();
        let native = tx.clone();
        let payload = serde_json::to_value(&input).map_err(|_| WorkbenchError::InvalidInput)?;
        let session_mode = self.runtime.get(context, input.session)?.mode;
        let outcome = self
            .runtime
            .with_lease_final(context, &tx.lease, purpose, move |connection| {
                Box::pin(async move {
                    let fresh = coordinator
                        .runtime
                        .binding_receipt(&ctx, input.session, purpose)
                        .await?;
                    let jctx = ctx.clone();
                    let jscope = native.scope.clone();
                    let id = input.transaction_id;
                    let old = coordinator
                        .jcall(move |journal| journal.get(&jctx, &jscope, id))
                        .await?;
                    let jctx = ctx.clone();
                    let admission = coordinator
                        .jcall(move |journal| {
                            journal.request_commit(
                                &jctx,
                                &token,
                                &payload,
                                &fresh,
                                old.version.value(),
                            )
                        })
                        .await?;
                    if admission.permit.is_none() {
                        return Ok((admission.admission, false));
                    }
                    native
                        .transaction
                        .lock()
                        .map_err(|_| WorkbenchError::Io)?
                        .state = TransactionState::CommitRequested;
                    connection
                        .transaction_control(TransactionCommand::Commit)
                        .await?;
                    let jctx = ctx.clone();
                    let jscope = native.scope.clone();
                    let version = admission.admission.version.value();
                    let committed = coordinator
                        .jcall(move |journal| {
                            journal.transition(
                                &jctx,
                                &jscope,
                                id,
                                version,
                                Transition::CommitConfirmed,
                            )
                        })
                        .await?;
                    let reusable = connection
                        .set_read_only(session_mode == Mode::ReadOnly)
                        .await
                        .is_ok();
                    Ok((
                        Admission {
                            operation_id: id,
                            version: committed.version,
                            deduplicated: false,
                        },
                        reusable,
                    ))
                })
            })
            .await;
        match outcome {
            Ok((admission, reusable)) => {
                tx.transaction.lock().map_err(|_| WorkbenchError::Io)?.state =
                    if reusable || !admission.deduplicated {
                        TransactionState::Committed
                    } else {
                        TransactionState::OutcomeUnknown
                    };
                self.runtime
                    .finish_lease(context, &tx.lease, reusable)
                    .await?;
                Ok(admission)
            }
            Err(error) => {
                if error == WorkbenchError::TargetBusy
                    && self
                        .runtime
                        .get(context, input.session)
                        .is_ok_and(|view| view.state != SessionState::Retiring)
                {
                    tx.finalizing.store(false, Ordering::SeqCst);
                    return Err(error);
                }
                tx.transaction.lock().map_err(|_| WorkbenchError::Io)?.state =
                    TransactionState::OutcomeUnknown;
                let ctx = context.clone();
                let scope = tx.scope.clone();
                let id = input.transaction_id;
                let _ = self
                    .jcall(move |journal| {
                        let old = journal.get(&ctx, &scope, id)?;
                        if matches!(
                            old.state,
                            OperationState::CommitRequested
                                | OperationState::Running
                                | OperationState::Dispatching
                        ) {
                            journal.transition(
                                &ctx,
                                &scope,
                                id,
                                old.version.value(),
                                Transition::EffectsUnknown,
                            )
                        } else {
                            Ok(old)
                        }
                    })
                    .await;
                let _ = self.runtime.finish_lease(context, &tx.lease, false).await;
                Err(error)
            }
        }
    }
    pub async fn transaction_rollback(
        self: &Arc<Self>,
        context: &InvocationContext,
        input: TransactionInput,
    ) -> Result<Transaction, WorkbenchError> {
        let tx = self.transaction(context, &input)?;
        let current = tx
            .transaction
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .clone();
        if current.state == TransactionState::RolledBack {
            return Ok(current);
        }
        if matches!(
            current.state,
            TransactionState::Committed
                | TransactionState::OutcomeUnknown
                | TransactionState::CommitRequested
        ) {
            return Err(WorkbenchError::OutcomeUnknown);
        }
        if tx.finalizing.swap(true, Ordering::SeqCst) {
            return Err(WorkbenchError::TargetBusy);
        }
        tx.transaction.lock().map_err(|_| WorkbenchError::Io)?.state =
            TransactionState::RollbackRequested;
        let coordinator = self.clone();
        let ctx = context.clone();
        let native = tx.clone();
        let mode = self
            .runtime
            .get(context, input.session)
            .map_or(Mode::ReadOnly, |session| session.mode);
        let result = self
            .runtime
            .with_lease_cleanup(context, &tx.lease, move |connection| {
                Box::pin(async move {
                    connection
                        .transaction_control(TransactionCommand::Rollback)
                        .await?;
                    let jctx = ctx.clone();
                    let scope = native.scope.clone();
                    let id = input.transaction_id;
                    coordinator
                        .jcall(move |journal| {
                            let old = journal.get(&jctx, &scope, id)?;
                            journal.transition(
                                &jctx,
                                &scope,
                                id,
                                old.version.value(),
                                Transition::RollbackConfirmed,
                            )
                        })
                        .await?;
                    Ok(connection
                        .set_read_only(mode == Mode::ReadOnly)
                        .await
                        .is_ok())
                })
            })
            .await;
        let reusable = result.as_ref().copied().unwrap_or(false);
        let _ = self
            .runtime
            .finish_lease(context, &tx.lease, reusable)
            .await;
        let answer = {
            let mut current = tx.transaction.lock().map_err(|_| WorkbenchError::Io)?;
            current.state = if result.is_ok() {
                TransactionState::RolledBack
            } else {
                TransactionState::OutcomeUnknown
            };
            current.clone()
        };
        if result.is_err() {
            let ctx = context.clone();
            let scope = tx.scope.clone();
            let id = input.transaction_id;
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
        Ok(answer)
    }
    pub async fn expire_transactions(self: &Arc<Self>) -> Result<usize, WorkbenchError> {
        let expired = self
            .transactions
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .values()
            .filter(|tx| !tx.finalizing.load(Ordering::SeqCst))
            .cloned()
            .collect::<Vec<_>>();
        let mut count = 0;
        for tx in expired {
            let transaction = tx
                .transaction
                .lock()
                .map_err(|_| WorkbenchError::Io)?
                .clone();
            if matches!(
                transaction.state,
                TransactionState::Active | TransactionState::FailedNeedsRollback
            ) {
                let context = InvocationContext::new(self.runtime.principal().clone(), tx.window);
                if self
                    .runtime
                    .get(&context, transaction.session)
                    .map_or(true, |session| session.state == SessionState::Retiring)
                {
                    self.mark_manual_unknown(&context, &tx).await?;
                    count += 1;
                    continue;
                }
                if !tx.lease.expired() {
                    continue;
                }
                let _ = self
                    .transaction_rollback(
                        &context,
                        TransactionInput {
                            session: transaction.session,
                            transaction_id: transaction.transaction_id,
                        },
                    )
                    .await;
                count += 1;
            }
        }
        Ok(count)
    }
    pub async fn retire_window(
        self: &Arc<Self>,
        context: &InvocationContext,
    ) -> Result<(), WorkbenchError> {
        self.context(context)?;
        let runs = self
            .runs
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .values()
            .filter(|run| {
                run.window == context.window_instance_id() && !run.finished.load(Ordering::SeqCst)
            })
            .cloned()
            .collect::<Vec<_>>();
        for run in runs {
            let _ = self
                .cancel(
                    context,
                    QueryCancelInput {
                        session: run.session,
                        query_run_id: run.id,
                        execution_epoch: run.reservation.epoch().into(),
                    },
                )
                .await;
        }
        let transactions = self
            .transactions
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .values()
            .filter(|tx| tx.window == context.window_instance_id())
            .cloned()
            .collect::<Vec<_>>();
        for tx in transactions {
            let transaction = tx
                .transaction
                .lock()
                .map_err(|_| WorkbenchError::Io)?
                .clone();
            if matches!(
                transaction.state,
                TransactionState::Active | TransactionState::FailedNeedsRollback
            ) {
                let _ = self
                    .transaction_rollback(
                        context,
                        TransactionInput {
                            session: transaction.session,
                            transaction_id: transaction.transaction_id,
                        },
                    )
                    .await;
            }
        }
        self.reviews
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .retain(|_, review| review.window != context.window_instance_id());
        self.prepared
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .retain(|_, entry| entry.review.window != context.window_instance_id());
        self.cursors
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .retain(|_, cursor| cursor.window != context.window_instance_id());
        Ok(())
    }
    async fn mark_manual_unknown(
        &self,
        context: &InvocationContext,
        manual: &Arc<Manual>,
    ) -> Result<(), WorkbenchError> {
        let id = {
            let mut tx = manual.transaction.lock().map_err(|_| WorkbenchError::Io)?;
            if matches!(
                tx.state,
                TransactionState::Committed | TransactionState::RolledBack
            ) {
                return Ok(());
            }
            tx.state = TransactionState::OutcomeUnknown;
            tx.transaction_id
        };
        let ctx = context.clone();
        let scope = manual.scope.clone();
        let _ = self
            .jcall(move |journal| {
                let old = journal.get(&ctx, &scope, id)?;
                if matches!(
                    old.state,
                    OperationState::Running
                        | OperationState::Dispatching
                        | OperationState::CommitRequested
                ) {
                    journal.transition(
                        &ctx,
                        &scope,
                        id,
                        old.version.value(),
                        Transition::EffectsUnknown,
                    )
                } else {
                    Ok(old)
                }
            })
            .await;
        Ok(())
    }
    /// Retire one session only; the returned transaction is observed evidence.
    pub async fn retire_session(
        self: &Arc<Self>,
        context: &InvocationContext,
        session: SessionRef,
        rollback_request: bool,
    ) -> Result<Option<Transaction>, WorkbenchError> {
        self.context(context)?;
        let runs = self
            .runs
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .values()
            .filter(|run| {
                run.window == context.window_instance_id()
                    && run.session == session
                    && !run.finished.load(Ordering::SeqCst)
            })
            .cloned()
            .collect::<Vec<_>>();
        for run in runs {
            let _ = self
                .cancel(
                    context,
                    QueryCancelInput {
                        session,
                        query_run_id: run.id,
                        execution_epoch: run.reservation.epoch().into(),
                    },
                )
                .await;
        }
        let transaction = if let Some(manual) = self.manual(context, session)? {
            let before = manual
                .transaction
                .lock()
                .map_err(|_| WorkbenchError::Io)?
                .clone();
            if rollback_request
                && matches!(
                    before.state,
                    TransactionState::Active | TransactionState::FailedNeedsRollback
                )
            {
                Some(
                    self.transaction_rollback(
                        context,
                        TransactionInput {
                            session,
                            transaction_id: before.transaction_id,
                        },
                    )
                    .await?,
                )
            } else {
                self.mark_manual_unknown(context, &manual).await?;
                Some(
                    manual
                        .transaction
                        .lock()
                        .map_err(|_| WorkbenchError::Io)?
                        .clone(),
                )
            }
        } else {
            None
        };
        self.reviews
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .retain(|_, review| {
                review.window != context.window_instance_id() || review.session != session
            });
        self.prepared
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .retain(|_, entry| {
                entry.review.window != context.window_instance_id()
                    || entry.review.session != session
            });
        let closed = self.runtime.close(context, session).await;
        if matches!(
            closed,
            Err(WorkbenchError::Forbidden | WorkbenchError::Elevated)
        ) {
            closed?;
        }
        Ok(transaction)
    }
    pub async fn reconnect(
        self: &Arc<Self>,
        context: &InvocationContext,
        session: SessionRef,
        revision: &str,
    ) -> Result<Session, WorkbenchError> {
        self.context(context)?;
        let (scope, credential, mode, check) =
            if let Ok(before) = self.runtime.reconnect_basis(context, session) {
                (
                    before.scope,
                    before.credential,
                    before.mode,
                    before.restored_check,
                )
            } else {
                let manual = self
                    .transactions
                    .lock()
                    .map_err(|_| WorkbenchError::Io)?
                    .values()
                    .find(|tx| {
                        tx.window == context.window_instance_id()
                            && tx.owner == context.principal().owner_id()
                            && tx.lease.session() == session
                    })
                    .cloned();
                if let Some(tx) = manual {
                    (
                        tx.scope.clone(),
                        tx.credential,
                        tx.mode,
                        tx.restored_check.clone(),
                    )
                } else {
                    let run = self
                        .runs
                        .lock()
                        .map_err(|_| WorkbenchError::Io)?
                        .values()
                        .find(|run| {
                            run.window == context.window_instance_id()
                                && run.owner == context.principal().owner_id()
                                && run.session == session
                        })
                        .cloned()
                        .ok_or(WorkbenchError::SessionStale)?;
                    (
                        run.scope.clone(),
                        run.credential,
                        run.mode,
                        run.restored_check.clone(),
                    )
                }
            };
        self.retire_session(context, session, false).await?;
        match check {
            Some(check) => {
                self.runtime
                    .open_checked(context, scope, credential, Some(revision), check)
                    .await
            }
            None => {
                self.runtime
                    .open(context, scope, credential, Some(revision), mode)
                    .await
            }
        }
    }
}
