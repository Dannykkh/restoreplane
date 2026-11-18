use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use base64::Engine as _;
use restoreplane_domain::workbench::{
    ActualContext, Column, ConnectDescriptor, Engine, Mode, Route, TargetPurpose, TypedCell,
};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::{Connection, OpenFlags};

use super::{
    column, encoded_row_bytes, DriverCancel, DriverConnection, DriverFacts, DriverSummary,
    QueryLimits, RowSink,
};
use crate::WorkbenchError;

struct SqliteConnection {
    connection: Arc<Mutex<Connection>>,
    _identity_guard: std::fs::File,
    facts: DriverFacts,
    cancel: Arc<SqliteCancel>,
    opened_read_only: bool,
}
struct SqliteCancel {
    interrupt: rusqlite::InterruptHandle,
}
#[async_trait]
impl DriverCancel for SqliteCancel {
    async fn request(&self) -> Result<(), WorkbenchError> {
        self.interrupt.interrupt();
        Ok(())
    }
}

pub(super) async fn connect(
    descriptor: &ConnectDescriptor,
    mode: Mode,
) -> Result<Box<dyn DriverConnection>, WorkbenchError> {
    let Route::File { canonical_path } = &descriptor.route else {
        return Err(WorkbenchError::InvalidInput);
    };
    let path = canonical_path.clone();
    let expected_object_id = descriptor.database.engine_object_id.clone();
    let read_only = mode == Mode::ReadOnly;
    let (connection, canonical, object_id, identity_guard, local_ntfs) =
        tokio::task::spawn_blocking(move || {
            if !std::path::Path::new(&path).is_absolute() {
                return Err(WorkbenchError::InvalidInput);
            }
            // Ordinary absolute Windows paths and verbatim canonical paths resolve
            // to the same bound file. The existing probe refuses reparse traversal.
            let mut options = std::fs::OpenOptions::new();
            options.read(true);
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                // Hold read/write sharing, excluding delete/replace until disconnect.
                options.share_mode(0x0000_0001 | 0x0000_0002);
            }
            let identity_guard = options.open(&path).map_err(|_| WorkbenchError::Io)?;
            let local_ntfs =
                restoreplane_source_sqlite::opened_handle_is_local_ntfs(&identity_guard)
                    .unwrap_or(false);
            let guard_id = restoreplane_source_sqlite::opened_file_object_id(&identity_guard)
                .map_err(|_| WorkbenchError::BindingUnproven)?;
            if format!("file:{guard_id}") != expected_object_id {
                return Err(WorkbenchError::BindingStale);
            }
            // Pin before probing: a replace between an initial probe and this open
            // must not bind a different file to the reviewed database target.
            let held = restoreplane_source_sqlite::probe_sqlite_file(std::path::Path::new(&path))
                .map_err(|_| WorkbenchError::BindingUnproven)?;
            if held.object_id() != guard_id {
                return Err(WorkbenchError::BindingStale);
            }
            let canonical = held.canonical_path().to_owned();
            let flags = if read_only {
                OpenFlags::SQLITE_OPEN_READ_ONLY
            } else {
                OpenFlags::SQLITE_OPEN_READ_WRITE
            };
            let mut connection =
                Connection::open_with_flags(&canonical, flags | OpenFlags::SQLITE_OPEN_NO_MUTEX)
                    .map_err(|_| WorkbenchError::Io)?;
            let observed =
                restoreplane_source_sqlite::probe_sqlite_file(std::path::Path::new(&path))
                    .map_err(|_| WorkbenchError::BindingUnproven)?;
            if !held.same_file(&observed) {
                return Err(WorkbenchError::BindingStale);
            }
            connection
                .set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH, 256 * 1024)
                .map_err(|_| WorkbenchError::Unsupported)?;
            connection
                .set_limit(
                    rusqlite::limits::Limit::SQLITE_LIMIT_SQL_LENGTH,
                    1024 * 1024,
                )
                .map_err(|_| WorkbenchError::Unsupported)?;
            connection
                .set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_COLUMN, 512)
                .map_err(|_| WorkbenchError::Unsupported)?;
            connection
                .busy_timeout(std::time::Duration::from_secs(2))
                .map_err(|_| WorkbenchError::Io)?;
            connection
                .pragma_update(None, "trusted_schema", false)
                .map_err(|_| WorkbenchError::Io)?;
            connection
                .pragma_update(None, "foreign_keys", true)
                .map_err(|_| WorkbenchError::Unsupported)?;
            if !connection
                .pragma_query_value(None, "foreign_keys", |row| row.get::<_, bool>(0))
                .map_err(|_| WorkbenchError::Io)?
            {
                return Err(WorkbenchError::Unsupported);
            }
            connection
                .authorizer(Some(move |context: AuthContext<'_>| {
                    sqlite_authorize(context, read_only)
                }))
                .map_err(|_| WorkbenchError::Unsupported)?;
            Ok::<_, WorkbenchError>((
                connection,
                canonical,
                held.object_id().to_owned(),
                identity_guard,
                local_ntfs,
            ))
        })
        .await
        .map_err(|_| WorkbenchError::Io)??;
    let cancel = Arc::new(SqliteCancel {
        interrupt: connection.get_interrupt_handle(),
    });
    let facts = DriverFacts {
        context: ActualContext {
            endpoint: canonical.to_string_lossy().into_owned(),
            database: descriptor.database.name.clone(),
            role: "current_os_user".into(),
            purpose: TargetPurpose::Source,
        },
        engine: Engine::Sqlite,
        server_version: rusqlite::version().into(),
        server_identity: Some(format!("sqlite-file-id:{object_id}")),
        actual_database_identity: local_ntfs.then(|| format!("sqlite-file-object-id:{object_id}")),
        read_only_held: read_only,
        verified_tls: false,
    };
    if descriptor
        .expected_server_identity
        .as_ref()
        .is_some_and(|expected| Some(expected) != facts.server_identity.as_ref())
    {
        return Err(WorkbenchError::BindingStale);
    }
    Ok(Box::new(SqliteConnection {
        connection: Arc::new(Mutex::new(connection)),
        _identity_guard: identity_guard,
        facts,
        cancel,
        opened_read_only: read_only,
    }))
}

fn sqlite_authorize(context: AuthContext<'_>, read_only: bool) -> Authorization {
    match context.action {
        AuthAction::Attach { .. } | AuthAction::Detach { .. } => Authorization::Deny,
        AuthAction::Function { function_name }
            if ["load_extension", "readfile", "writefile"]
                .contains(&function_name.to_ascii_lowercase().as_str()) =>
        {
            Authorization::Deny
        }
        AuthAction::Pragma {
            pragma_name,
            pragma_value,
        } => {
            let name = pragma_name.to_ascii_lowercase();
            if [
                "table_info",
                "table_xinfo",
                "index_list",
                "index_info",
                "index_xinfo",
                "foreign_key_list",
                "table_list",
            ]
            .contains(&name.as_str())
                || (pragma_value.is_none()
                    && ["database_list", "query_only"].contains(&name.as_str()))
            {
                Authorization::Allow
            } else {
                Authorization::Deny
            }
        }
        AuthAction::Read { .. }
        | AuthAction::Select
        | AuthAction::Transaction { .. }
        | AuthAction::Function { .. }
        | AuthAction::Recursive => Authorization::Allow,
        AuthAction::Unknown { .. } => Authorization::Deny,
        _ if read_only => Authorization::Deny,
        _ => Authorization::Allow,
    }
}

enum Event {
    Columns(Vec<Column>),
    Row(Vec<TypedCell>),
    End(Result<DriverSummary, WorkbenchError>),
}

#[async_trait]
impl DriverConnection for SqliteConnection {
    fn facts(&self) -> &DriverFacts {
        &self.facts
    }
    fn cancel_handle(&self) -> Arc<dyn DriverCancel> {
        self.cancel.clone()
    }
    async fn transaction_read_only(&mut self) -> Result<bool, WorkbenchError> {
        let connection = self.connection.clone();
        let held = self.facts.read_only_held;
        tokio::task::spawn_blocking(move || {
            Ok(held
                && !connection
                    .lock()
                    .map_err(|_| WorkbenchError::Io)?
                    .is_autocommit())
        })
        .await
        .map_err(|_| WorkbenchError::Io)?
    }
    async fn configure_limits(
        &mut self,
        lock_wait: std::time::Duration,
    ) -> Result<(), WorkbenchError> {
        if lock_wait.is_zero() || lock_wait > std::time::Duration::from_secs(30) {
            return Err(WorkbenchError::Limit);
        }
        let connection = self.connection.clone();
        tokio::task::spawn_blocking(move || {
            connection
                .lock()
                .map_err(|_| WorkbenchError::Io)?
                .busy_timeout(lock_wait)
                .map_err(|_| WorkbenchError::Io)
        })
        .await
        .map_err(|_| WorkbenchError::Io)?
    }
    async fn set_read_only(&mut self, read_only: bool) -> Result<(), WorkbenchError> {
        if self.opened_read_only && !read_only {
            return Err(WorkbenchError::Unsupported);
        }
        let connection = self.connection.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = connection.lock().map_err(|_| WorkbenchError::Io)?;
            connection
                .authorizer(Some(move |context: AuthContext<'_>| {
                    sqlite_authorize(context, read_only)
                }))
                .map_err(|_| WorkbenchError::Unsupported)?;
            Ok::<_, WorkbenchError>(())
        })
        .await
        .map_err(|_| WorkbenchError::Io)??;
        self.facts.read_only_held = read_only;
        Ok(())
    }
    async fn execute(
        &mut self,
        sql: &str,
        limits: QueryLimits,
        sink: &mut dyn RowSink,
    ) -> Result<DriverSummary, WorkbenchError> {
        self.execute_bound(sql, &[], limits, sink).await
    }
    async fn execute_bound(
        &mut self,
        sql: &str,
        parameters: &[TypedCell],
        limits: QueryLimits,
        sink: &mut dyn RowSink,
    ) -> Result<DriverSummary, WorkbenchError> {
        limits.validate()?;
        self.execute_rows(sql, parameters, limits, sink).await
    }
    async fn execute_file_read(
        &mut self,
        sql: &str,
        limits: super::FileReadLimits,
        sink: &mut dyn RowSink,
    ) -> Result<DriverSummary, WorkbenchError> {
        self.execute_rows(sql, &[], limits.query_limits(), sink)
            .await
    }
    async fn close(&mut self) -> Result<(), WorkbenchError> {
        self.cancel.interrupt.interrupt();
        Ok(())
    }
}

fn sqlite_statement_error(error: rusqlite::Error, connection: &Connection) -> WorkbenchError {
    use rusqlite::ErrorCode;
    // Both the native progress deadline and an explicit interrupt produce
    // SQLITE_INTERRUPT (9). It is not SQLITE_IOERR and proves no rollback.
    if matches!(&error,rusqlite::Error::SqliteFailure(inner,_) if inner.code==ErrorCode::OperationInterrupted)
    {
        return WorkbenchError::CancelUnsettled;
    }
    let known = matches!(&error,rusqlite::Error::SqliteFailure(inner,_) if matches!(inner.code,
        ErrorCode::ConstraintViolation|ErrorCode::TypeMismatch|ErrorCode::ReadOnly|ErrorCode::AuthorizationForStatementDenied));
    if known {
        WorkbenchError::StatementRejected(if connection.is_autocommit() {
            super::DriverTransactionState::Idle
        } else {
            super::DriverTransactionState::Active
        })
    } else {
        WorkbenchError::Io
    }
}

fn sqlite_parameter(cell: &TypedCell) -> Result<rusqlite::types::Value, WorkbenchError> {
    use rusqlite::types::Value;
    Ok(match cell {
        TypedCell::Null => Value::Null,
        TypedCell::Bool { value } => Value::Integer(i64::from(*value)),
        TypedCell::I64 { value } => {
            Value::Integer(value.parse().map_err(|_| WorkbenchError::InvalidInput)?)
        }
        TypedCell::U64 { value } => {
            Value::Integer(value.parse().map_err(|_| WorkbenchError::Unsupported)?)
        }
        TypedCell::Text { value } if value.len() <= 64 * 1024 => Value::Text(value.clone()),
        TypedCell::Bytes { base64 } if base64.len() <= 88 * 1024 => Value::Blob(
            base64::engine::general_purpose::STANDARD
                .decode(base64)
                .map_err(|_| WorkbenchError::InvalidInput)?,
        ),
        TypedCell::Float { value } => {
            let parsed: f64 = value.parse().map_err(|_| WorkbenchError::InvalidInput)?;
            if !parsed.is_finite() {
                return Err(WorkbenchError::Unsupported);
            }
            Value::Real(parsed)
        }
        _ => return Err(WorkbenchError::Unsupported),
    })
}

fn sqlite_cell(value: rusqlite::Result<rusqlite::types::ValueRef<'_>>) -> TypedCell {
    use rusqlite::types::ValueRef;
    match value {
        Ok(ValueRef::Null) => TypedCell::Null,
        Ok(ValueRef::Integer(value)) => TypedCell::I64 {
            value: value.to_string(),
        },
        Ok(ValueRef::Real(value)) => TypedCell::Float {
            value: super::float_text(value),
        },
        Ok(ValueRef::Text(bytes)) if bytes.len() <= 64 * 1024 => match std::str::from_utf8(bytes) {
            Ok(value) => TypedCell::Text {
                value: value.into(),
            },
            Err(_) => TypedCell::Unsupported {
                type_name: "text".into(),
                reason: "invalid_utf8".into(),
            },
        },
        Ok(ValueRef::Blob(bytes)) if bytes.len() <= 64 * 1024 => TypedCell::Bytes {
            base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        },
        _ => TypedCell::Unsupported {
            type_name: "sqlite".into(),
            reason: "inline_cell_limit_or_codec".into(),
        },
    }
}

impl SqliteConnection {
    async fn execute_rows(
        &mut self,
        sql: &str,
        parameters: &[TypedCell],
        limits: QueryLimits,
        sink: &mut dyn RowSink,
    ) -> Result<DriverSummary, WorkbenchError> {
        super::validate_statement(sql, parameters)?;
        let parameters = parameters
            .iter()
            .map(sqlite_parameter)
            .collect::<Result<Vec<_>, _>>()?;
        let sql = sql.to_owned();
        let connection = self.connection.clone();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(2);
        let work = tokio::task::spawn_blocking(move || {
            let result = (|| {
                let mut connection = connection.lock().map_err(|_| WorkbenchError::Io)?;
                let started = Instant::now();
                let timeout = limits.timeout;
                connection
                    .progress_handler(1000, Some(move || started.elapsed() > timeout))
                    .map_err(|_| WorkbenchError::Unsupported)?;
                let result = (|| {
                    let mut statement = connection
                        .prepare(&sql)
                        .map_err(|error| sqlite_statement_error(error, &connection))?;
                    if statement.column_count() > 512 {
                        return Err(WorkbenchError::Limit);
                    }
                    sender
                        .blocking_send(Event::Columns(
                            statement
                                .column_names()
                                .map(|name| column(name.into(), "sqlite_storage_class".into()))
                                .collect(),
                        ))
                        .map_err(|_| WorkbenchError::SessionStale)?;
                    let columns = statement.column_count();
                    let mut rows = statement
                        .query(rusqlite::params_from_iter(parameters))
                        .map_err(|error| sqlite_statement_error(error, &connection))?;
                    let mut summary = DriverSummary::default();
                    while let Some(row) = rows
                        .next()
                        .map_err(|error| sqlite_statement_error(error, &connection))?
                    {
                        summary.rows_seen += 1;
                        if summary.capped || summary.rows_kept as usize >= limits.max_rows {
                            summary.capped = true;
                            continue;
                        }
                        let cells = (0..columns)
                            .map(|index| sqlite_cell(row.get_ref(index)))
                            .collect::<Vec<_>>();
                        let bytes = encoded_row_bytes(&cells)?;
                        if summary.encoded_bytes.saturating_add(bytes) > limits.max_bytes {
                            summary.capped = true;
                            continue;
                        }
                        sender
                            .blocking_send(Event::Row(cells))
                            .map_err(|_| WorkbenchError::SessionStale)?;
                        summary.rows_kept += 1;
                        summary.encoded_bytes += bytes;
                    }
                    summary.rows_affected = Some(connection.changes());
                    summary.transaction_state = if connection.is_autocommit() {
                        super::DriverTransactionState::Idle
                    } else {
                        super::DriverTransactionState::Active
                    };
                    Ok(summary)
                })();
                connection
                    .progress_handler(0, None::<fn() -> bool>)
                    .map_err(|_| WorkbenchError::Io)?;
                result
            })();
            let _ = sender.blocking_send(Event::End(result));
        });
        let mut answer = Err(WorkbenchError::Io);
        while let Some(event) = receiver.recv().await {
            let forwarded = match event {
                Event::Columns(columns) => sink.columns(columns).await,
                Event::Row(row) => sink.row(row).await,
                Event::End(result) => {
                    answer = result;
                    break;
                }
            };
            if let Err(error) = forwarded {
                self.cancel.interrupt.interrupt();
                answer = Err(error);
                break;
            }
        }
        drop(receiver);
        work.await.map_err(|_| WorkbenchError::Io)?;
        answer
    }
}
