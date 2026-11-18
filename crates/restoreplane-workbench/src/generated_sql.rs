//! Closed generated mutations. SQL text is never accepted as native authority.
use crate::driver::{DriverConnection, DriverFacts, DriverSummary, QueryLimits, RowSink};
use crate::metadata::{ColumnMetadata, MetadataCatalog, RelationMetadata};
use crate::result_stream::NativeBudget;
use crate::session::{ConnectionLease, ConnectionLeaseKind};
use crate::types::{placeholder, quote_identifier, validate_cells};
use crate::{InvocationContext, WorkbenchError, WorkbenchState};
use futures::future::BoxFuture;
use restoreplane_domain::workbench::{
    AuthorizePurpose, BindingReceipt, Column, Engine, Mode, SessionRef, TypedCell,
};
use restoreplane_domain::workbench_metadata::ObjectRef;
use sqlparser::dialect::{Dialect, MySqlDialect, PostgreSqlDialect, SQLiteDialect};
use sqlparser::tokenizer::{Token, Tokenizer, Whitespace};
use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use uuid::Uuid;

/// Only native build composition may inject independently executed evidence.
#[derive(Clone)]
pub struct WriteCertification {
    evidence: String,
    versions: Vec<(Engine, String)>,
}
impl WriteCertification {
    pub fn from_verified_build(
        evidence: String,
        versions: Vec<(Engine, String)>,
    ) -> Result<Self, WorkbenchError> {
        if evidence.is_empty()
            || evidence.len() > 256
            || versions.len() > 4
            || versions.iter().any(|(_, v)| v.is_empty() || v.len() > 512)
        {
            return Err(WorkbenchError::InvalidInput);
        }
        Ok(Self { evidence, versions })
    }
    pub fn supports(&self, facts: &DriverFacts) -> bool {
        !self.evidence.is_empty()
            && self
                .versions
                .iter()
                .any(|(e, v)| *e == facts.engine && v == &facts.server_version)
    }
}

/// Standalone physical pin, fresh binding and schema authority. No serde or
/// public field constructor exists. Forgetting cleanup retires its connection.
pub struct FreshWriteGate {
    runtime: Arc<WorkbenchState>,
    metadata: Arc<MetadataCatalog>,
    context: InvocationContext,
    lease: ConnectionLease,
    receipt: BindingReceipt,
    relation: Arc<RelationMetadata>,
    facts: DriverFacts,
    begun: AtomicBool,
}
impl FreshWriteGate {
    pub async fn acquire(
        metadata: Arc<MetadataCatalog>,
        context: &InvocationContext,
        session: SessionRef,
        object: &ObjectRef,
        certification: &WriteCertification,
    ) -> Result<Arc<Self>, WorkbenchError> {
        let runtime = metadata.runtime.clone();
        if runtime.get(context, session)?.mode == Mode::ReadOnly {
            return Err(WorkbenchError::Forbidden);
        }
        let receipt = runtime
            .binding_receipt(context, session, AuthorizePurpose::Mutate)
            .await?;
        let lease = runtime
            .pin_connection(
                context,
                session,
                ConnectionLeaseKind::StandaloneWrite,
                Duration::from_secs(30),
            )
            .await?;
        let relation = match metadata
            .resolve_with_lease(context, session, object, Some(&lease))
            .await
        {
            Ok(relation) => relation,
            Err(error) => {
                if error == WorkbenchError::CursorInvalid {
                    release_idle(&runtime, context, &lease).await?;
                } else {
                    let _ = runtime.finish_lease(context, &lease, false).await;
                }
                return Err(error);
            }
        };
        let facts = runtime
            .with_lease(context, &lease, AuthorizePurpose::Read, |connection| {
                Box::pin(async move { Ok(connection.facts().clone()) })
            })
            .await?;
        if !certification.supports(&facts)
            || relation.kind != "table"
            || relation.stable_key().is_none()
            || (matches!(facts.engine, Engine::Mysql | Engine::Mariadb)
                && relation.schema != facts.context.database)
        {
            release_idle(&runtime, context, &lease).await?;
            return Err(WorkbenchError::Unsupported);
        }
        Ok(Arc::new(Self {
            runtime,
            metadata,
            context: context.clone(),
            lease,
            receipt,
            relation,
            facts,
            begun: AtomicBool::new(false),
        }))
    }
    pub fn relation(&self) -> &Arc<RelationMetadata> {
        &self.relation
    }
    pub fn receipt(&self) -> &BindingReceipt {
        &self.receipt
    }
    pub fn lease(&self) -> &ConnectionLease {
        &self.lease
    }
    /// Call only after the owning journal has persisted dispatch intent. SQLite
    /// uses BEGIN IMMEDIATE; this cannot piggyback on an existing transaction.
    pub async fn begin_transaction(&self) -> Result<(), WorkbenchError> {
        if self.begun.swap(true, Ordering::SeqCst) {
            return Err(WorkbenchError::TargetBusy);
        }
        self.with_connection(|connection| {
            Box::pin(async move {
                connection.configure_limits(Duration::from_secs(2)).await?;
                connection
                    .transaction_control(crate::driver::TransactionCommand::StandaloneWriteBegin)
                    .await
            })
        })
        .await
    }
    pub async fn with_connection<T: Send, F>(&self, action: F) -> Result<T, WorkbenchError>
    where
        F: for<'a> FnOnce(&'a mut dyn DriverConnection) -> BoxFuture<'a, Result<T, WorkbenchError>>
            + Send,
    {
        self.runtime
            .with_lease(&self.context, &self.lease, AuthorizePurpose::Mutate, action)
            .await
    }
    pub async fn with_commit<T: Send, F>(&self, action: F) -> Result<T, WorkbenchError>
    where
        F: for<'a> FnOnce(&'a mut dyn DriverConnection) -> BoxFuture<'a, Result<T, WorkbenchError>>
            + Send,
    {
        self.runtime
            .with_lease_final(&self.context, &self.lease, AuthorizePurpose::Commit, action)
            .await
    }
    pub async fn with_cleanup<T: Send, F>(&self, action: F) -> Result<T, WorkbenchError>
    where
        F: for<'a> FnOnce(&'a mut dyn DriverConnection) -> BoxFuture<'a, Result<T, WorkbenchError>>
            + Send,
    {
        self.runtime
            .with_lease_cleanup(&self.context, &self.lease, action)
            .await
    }
    pub async fn finish(&self, reusable: bool) -> Result<(), WorkbenchError> {
        if reusable && !self.begun.load(Ordering::SeqCst) {
            return release_idle(&self.runtime, &self.context, &self.lease).await;
        }
        self.runtime
            .finish_lease(&self.context, &self.lease, reusable)
            .await
    }
    pub async fn validate_physical(
        &self,
        connection: &mut dyn DriverConnection,
    ) -> Result<(), WorkbenchError> {
        if !self.begun.load(Ordering::SeqCst)
            || self.lease.expired()
            || connection.facts() != &self.facts
        {
            return Err(WorkbenchError::BindingStale);
        }
        let fresh = self
            .runtime
            .binding_receipt(
                &self.context,
                self.lease.session(),
                AuthorizePurpose::Mutate,
            )
            .await?;
        if fresh.scope != self.receipt.scope
            || fresh.descriptor != self.receipt.descriptor
            || fresh.descriptor_revision != self.receipt.descriptor_revision
            || fresh.authority_epoch != self.receipt.authority_epoch
        {
            return Err(WorkbenchError::BindingStale);
        }
        let mut probe = Discard;
        let summary = connection
            .execute(
                "SELECT 1",
                QueryLimits {
                    max_rows: 1,
                    max_bytes: 1024,
                    timeout: Duration::from_secs(2),
                },
                &mut probe,
            )
            .await?;
        if summary.transaction_state != crate::driver::DriverTransactionState::Active {
            return Err(WorkbenchError::Unsupported);
        }
        let lock = match self.facts.engine {
            Engine::Postgresql => format!(
                "LOCK TABLE {} IN SHARE UPDATE EXCLUSIVE MODE",
                self.relation.qualified_name()?
            ),
            Engine::Mysql | Engine::Mariadb => format!(
                "SELECT {} FROM {} WHERE 1=0 FOR UPDATE",
                quote_identifier(self.facts.engine, &self.relation.columns[0].column.name)?,
                self.relation.qualified_name()?
            ),
            Engine::Sqlite => String::new(),
        };
        let budget = self.metadata.budget();
        if !lock.is_empty() {
            crate::types::read(connection, &budget, &lock, &[], 1).await?;
        }
        let current = self
            .metadata
            .reload_on_connection(
                &self.context,
                self.lease.session(),
                &self.relation.object,
                connection,
            )
            .await?;
        verify_transactional_table(connection, &budget, &current).await?;
        Ok(())
    }
}
async fn release_idle(
    runtime: &WorkbenchState,
    context: &InvocationContext,
    lease: &ConnectionLease,
) -> Result<(), WorkbenchError> {
    let idle = runtime
        .with_lease_cleanup(context, lease, |connection| {
            Box::pin(async move {
                let summary = connection
                    .execute(
                        "SELECT 1",
                        QueryLimits {
                            max_rows: 1,
                            max_bytes: 1024,
                            timeout: Duration::from_secs(2),
                        },
                        &mut Discard,
                    )
                    .await?;
                if summary.transaction_state == crate::driver::DriverTransactionState::Idle {
                    Ok(())
                } else {
                    Err(WorkbenchError::OutcomeUnknown)
                }
            })
        })
        .await;
    runtime.finish_lease(context, lease, idle.is_ok()).await?;
    idle
}
#[derive(Clone, Copy)]
pub struct NativeInsertLimits {
    pub max_rows: usize,
    pub max_parameter_bytes: usize,
}
impl NativeInsertLimits {
    pub fn editing() -> Self {
        Self {
            max_rows: 100,
            max_parameter_bytes: 512 * 1024,
        }
    }
    pub fn importing() -> Self {
        Self {
            max_rows: 200,
            max_parameter_bytes: 256 * 1024,
        }
    }
}
pub struct GeneratedInsert {
    relation: Arc<RelationMetadata>,
    columns: Vec<Uuid>,
    rows: usize,
    sql: String,
    parameters: Vec<TypedCell>,
}
impl GeneratedInsert {
    pub fn preview(&self) -> &str {
        &self.sql
    }
    pub fn parameters(&self) -> &[TypedCell] {
        &self.parameters
    }
    pub fn rows(&self) -> usize {
        self.rows
    }
    pub async fn execute(
        &self,
        connection: &mut dyn DriverConnection,
        gate: &FreshWriteGate,
        limits: QueryLimits,
        sink: &mut dyn RowSink,
    ) -> Result<DriverSummary, WorkbenchError> {
        if self.relation.object != gate.relation.object || self.relation.engine != gate.facts.engine
        {
            return Err(WorkbenchError::Forbidden);
        }
        validate_generated_insert(&self.sql, &self.relation, &self.columns, self.rows)?;
        gate.validate_physical(connection).await?;
        connection
            .execute_bound(&self.sql, &self.parameters, limits, sink)
            .await
    }
}
pub fn prepare_insert(
    relation: Arc<RelationMetadata>,
    destination: &[Uuid],
    rows: &[Vec<TypedCell>],
    limits: NativeInsertLimits,
) -> Result<GeneratedInsert, WorkbenchError> {
    if rows.is_empty()
        || rows.len() > limits.max_rows
        || limits.max_rows > 200
        || limits.max_parameter_bytes > 512 * 1024
        || destination.is_empty()
        || destination.len() > 512
        || rows.len().saturating_mul(destination.len()) > 16384
    {
        return Err(WorkbenchError::Limit);
    }
    if relation.kind != "table" || relation.stable_key().is_none() {
        return Err(WorkbenchError::Unsupported);
    }
    if matches!(relation.engine, Engine::Mysql | Engine::Mariadb)
        && std::iter::once(&relation.schema)
            .chain(std::iter::once(&relation.name))
            .chain(relation.columns.iter().map(|c| &c.column.name))
            .any(|name| name.contains('\\'))
    {
        return Err(WorkbenchError::Unsupported);
    }
    if relation
        .stable_key()
        .is_some_and(|key| key.columns.iter().any(|id| !destination.contains(id)))
    {
        return Err(WorkbenchError::Unsupported);
    }
    let mut seen = HashSet::new();
    let mut columns = Vec::new();
    for id in destination {
        if !seen.insert(*id) {
            return Err(WorkbenchError::InvalidInput);
        }
        let column = relation
            .columns
            .iter()
            .find(|c| c.column.column_id == *id)
            .ok_or(WorkbenchError::InvalidInput)?;
        if column.generated {
            return Err(WorkbenchError::Unsupported);
        }
        columns.push(column);
    }
    // Omission has explicit reviewed default semantics. First certification
    // permits only nullable/no-default omissions; generated/default expressions
    // require a separately certified retrieval/effect path.
    if relation.columns.iter().any(|column| {
        !seen.contains(&column.column.column_id)
            && (column.generated
                || (column.defaulted && !column.null_default)
                || !column.column.nullable)
    }) {
        return Err(WorkbenchError::Unsupported);
    }
    let mut parameters = Vec::new();
    let mut size = 0usize;
    for row in rows {
        if row.len() != columns.len() {
            return Err(WorkbenchError::InvalidInput);
        }
        validate_cells(row)?;
        for (cell, column) in row.iter().zip(&columns) {
            if matches!(cell, TypedCell::Null)
                && relation
                    .stable_key()
                    .is_some_and(|key| key.columns.contains(&column.column.column_id))
            {
                return Err(WorkbenchError::Unsupported);
            }
            validate_column_cell(relation.engine, column, cell)?;
            if relation
                .stable_key()
                .is_some_and(|key| key.columns.contains(&column.column.column_id))
                && !exact_identity_cell(cell)
            {
                return Err(WorkbenchError::Unsupported);
            }
            size = size.saturating_add(
                serde_json::to_vec(cell)
                    .map_err(|_| WorkbenchError::InvalidInput)?
                    .len(),
            );
            if size > limits.max_parameter_bytes {
                return Err(WorkbenchError::Limit);
            }
            parameters.push(cell.clone());
        }
    }
    let names = columns
        .iter()
        .map(|column| quote_identifier(relation.engine, &column.column.name))
        .collect::<Result<Vec<_>, _>>()?
        .join(",");
    let mut ordinal = 0;
    let values = rows
        .iter()
        .map(|row| {
            format!(
                "({})",
                row.iter()
                    .map(|_| {
                        ordinal += 1;
                        placeholder(relation.engine, ordinal)
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "INSERT INTO {} ({names}) VALUES {values}",
        relation.qualified_name()?
    );
    validate_generated_insert(&sql, &relation, destination, rows.len())?;
    Ok(GeneratedInsert {
        relation,
        columns: destination.to_vec(),
        rows: rows.len(),
        sql,
        parameters,
    })
}

/// Independent token grammar: no SQL generator is called here. Quoted native
/// identifiers, one exact target, ordered columns and numbered binds are the
/// entire accepted language; comments, expressions and extra statements fail.
pub fn validate_generated_insert(
    sql: &str,
    relation: &RelationMetadata,
    columns: &[Uuid],
    rows: usize,
) -> Result<(), WorkbenchError> {
    if sql.len() > 1024 * 1024
        || rows == 0
        || rows > 200
        || columns.is_empty()
        || columns.len() > 512
    {
        return Err(WorkbenchError::Limit);
    }
    let dialect: Box<dyn Dialect> = match relation.engine {
        Engine::Postgresql => Box::new(PostgreSqlDialect {}),
        Engine::Sqlite => Box::new(SQLiteDialect {}),
        _ => Box::new(MySqlDialect {}),
    };
    let tokens = Tokenizer::new(dialect.as_ref(), sql)
        .tokenize()
        .map_err(|_| WorkbenchError::InvalidInput)?;
    let mut filtered = Vec::new();
    for token in tokens {
        match token {
            Token::Whitespace(Whitespace::Space | Whitespace::Tab | Whitespace::Newline) => {}
            Token::Whitespace(_) => return Err(WorkbenchError::Forbidden),
            token => filtered.push(token),
        }
    }
    let mut cursor = Grammar {
        tokens: &filtered,
        index: 0,
    };
    cursor.keyword("INSERT")?;
    cursor.keyword("INTO")?;
    cursor.identifier(&relation.schema, relation.engine)?;
    cursor.take(&Token::Period)?;
    cursor.identifier(&relation.name, relation.engine)?;
    cursor.take(&Token::LParen)?;
    for (index, id) in columns.iter().enumerate() {
        if index > 0 {
            cursor.take(&Token::Comma)?;
        }
        let column = relation
            .columns
            .iter()
            .find(|column| column.column.column_id == *id)
            .ok_or(WorkbenchError::Forbidden)?;
        cursor.identifier(&column.column.name, relation.engine)?;
    }
    cursor.take(&Token::RParen)?;
    cursor.keyword("VALUES")?;
    let mut ordinal = 0;
    for row in 0..rows {
        if row > 0 {
            cursor.take(&Token::Comma)?;
        }
        cursor.take(&Token::LParen)?;
        for column in 0..columns.len() {
            if column > 0 {
                cursor.take(&Token::Comma)?;
            }
            ordinal += 1;
            cursor.take(&Token::Placeholder(placeholder(relation.engine, ordinal)))?;
        }
        cursor.take(&Token::RParen)?;
    }
    if cursor.index != filtered.len() {
        return Err(WorkbenchError::Forbidden);
    }
    Ok(())
}
struct Grammar<'a> {
    tokens: &'a [Token],
    index: usize,
}
pub(crate) struct GeneratedMutation {
    pub sql: String,
    pub parameters: Vec<TypedCell>,
    relation: Arc<RelationMetadata>,
    columns: Vec<Uuid>,
    delete: bool,
}
impl GeneratedMutation {
    pub async fn execute(
        &self,
        connection: &mut dyn DriverConnection,
        gate: &FreshWriteGate,
        limits: QueryLimits,
        sink: &mut dyn RowSink,
    ) -> Result<DriverSummary, WorkbenchError> {
        if self.relation.object != gate.relation.object {
            return Err(WorkbenchError::Forbidden);
        }
        validate_generated_mutation(&self.sql, &self.relation, &self.columns, self.delete)?;
        gate.validate_physical(connection).await?;
        connection
            .execute_bound(&self.sql, &self.parameters, limits, sink)
            .await
    }
}
pub(crate) fn prepare_mutation(
    relation: Arc<RelationMetadata>,
    values: &[(Uuid, TypedCell)],
    key_values: &[TypedCell],
    delete: bool,
) -> Result<GeneratedMutation, WorkbenchError> {
    let key = relation.stable_key().ok_or(WorkbenchError::Unsupported)?;
    if key.columns.len() != key_values.len()
        || (!delete && values.is_empty())
        || delete && !values.is_empty()
    {
        return Err(WorkbenchError::InvalidInput);
    }
    let mut columns = Vec::new();
    let mut parameters = Vec::new();
    let mut assignments = Vec::new();
    for (id, cell) in values {
        if columns.contains(id) {
            return Err(WorkbenchError::InvalidInput);
        }
        let column = relation
            .columns
            .iter()
            .find(|c| c.column.column_id == *id)
            .ok_or(WorkbenchError::InvalidInput)?;
        validate_column_cell(relation.engine, column, cell)?;
        if matches!(cell, TypedCell::Null) && key.columns.contains(id) {
            return Err(WorkbenchError::Unsupported);
        }
        columns.push(*id);
        parameters.push(cell.clone());
        assignments.push(format!(
            "{}={}",
            quote_identifier(relation.engine, &column.column.name)?,
            placeholder(relation.engine, parameters.len())
        ));
    }
    let mut predicates = Vec::new();
    for (id, cell) in key.columns.iter().zip(key_values) {
        let column = relation
            .columns
            .iter()
            .find(|c| c.column.column_id == *id)
            .ok_or(WorkbenchError::InvalidInput)?;
        validate_column_cell(relation.engine, column, cell)?;
        if !exact_identity_cell(cell) {
            return Err(WorkbenchError::Unsupported);
        }
        parameters.push(cell.clone());
        let operator = if relation.engine == Engine::Postgresql {
            " OPERATOR(pg_catalog.=) "
        } else {
            "="
        };
        predicates.push(format!(
            "{}{operator}{}",
            quote_identifier(relation.engine, &column.column.name)?,
            placeholder(relation.engine, parameters.len())
        ));
    }
    let sql = if delete {
        format!(
            "DELETE FROM {} WHERE {}",
            relation.qualified_name()?,
            predicates.join(" AND ")
        )
    } else {
        format!(
            "UPDATE {} SET {} WHERE {}",
            relation.qualified_name()?,
            assignments.join(","),
            predicates.join(" AND ")
        )
    };
    crate::driver::validate_statement(&sql, &parameters)?;
    validate_generated_mutation(&sql, &relation, &columns, delete)?;
    Ok(GeneratedMutation {
        sql,
        parameters,
        relation,
        columns,
        delete,
    })
}
pub fn exact_identity_cell(cell: &TypedCell) -> bool {
    matches!(
        cell,
        TypedCell::I64 { .. }
            | TypedCell::U64 { .. }
            | TypedCell::Decimal { .. }
            | TypedCell::Bytes { .. }
    ) && validate_cells(std::slice::from_ref(cell)).is_ok()
}
pub fn validate_generated_mutation(
    sql: &str,
    relation: &RelationMetadata,
    columns: &[Uuid],
    delete: bool,
) -> Result<(), WorkbenchError> {
    if sql.len() > 1024 * 1024 || columns.len() > 512 {
        return Err(WorkbenchError::Limit);
    }
    let dialect: Box<dyn Dialect> = match relation.engine {
        Engine::Postgresql => Box::new(PostgreSqlDialect {}),
        Engine::Sqlite => Box::new(SQLiteDialect {}),
        _ => Box::new(MySqlDialect {}),
    };
    let tokens = Tokenizer::new(dialect.as_ref(), sql)
        .tokenize()
        .map_err(|_| WorkbenchError::InvalidInput)?;
    let mut filtered = Vec::new();
    for token in tokens {
        match token {
            Token::Whitespace(Whitespace::Space | Whitespace::Tab | Whitespace::Newline) => {}
            Token::Whitespace(_) => return Err(WorkbenchError::Forbidden),
            token => filtered.push(token),
        }
    }
    let mut grammar = Grammar {
        tokens: &filtered,
        index: 0,
    };
    if delete {
        grammar.keyword("DELETE")?;
        grammar.keyword("FROM")?;
    } else {
        grammar.keyword("UPDATE")?;
    }
    grammar.identifier(&relation.schema, relation.engine)?;
    grammar.take(&Token::Period)?;
    grammar.identifier(&relation.name, relation.engine)?;
    let mut ordinal = 0;
    if !delete {
        if columns.is_empty() {
            return Err(WorkbenchError::Forbidden);
        }
        grammar.keyword("SET")?;
        for (index, id) in columns.iter().enumerate() {
            if index > 0 {
                grammar.take(&Token::Comma)?;
            }
            let column = relation
                .columns
                .iter()
                .find(|c| c.column.column_id == *id)
                .ok_or(WorkbenchError::Forbidden)?;
            grammar.identifier(&column.column.name, relation.engine)?;
            grammar.take(&Token::Eq)?;
            ordinal += 1;
            grammar.take(&Token::Placeholder(placeholder(relation.engine, ordinal)))?;
        }
    } else if !columns.is_empty() {
        return Err(WorkbenchError::Forbidden);
    }
    grammar.keyword("WHERE")?;
    let key = relation.stable_key().ok_or(WorkbenchError::Forbidden)?;
    for (index, id) in key.columns.iter().enumerate() {
        if index > 0 {
            grammar.keyword("AND")?;
        }
        let column = relation
            .columns
            .iter()
            .find(|c| c.column.column_id == *id)
            .ok_or(WorkbenchError::Forbidden)?;
        grammar.identifier(&column.column.name, relation.engine)?;
        if relation.engine == Engine::Postgresql {
            grammar.keyword("OPERATOR")?;
            grammar.take(&Token::LParen)?;
            grammar.keyword("pg_catalog")?;
            grammar.take(&Token::Period)?;
            grammar.take(&Token::Eq)?;
            grammar.take(&Token::RParen)?;
        } else {
            grammar.take(&Token::Eq)?;
        }
        ordinal += 1;
        grammar.take(&Token::Placeholder(placeholder(relation.engine, ordinal)))?;
    }
    if grammar.index != filtered.len() {
        return Err(WorkbenchError::Forbidden);
    }
    Ok(())
}
impl Grammar<'_> {
    fn take(&mut self, expected: &Token) -> Result<(), WorkbenchError> {
        if self.tokens.get(self.index) != Some(expected) {
            return Err(WorkbenchError::Forbidden);
        }
        self.index += 1;
        Ok(())
    }
    fn keyword(&mut self, expected: &str) -> Result<(), WorkbenchError> {
        match self.tokens.get(self.index) {
            Some(Token::Word(word))
                if word.quote_style.is_none() && word.value.eq_ignore_ascii_case(expected) =>
            {
                self.index += 1;
                Ok(())
            }
            _ => Err(WorkbenchError::Forbidden),
        }
    }
    fn identifier(&mut self, expected: &str, engine: Engine) -> Result<(), WorkbenchError> {
        let quote = if matches!(engine, Engine::Mysql | Engine::Mariadb) {
            '`'
        } else {
            '"'
        };
        match self.tokens.get(self.index) {
            Some(Token::Word(word))
                if word.quote_style == Some(quote) && word.value == expected =>
            {
                self.index += 1;
                Ok(())
            }
            _ => Err(WorkbenchError::Forbidden),
        }
    }
}
pub fn validate_column_cell(
    engine: Engine,
    column: &ColumnMetadata,
    cell: &TypedCell,
) -> Result<(), WorkbenchError> {
    if engine == Engine::Postgresql
        && (column.native_type_schema.as_deref() != Some("pg_catalog")
            || !column.native_type_id.is_some_and(|oid| {
                crate::types::postgresql_builtin_type_id(&column.declared_type, oid)
            }))
    {
        return Err(WorkbenchError::Unsupported);
    }
    validate_cells(std::slice::from_ref(cell))?;
    if column.generated {
        return Err(WorkbenchError::Unsupported);
    }
    if matches!(cell, TypedCell::Null) {
        return if column.column.nullable {
            Ok(())
        } else {
            Err(WorkbenchError::InvalidInput)
        };
    }
    let kind = column.declared_type.to_ascii_lowercase();
    let detail = column.type_detail.to_ascii_lowercase();
    let valid = match (engine, kind.as_str(), cell) {
        (Engine::Postgresql, "int2", TypedCell::I64 { value }) => value.parse::<i16>().is_ok(),
        (Engine::Postgresql, "int4", TypedCell::I64 { value }) => value.parse::<i32>().is_ok(),
        (Engine::Postgresql, "int8", TypedCell::I64 { .. }) => true,
        (Engine::Postgresql, "bool", TypedCell::Bool { .. }) => true,
        (Engine::Postgresql, "text" | "varchar", TypedCell::Text { .. }) => true,
        (Engine::Postgresql, "bytea", TypedCell::Bytes { .. }) => true,
        (Engine::Postgresql, "numeric", TypedCell::Decimal { value, .. }) => {
            decimal_fits(value, &detail)
        }
        (Engine::Sqlite, _, TypedCell::I64 { .. }) => kind.contains("int"),
        (Engine::Sqlite, _, TypedCell::Text { .. }) => {
            kind.contains("text") || kind.contains("char") || kind.contains("clob")
        }
        (Engine::Sqlite, _, TypedCell::Bytes { .. }) => kind.is_empty() || kind == "blob",
        (
            Engine::Mysql | Engine::Mariadb,
            "tinyint" | "smallint" | "mediumint" | "int" | "bigint",
            TypedCell::I64 { value },
        ) => mysql_integer(value, &kind, &detail),
        (
            Engine::Mysql | Engine::Mariadb,
            "tinyint" | "smallint" | "mediumint" | "int" | "bigint",
            TypedCell::U64 { value },
        ) => detail.contains("unsigned") && mysql_unsigned(value, &kind),
        (
            Engine::Mysql | Engine::Mariadb,
            "varchar" | "text" | "tinytext" | "mediumtext" | "longtext",
            TypedCell::Text { .. },
        ) => true,
        (
            Engine::Mysql | Engine::Mariadb,
            "blob" | "tinyblob" | "mediumblob" | "longblob" | "varbinary",
            TypedCell::Bytes { .. },
        ) => true,
        (Engine::Mysql | Engine::Mariadb, "decimal", TypedCell::Decimal { value, .. }) => {
            decimal_fits(value, &detail)
        }
        // Temporal (including MySQL TIMESTAMP without actual TZ proof), floats,
        // padded CHAR/BINARY, JSON, domains and extension types stay read-only.
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(WorkbenchError::Unsupported)
    }
}
pub fn column_supports_editing(engine: Engine, column: &ColumnMetadata, key: bool) -> bool {
    let mut probes = vec![
        TypedCell::I64 { value: "0".into() },
        TypedCell::U64 { value: "0".into() },
        TypedCell::Decimal {
            value: "0".into(),
            scale: None,
        },
        TypedCell::Bytes {
            base64: String::new(),
        },
    ];
    if !key {
        probes.push(TypedCell::Text {
            value: String::new(),
        });
        probes.push(TypedCell::Bool { value: false });
    }
    probes
        .iter()
        .any(|probe| validate_column_cell(engine, column, probe).is_ok())
}
fn decimal_fits(value: &str, detail: &str) -> bool {
    let Some(args) = detail
        .split_once('(')
        .and_then(|(_, rest)| rest.split_once(')').map(|(args, _)| args))
    else {
        return detail == "numeric";
    };
    let mut parts = args.split(',');
    let Some(precision) = parts.next().and_then(|v| v.trim().parse::<usize>().ok()) else {
        return false;
    };
    let Some(scale) = parts.next().and_then(|v| v.trim().parse::<usize>().ok()) else {
        return false;
    };
    if parts.next().is_some() || precision < scale {
        return false;
    }
    let value = value.trim_start_matches('-');
    let (integer, fraction) = value.split_once('.').unwrap_or((value, ""));
    integer.trim_start_matches('0').len() <= precision - scale && fraction.len() <= scale
}
fn integer_bits(kind: &str) -> u32 {
    match kind {
        "tinyint" => 8,
        "smallint" => 16,
        "mediumint" => 24,
        "int" => 32,
        _ => 64,
    }
}
fn mysql_integer(value: &str, kind: &str, detail: &str) -> bool {
    if detail.contains("unsigned") {
        return mysql_unsigned(value, kind);
    }
    let bits = integer_bits(kind);
    value
        .parse::<i64>()
        .is_ok_and(|v| bits == 64 || v >= -(1i64 << (bits - 1)) && v < (1i64 << (bits - 1)))
}
fn mysql_unsigned(value: &str, kind: &str) -> bool {
    let bits = integer_bits(kind);
    value
        .parse::<u64>()
        .is_ok_and(|v| bits == 64 || v < (1u64 << bits))
}
pub(crate) async fn verify_transactional_table(
    connection: &mut dyn DriverConnection,
    budget: &Arc<NativeBudget>,
    relation: &RelationMetadata,
) -> Result<(), WorkbenchError> {
    if matches!(relation.engine, Engine::Mysql | Engine::Mariadb) {
        verify_mysql_metadata_visibility(connection, budget, &relation.schema, &relation.name)
            .await?;
    }
    verify_base_origin(connection, budget, relation, true).await?;
    if matches!(relation.engine, Engine::Mysql | Engine::Mariadb) {
        let settings=crate::types::read(connection,budget,"SELECT @@session.sql_mode,@@session.character_set_client,@@session.character_set_connection",&[],1).await?;
        let row = settings.rows.first().ok_or(WorkbenchError::Unsupported)?;
        let strict = crate::types::text(&row[0])?
            .split(',')
            .any(|mode| matches!(mode, "STRICT_ALL_TABLES" | "STRICT_TRANS_TABLES"));
        if !strict
            || crate::types::text(&row[1])? != "utf8mb4"
            || crate::types::text(&row[2])? != "utf8mb4"
        {
            return Err(WorkbenchError::Unsupported);
        }
    }
    let params = [
        TypedCell::Text {
            value: relation.schema.clone(),
        },
        TypedCell::Text {
            value: relation.name.clone(),
        },
    ];
    let sql=match relation.engine{
        Engine::Postgresql=>"SELECT COUNT(*)::bigint FROM pg_catalog.pg_trigger t JOIN pg_catalog.pg_class c ON c.oid=t.tgrelid JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=$1 AND c.relname=$2 AND NOT t.tgisinternal",
        Engine::Sqlite=>"SELECT COUNT(*) FROM sqlite_schema WHERE type='trigger' AND tbl_name=?2 AND ?1='main'",
        _=>"SELECT COUNT(*) FROM information_schema.TRIGGERS WHERE BINARY EVENT_OBJECT_SCHEMA=BINARY ? AND BINARY EVENT_OBJECT_TABLE=BINARY ?",
    };
    let result = crate::types::read(connection, budget, sql, &params, 1).await?;
    if result.rows.len() != 1
        || crate::types::integer(&result.rows[0][0])? != 0
        || relation
            .columns
            .iter()
            .any(|c| c.generated || c.defaulted && !c.null_default)
    {
        return Err(WorkbenchError::Unsupported);
    }
    if matches!(relation.engine, Engine::Mysql | Engine::Mariadb)
        && !relation
            .storage_engine
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case("InnoDB"))
    {
        return Err(WorkbenchError::Unsupported);
    }
    Ok(())
}
/// Metadata rows can be hidden by privileges. Only direct database/table
/// SELECT+TRIGGER grants qualify initially; global/role expansion is unproven.
pub(crate) async fn verify_mysql_metadata_visibility(
    connection: &mut dyn DriverConnection,
    budget: &Arc<NativeBudget>,
    schema: &str,
    table: &str,
) -> Result<(), WorkbenchError> {
    let role = connection.facts().context.role.clone();
    let (user, host) = role.rsplit_once('@').ok_or(WorkbenchError::Unsupported)?;
    let grantee = format!(
        "'{}'@'{}'",
        user.replace('\'', "''"),
        host.replace('\'', "''")
    );
    let params = [
        TypedCell::Text {
            value: grantee.clone(),
        },
        TypedCell::Text {
            value: schema.into(),
        },
        TypedCell::Text { value: grantee },
        TypedCell::Text {
            value: schema.into(),
        },
        TypedCell::Text {
            value: table.into(),
        },
    ];
    let sql="SELECT PRIVILEGE_TYPE FROM information_schema.SCHEMA_PRIVILEGES WHERE BINARY GRANTEE=BINARY ? AND BINARY TABLE_SCHEMA=BINARY ? AND PRIVILEGE_TYPE IN('SELECT','TRIGGER') UNION ALL SELECT PRIVILEGE_TYPE FROM information_schema.TABLE_PRIVILEGES WHERE BINARY GRANTEE=BINARY ? AND BINARY TABLE_SCHEMA=BINARY ? AND BINARY TABLE_NAME=BINARY ? AND PRIVILEGE_TYPE IN('SELECT','TRIGGER') LIMIT 5";
    let rows = crate::types::read(connection, budget, sql, &params, 5).await?;
    let select = rows
        .rows
        .iter()
        .any(|row| crate::types::text(&row[0]).is_ok_and(|value| value == "SELECT"));
    let trigger = rows
        .rows
        .iter()
        .any(|row| crate::types::text(&row[0]).is_ok_and(|value| value == "TRIGGER"));
    if select && trigger {
        Ok(())
    } else {
        Err(WorkbenchError::Unsupported)
    }
}
/// Closed native origin checks. The initial certificate excludes indirect
/// policies/rules/inheritance and write-time constraints rather than treating
/// a table label or a zero visible trigger count as a semantic proof.
pub(crate) async fn verify_base_origin(
    connection: &mut dyn DriverConnection,
    budget: &Arc<NativeBudget>,
    relation: &RelationMetadata,
    write: bool,
) -> Result<(), WorkbenchError> {
    if relation.kind != "table"
        || matches!(relation.engine, Engine::Mysql | Engine::Mariadb)
            && relation.storage_engine.as_deref() != Some("InnoDB")
        || relation.columns.iter().any(|column| column.generated)
    {
        return Err(WorkbenchError::Unsupported);
    }
    let params = [
        TypedCell::Text {
            value: relation.schema.clone(),
        },
        TypedCell::Text {
            value: relation.name.clone(),
        },
    ];
    match relation.engine {
        Engine::Postgresql => {
            let sql="SELECT c.relkind::text,c.relrowsecurity,EXISTS(SELECT 1 FROM pg_catalog.pg_inherits i WHERE i.inhparent=c.oid OR i.inhrelid=c.oid),EXISTS(SELECT 1 FROM pg_catalog.pg_rewrite r WHERE r.ev_class=c.oid),EXISTS(SELECT 1 FROM pg_catalog.pg_constraint x WHERE (x.conrelid=c.oid OR x.confrelid=c.oid) AND x.contype IN('c','f')),EXISTS(SELECT 1 FROM pg_catalog.pg_am a JOIN pg_catalog.pg_proc p ON p.oid=a.amhandler JOIN pg_catalog.pg_namespace an ON an.oid=p.pronamespace WHERE a.oid=c.relam AND a.amname='heap' AND an.nspname='pg_catalog' AND p.proname='heap_tableam_handler') FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=$1 AND c.relname=$2";
            let result = crate::types::read(connection, budget, sql, &params, 2).await?;
            let row = result
                .rows
                .first()
                .filter(|_| result.rows.len() == 1)
                .ok_or(WorkbenchError::Unsupported)?;
            if crate::types::text(&row[0])? != "r"
                || crate::types::boolean(&row[1])?
                || crate::types::boolean(&row[2])?
                || crate::types::boolean(&row[3])?
                || write && crate::types::boolean(&row[4])?
                || !crate::types::boolean(&row[5])?
            {
                return Err(WorkbenchError::Unsupported);
            }
        }
        Engine::Sqlite => {
            let result = crate::types::read(
                connection,
                budget,
                "SELECT p.type,s.sql FROM pragma_table_list p JOIN main.sqlite_schema s ON s.name=p.name WHERE p.schema=?1 AND p.name=?2",
                &params,
                2,
            )
            .await?;
            if result.rows.len() != 1 || crate::types::text(&result.rows[0][0])? != "table" {
                return Err(WorkbenchError::Unsupported);
            }
            if write {
                let ddl = crate::types::text(&result.rows[0][1])?;
                let tokens = Tokenizer::new(&SQLiteDialect {}, ddl)
                    .tokenize()
                    .map_err(|_| WorkbenchError::Unsupported)?;
                if tokens.iter().any(|token| matches!(token,Token::Word(word) if word.quote_style.is_none() && word.value.eq_ignore_ascii_case("CHECK"))) {
                    return Err(WorkbenchError::Unsupported);
                }
                let foreign=crate::types::read(connection,budget,"SELECT COUNT(*) FROM main.sqlite_schema s JOIN pragma_foreign_key_list(s.name) f WHERE s.type='table' AND (s.name=?2 OR f.\"table\"=?2) AND ?1='main'",&params,1).await?;
                if crate::types::integer(&foreign.rows[0][0])? != 0 {
                    return Err(WorkbenchError::Unsupported);
                }
            }
        }
        _ => {
            if relation.schema != connection.facts().context.database {
                return Err(WorkbenchError::Unsupported);
            }
            if write {
                let sql="SELECT (SELECT COUNT(*) FROM information_schema.KEY_COLUMN_USAGE k WHERE (BINARY k.TABLE_SCHEMA=BINARY t.TABLE_SCHEMA AND BINARY k.TABLE_NAME=BINARY t.TABLE_NAME AND k.REFERENCED_TABLE_NAME IS NOT NULL) OR (BINARY k.REFERENCED_TABLE_SCHEMA=BINARY t.TABLE_SCHEMA AND BINARY k.REFERENCED_TABLE_NAME=BINARY t.TABLE_NAME))+(SELECT COUNT(*) FROM information_schema.TABLE_CONSTRAINTS c WHERE BINARY c.TABLE_SCHEMA=BINARY t.TABLE_SCHEMA AND BINARY c.TABLE_NAME=BINARY t.TABLE_NAME AND c.CONSTRAINT_TYPE='CHECK') FROM information_schema.TABLES t WHERE BINARY t.TABLE_SCHEMA=BINARY ? AND BINARY t.TABLE_NAME=BINARY ?";
                let result = crate::types::read(connection, budget, sql, &params, 2).await?;
                if result.rows.len() != 1 || crate::types::integer(&result.rows[0][0])? != 0 {
                    return Err(WorkbenchError::Unsupported);
                }
            }
        }
    }
    Ok(())
}
struct Discard;
#[async_trait::async_trait]
impl RowSink for Discard {
    async fn columns(&mut self, _: Vec<Column>) -> Result<(), WorkbenchError> {
        Ok(())
    }
    async fn row(&mut self, _: Vec<TypedCell>) -> Result<(), WorkbenchError> {
        Ok(())
    }
}
