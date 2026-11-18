//! A native, single-use read approval. This deliberately accepts only a plain
//! projection of one fully qualified base table; a parsed SELECT is not an
//! origin or side-effect certificate.
use super::*;
use crate::browse::BrowseService;
use crate::driver::{DriverSummary, FileReadLimits};
use crate::metadata::{MetadataCatalog, RelationMetadata};
use sqlparser::dialect::{Dialect, MySqlDialect, PostgreSqlDialect, SQLiteDialect};
use sqlparser::tokenizer::{Token, Tokenizer, Whitespace};
use std::sync::atomic::AtomicU64;

pub struct ExportReadOutcome {
    pub summary: DriverSummary,
    pub consistency: Consistency,
    pub completeness: Completion,
}
/// Never deserialized and never reconstructed from a frontend SQL string.
pub struct ApprovedExportRead {
    coordinator: Arc<QueryCoordinator>,
    metadata: Arc<MetadataCatalog>,
    browse: Arc<BrowseService>,
    review: Arc<Review>,
    relation: Arc<RelationMetadata>,
    snapshot_id: Option<Uuid>,
    projection: Vec<String>,
    used: AtomicBool,
    cancelled: AtomicBool,
    finished: AtomicBool,
    epoch: AtomicU64,
}
impl QueryCoordinator {
    pub async fn approve_export_read(
        self: &Arc<Self>,
        context: &InvocationContext,
        session: SessionRef,
        review_id: Uuid,
        snapshot_id: Option<Uuid>,
        metadata: Arc<MetadataCatalog>,
        browse: Arc<BrowseService>,
    ) -> Result<Arc<ApprovedExportRead>, WorkbenchError> {
        self.context(context)?;
        let review = self
            .reviews
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .get(&review_id)
            .cloned()
            .ok_or(WorkbenchError::AttemptExpired)?;
        check_review(self, context, session, &review)?;
        if self.manual(context, session)?.is_some()
            || review.manual.is_some()
            || review.mode != Mode::ReadOnly
            || review.purpose != QueryPurpose::Run
            || review.policy.statements.len() != 1
        {
            return Err(WorkbenchError::Unsupported);
        }
        let statement = &review.policy.statements[0];
        if !statement.boundary_verified
            || !statement.read_only_candidate
            || statement.has_function_calls
            || statement.effect != EffectClass::Read
        {
            return Err(WorkbenchError::Unsupported);
        }
        review.policy.verify_source(&review.sql)?;
        let source = statement.source(&review.sql)?;
        let (schema, table, projection) = plain_projection(review.facts.engine, source)?;
        let lease = browse.snapshot_lease(context, session, snapshot_id)?;
        let relation = metadata
            .resolve_named(context, session, &schema, &table, lease.as_deref())
            .await?;
        validate_projection(&relation, &projection)?;
        let checked = relation.clone();
        let budget = self.streams.budget();
        if let Some(lease) = lease {
            self.runtime
                .with_lease(context, &lease, AuthorizePurpose::Read, move |connection| {
                    Box::pin(async move {
                        Ok(crate::generated_sql::verify_base_origin(
                            connection, &budget, &checked, false,
                        )
                        .await)
                    })
                })
                .await??;
        } else {
            self.runtime
                .with_connection(
                    context,
                    session,
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
        Ok(Arc::new(ApprovedExportRead {
            coordinator: self.clone(),
            metadata,
            browse,
            review,
            relation,
            snapshot_id,
            projection,
            used: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
        }))
    }
}
impl ApprovedExportRead {
    pub fn review_digest(&self) -> &str {
        &self.review.policy.digest
    }
    pub fn engine(&self) -> Engine {
        self.review.facts.engine
    }
    pub fn session(&self) -> SessionRef {
        self.review.session
    }
    pub fn fingerprint(&self) -> &str {
        &self.relation.object.schema_version
    }
    pub async fn cancel(
        &self,
        context: &InvocationContext,
    ) -> Result<QueryCancelResult, WorkbenchError> {
        check_owner(context, self.session(), &self.review)?;
        if self.finished.load(Ordering::SeqCst) {
            return Ok(QueryCancelResult {
                cancel_requested: false,
                settled: true,
            });
        }
        self.cancelled.store(true, Ordering::SeqCst);
        let epoch = self.epoch.load(Ordering::SeqCst);
        if epoch == 0 {
            return Ok(QueryCancelResult {
                cancel_requested: true,
                settled: false,
            });
        }
        self.coordinator
            .runtime
            .request_cancel(context, self.session(), epoch)
            .await?;
        Ok(QueryCancelResult {
            cancel_requested: true,
            settled: false,
        })
    }
    pub async fn execute(
        self: &Arc<Self>,
        context: &InvocationContext,
        sink: Box<dyn RowSink>,
        limits: FileReadLimits,
    ) -> Result<ExportReadOutcome, WorkbenchError> {
        check_review(&self.coordinator, context, self.session(), &self.review)?;
        if self.used.swap(true, Ordering::SeqCst) {
            return Err(WorkbenchError::AttemptExpired);
        }
        let snapshot = self
            .browse
            .snapshot_lease(context, self.session(), self.snapshot_id)?;
        let own_lease = snapshot.is_none();
        let lease = match snapshot {
            Some(lease) => lease,
            None => Arc::new(
                self.coordinator
                    .runtime
                    .pin_connection(
                        context,
                        self.session(),
                        ConnectionLeaseKind::ReadExport,
                        limits.timeout(),
                    )
                    .await?,
            ),
        };
        let consistency = if own_lease {
            Consistency::LivePerPage
        } else {
            Consistency::TransactionSnapshot {
                snapshot_id: lease.id(),
                expires_at_ms: lease.expires_at_ms().into(),
            }
        };
        let approval = self.clone();
        let native_context = context.clone();
        let lease_copy = lease.clone();
        let run = self
            .coordinator
            .runtime
            .with_lease(context, &lease, AuthorizePurpose::Read, move |connection| {
                Box::pin(async move {
                    let mut sink = sink;
                    let review = &approval.review;
                    let budget = approval.coordinator.streams.budget();
                    let fresh = approval
                        .coordinator
                        .runtime
                        .binding_receipt(
                            &native_context,
                            approval.session(),
                            AuthorizePurpose::Read,
                        )
                        .await?;
                    same_basis(&review.receipt, &fresh)?;
                    if lexical_settings(connection, &budget).await? != review.lexical
                        || native_session_basis(connection, &budget).await? != review.session_basis
                        || connection.facts().server_identity != review.facts.server_identity
                        || connection.facts().actual_database_identity
                            != review.facts.actual_database_identity
                    {
                        return Err(WorkbenchError::BindingStale);
                    }
                    if own_lease {
                        connection
                            .transaction_control(TransactionCommand::ReadOnlyStatementBegin)
                            .await?;
                    }
                    if !connection.transaction_read_only().await? {
                        return Err(WorkbenchError::Unsupported);
                    }
                    // A target-only generated read acquires the relation/metadata
                    // lock without evaluating any user projection or source rows.
                    let lock = if approval.engine() == Engine::Postgresql {
                        format!(
                            "LOCK TABLE ONLY {} IN ACCESS SHARE MODE",
                            approval.relation.qualified_name()?
                        )
                    } else {
                        format!(
                            "SELECT 1 FROM {} LIMIT 0",
                            approval.relation.qualified_name()?
                        )
                    };
                    crate::types::read(connection, &budget, &lock, &[], 1).await?;
                    let current = approval
                        .metadata
                        .reload_on_connection(
                            &native_context,
                            approval.session(),
                            &approval.relation.object,
                            connection,
                        )
                        .await?;
                    validate_projection(&current, &approval.projection)?;
                    crate::generated_sql::verify_base_origin(connection, &budget, &current, false)
                        .await?;
                    if lease_copy.expired() || approval.cancelled.load(Ordering::SeqCst) {
                        return Err(WorkbenchError::CancelUnsettled);
                    }
                    approval.epoch.store(
                        approval
                            .coordinator
                            .runtime
                            .get(&native_context, approval.session())?
                            .execution_epoch
                            .value(),
                        Ordering::SeqCst,
                    );
                    if approval.cancelled.load(Ordering::SeqCst) {
                        return Err(WorkbenchError::CancelUnsettled);
                    }
                    let sql = review.policy.statements[0].source(&review.sql)?;
                    let remaining =
                        Duration::from_millis(lease_copy.expires_at_ms().saturating_sub(now_ms()));
                    let duration = limits.timeout().min(remaining);
                    let summary = tokio::time::timeout(
                        duration,
                        connection.execute_file_read(sql, limits, sink.as_mut()),
                    )
                    .await
                    .map_err(|_| WorkbenchError::CancelUnsettled)??;
                    if lease_copy.expired() || approval.cancelled.load(Ordering::SeqCst) {
                        return Err(WorkbenchError::CancelUnsettled);
                    }
                    if !connection.transaction_read_only().await?
                        || summary.transaction_state != DriverTransactionState::Active
                    {
                        return Err(WorkbenchError::CancelUnsettled);
                    }
                    Ok(summary)
                })
            })
            .await;
        // Fresh standalone controls are separate from the reviewed user SQL.
        // A cleanup ACK is connection evidence, never a claim about arbitrary
        // external effects. Snapshots stay owned by their original registry.
        let result = if own_lease {
            let clean = self
                .coordinator
                .runtime
                .with_lease_cleanup(context, &lease, |connection| {
                    Box::pin(async move {
                        connection
                            .transaction_control(TransactionCommand::Rollback)
                            .await
                    })
                })
                .await;
            let reusable = run.is_ok() && clean.is_ok();
            let finished = self
                .coordinator
                .runtime
                .finish_lease(context, &lease, reusable)
                .await;
            match (run, clean, finished) {
                (Ok(summary), Ok(()), Ok(())) => Ok(summary),
                (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => Err(error),
            }
        } else {
            run
        };
        self.finished.store(true, Ordering::SeqCst);
        result.map(|summary| ExportReadOutcome {
            completeness: if summary.capped {
                Completion::Capped
            } else {
                Completion::Complete
            },
            summary,
            consistency,
        })
    }
}
fn check_owner(
    context: &InvocationContext,
    session: SessionRef,
    review: &Review,
) -> Result<(), WorkbenchError> {
    if context.principal().owner_id() != review.owner
        || context.window_instance_id() != review.window
        || session != review.session
    {
        Err(WorkbenchError::Forbidden)
    } else {
        Ok(())
    }
}
fn check_review(
    coordinator: &QueryCoordinator,
    context: &InvocationContext,
    session: SessionRef,
    review: &Review,
) -> Result<(), WorkbenchError> {
    check_owner(context, session, review)?;
    if review.created.elapsed() >= Duration::from_secs(60) {
        return Err(WorkbenchError::AttemptExpired);
    }
    let current = coordinator.runtime.get(context, session)?;
    if current.mode != Mode::ReadOnly
        || current.mode != review.mode
        || current.scope != review.scope
        || current.descriptor_revision != review.receipt.descriptor_revision
    {
        return Err(WorkbenchError::BindingStale);
    }
    review.policy.verify_source(&review.sql)
}
fn validate_projection(
    relation: &RelationMetadata,
    projection: &[String],
) -> Result<(), WorkbenchError> {
    if relation.kind != "table"
        || matches!(relation.engine, Engine::Mysql | Engine::Mariadb)
            && relation.storage_engine.as_deref() != Some("InnoDB")
    {
        return Err(WorkbenchError::Unsupported);
    }
    let columns = if projection.is_empty() {
        relation.columns.iter().collect::<Vec<_>>()
    } else {
        projection
            .iter()
            .map(|name| {
                relation
                    .columns
                    .iter()
                    .find(|column| column.column.name == *name)
                    .ok_or(WorkbenchError::Unsupported)
            })
            .collect::<Result<Vec<_>, _>>()?
    };
    if columns.is_empty() || columns.len() > 512 {
        return Err(WorkbenchError::Unsupported);
    }
    for column in columns {
        let kind = column.declared_type.to_ascii_lowercase();
        if column.generated
            || relation.engine == Engine::Postgresql
                && (column.native_type_schema.as_deref() != Some("pg_catalog")
                    || !column
                        .native_type_id
                        .is_some_and(|oid| crate::types::postgresql_builtin_type_id(&kind, oid)))
            || matches!(relation.engine, Engine::Mysql | Engine::Mariadb) && kind == "timestamp"
            || !matches!(
                kind.as_str(),
                "bool"
                    | "boolean"
                    | "int2"
                    | "int4"
                    | "int8"
                    | "integer"
                    | "int"
                    | "bigint"
                    | "smallint"
                    | "tinyint"
                    | "mediumint"
                    | "text"
                    | "varchar"
                    | "char"
                    | "bpchar"
                    | "longtext"
                    | "mediumtext"
                    | "tinytext"
                    | "blob"
                    | "binary"
                    | "varbinary"
                    | "longblob"
                    | "mediumblob"
                    | "tinyblob"
                    | "bytea"
                    | "numeric"
                    | "decimal"
                    | "real"
                    | "float"
                    | "double"
                    | "float4"
                    | "float8"
                    | "date"
                    | "datetime"
                    | "time"
                    | "timestamp"
                    | "timestamptz"
                    | "timetz"
            )
        {
            return Err(WorkbenchError::Unsupported);
        }
    }
    Ok(())
}
fn plain_projection(
    engine: Engine,
    sql: &str,
) -> Result<(String, String, Vec<String>), WorkbenchError> {
    let dialect: Box<dyn Dialect> = match engine {
        Engine::Postgresql => Box::new(PostgreSqlDialect {}),
        Engine::Sqlite => Box::new(SQLiteDialect {}),
        _ => Box::new(MySqlDialect {}),
    };
    let tokens = Tokenizer::new(dialect.as_ref(), sql)
        .tokenize()
        .map_err(|_| WorkbenchError::Unsupported)?;
    let tokens = tokens
        .into_iter()
        .filter(|token| {
            !matches!(
                token,
                Token::Whitespace(Whitespace::Space | Whitespace::Tab | Whitespace::Newline)
            )
        })
        .collect::<Vec<_>>();
    let mut parser = Projection {
        tokens: &tokens,
        index: 0,
        engine,
    };
    parser.keyword("SELECT")?;
    let mut projection = Vec::new();
    if parser.peek() == Some(&Token::Mul) {
        parser.index += 1;
    } else {
        loop {
            if projection.len() >= 512 {
                return Err(WorkbenchError::Limit);
            }
            projection.push(parser.identifier()?);
            if parser.peek() != Some(&Token::Comma) {
                break;
            }
            parser.index += 1;
        }
    }
    parser.keyword("FROM")?;
    let schema = parser.identifier()?;
    if parser.peek() != Some(&Token::Period) {
        return Err(WorkbenchError::Unsupported);
    }
    parser.index += 1;
    let table = parser.identifier()?;
    if parser.peek() == Some(&Token::SemiColon) {
        parser.index += 1;
    }
    if parser.index != tokens.len() {
        return Err(WorkbenchError::Unsupported);
    }
    Ok((schema, table, projection))
}
struct Projection<'a> {
    tokens: &'a [Token],
    index: usize,
    engine: Engine,
}
impl Projection<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.index)
    }
    fn keyword(&mut self, expected: &str) -> Result<(), WorkbenchError> {
        match self.peek() {
            Some(Token::Word(word))
                if word.quote_style.is_none() && word.value.eq_ignore_ascii_case(expected) =>
            {
                self.index += 1;
                Ok(())
            }
            _ => Err(WorkbenchError::Unsupported),
        }
    }
    fn identifier(&mut self) -> Result<String, WorkbenchError> {
        let value = match self.peek() {
            Some(Token::Word(word)) => {
                if word.quote_style.is_none() && self.engine == Engine::Postgresql {
                    word.value.to_ascii_lowercase()
                } else {
                    word.value.clone()
                }
            }
            _ => return Err(WorkbenchError::Unsupported),
        };
        if value.is_empty() || value.len() > 256 || value.contains('\0') {
            return Err(WorkbenchError::Unsupported);
        }
        self.index += 1;
        Ok(value)
    }
}
