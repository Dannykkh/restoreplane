use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use futures::FutureExt;
use mysql_async::{prelude::Queryable, Conn, Opts, OptsBuilder, SslOpts, Value};
use restoreplane_domain::workbench::{
    ActualContext, ConnectDescriptor, Engine, Mode, Route, TargetPurpose, TypedCell,
};

use super::{
    check_identity, column, encoded_row_bytes, root_certificate, text_cell, DriverCancel,
    DriverConnection, DriverFacts, DriverLogin, DriverSummary, QueryLimits, RowSink,
};
use crate::WorkbenchError;

struct MySqlConnection {
    connection: Option<OwnedConnection>,
    facts: DriverFacts,
    cancel: Arc<MySqlCancel>,
}

// mysql_async's default Drop starts background result draining. Workbench must
// bound retirement instead of retaining arbitrarily long server-side results.
struct OwnedConnection(Option<Conn>);
impl OwnedConnection {
    fn into_inner(mut self) -> Conn {
        self.0.take().expect("owned MySQL connection")
    }
}
impl std::ops::Deref for OwnedConnection {
    type Target = Conn;
    fn deref(&self) -> &Self::Target {
        self.0.as_ref().expect("owned MySQL connection")
    }
}
impl std::ops::DerefMut for OwnedConnection {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut().expect("owned MySQL connection")
    }
}
impl Drop for OwnedConnection {
    fn drop(&mut self) {
        if let Some(connection) = self.0.take() {
            // First poll marks the connection disconnected before its first I/O
            // await. Dropping a pending future then closes the socket without
            // mysql_async's background cleanup, and proves no remote outcome.
            if tokio::runtime::Handle::try_current().is_ok() {
                let _ = connection.disconnect().now_or_never();
            }
        }
    }
}
struct MySqlCancel {
    options: Opts,
    connection_id: u32,
    engine: Engine,
    server_identity: String,
}

#[async_trait]
impl DriverCancel for MySqlCancel {
    async fn request(&self) -> Result<(), WorkbenchError> {
        let mut control = OwnedConnection(Some(
            Conn::new(self.options.clone())
                .await
                .map_err(|_| WorkbenchError::CancelUnsettled)?,
        ));
        let raw: Option<String> = control
            .query_first(if self.engine == Engine::Mariadb {
                "SELECT @@server_uid"
            } else {
                "SELECT @@server_uuid"
            })
            .await
            .map_err(|_| WorkbenchError::CancelUnsettled)?;
        let observed = raw.map(|raw| {
            if self.engine == Engine::Mariadb {
                format!("mariadb-server-uid:{raw}")
            } else {
                format!("mysql-server-uuid:{}", raw.to_ascii_lowercase())
            }
        });
        if observed.as_ref() != Some(&self.server_identity) {
            return Err(WorkbenchError::CancelUnsettled);
        }
        let result = control
            .query_drop(format!("KILL QUERY {}", self.connection_id))
            .await
            .map_err(|_| WorkbenchError::CancelUnsettled);
        let _ = control.into_inner().disconnect().await;
        result
    }
}

pub(super) async fn connect(
    descriptor: &ConnectDescriptor,
    login: DriverLogin,
    mode: Mode,
    plaintext: bool,
) -> Result<Box<dyn DriverConnection>, WorkbenchError> {
    let Route::Tcp { host, port } = &descriptor.route else {
        return Err(WorkbenchError::InvalidInput);
    };
    let root = tokio::task::spawn_blocking({
        let trust = descriptor.tls.root_trust.clone();
        move || root_certificate(trust.as_ref())
    })
    .await
    .map_err(|_| WorkbenchError::Io)??;
    let ssl = if plaintext {
        None
    } else {
        // The authority-bound name changes the verification identity, never the
        // certificate or hostname checks themselves (both remain enabled).
        let mut ssl = SslOpts::default()
            .with_danger_tls_hostname_override(descriptor.tls.server_name.clone());
        if let Some(bytes) = root {
            ssl = ssl
                .with_disable_built_in_roots(true)
                .with_root_certs(vec![bytes.into()]);
        }
        Some(ssl)
    };
    let options: Opts = OptsBuilder::default()
        .ip_or_hostname(host)
        .tcp_port(*port)
        .prefer_socket(false)
        .user(Some(login.username))
        .pass(Some(login.secret.to_string()))
        .db_name(Some(descriptor.database.name.clone()))
        .ssl_opts(ssl)
        .stmt_cache_size(0)
        .max_allowed_packet(Some(1024 * 1024))
        .into();
    let mut connection = OwnedConnection(Some(
        Conn::new(options.clone())
            .await
            .map_err(|_| WorkbenchError::Io)?,
    ));
    let identity_sql = if descriptor.engine == Engine::Mariadb {
        "SELECT VERSION(), DATABASE(), CURRENT_USER(), @@server_uid"
    } else {
        "SELECT VERSION(), DATABASE(), CURRENT_USER(), @@server_uuid"
    };
    let (version, database, role, raw_identity): (String, String, String, String) = connection
        .query_first(identity_sql)
        .await
        .map_err(|_| WorkbenchError::BindingUnproven)?
        .ok_or(WorkbenchError::BindingUnproven)?;
    if (descriptor.engine == Engine::Mariadb) != version.to_ascii_lowercase().contains("mariadb") {
        return Err(WorkbenchError::BindingStale);
    }
    let identity = if descriptor.engine == Engine::Mariadb {
        format!("mariadb-server-uid:{raw_identity}")
    } else {
        format!("mysql-server-uuid:{}", raw_identity.to_ascii_lowercase())
    };
    let facts = DriverFacts {
        context: ActualContext {
            endpoint: format!("{host}:{port}"),
            database,
            role,
            purpose: TargetPurpose::Source,
        },
        engine: descriptor.engine,
        server_version: version,
        server_identity: Some(identity),
        actual_database_identity: None,
        read_only_held: false,
        verified_tls: !plaintext,
    };
    check_identity(descriptor, &facts)?;
    let cancel = Arc::new(MySqlCancel {
        options,
        connection_id: connection.id(),
        engine: descriptor.engine,
        server_identity: facts
            .server_identity
            .clone()
            .ok_or(WorkbenchError::BindingUnproven)?,
    });
    let mut connection = MySqlConnection {
        connection: Some(connection),
        facts,
        cancel,
    };
    connection.set_read_only(mode == Mode::ReadOnly).await?;
    Ok(Box::new(connection))
}

#[async_trait]
impl DriverConnection for MySqlConnection {
    fn facts(&self) -> &DriverFacts {
        &self.facts
    }
    fn cancel_handle(&self) -> Arc<dyn DriverCancel> {
        self.cancel.clone()
    }
    async fn transaction_read_only(&mut self) -> Result<bool, WorkbenchError> {
        use mysql_async::consts::StatusFlags;
        let connection = self
            .connection
            .as_mut()
            .ok_or(WorkbenchError::SessionStale)?;
        connection
            .query_drop("DO 0")
            .await
            .map_err(|_| WorkbenchError::Io)?;
        Ok(connection.last_ok_packet().is_some_and(|packet| {
            packet.status_flags().contains(
                StatusFlags::SERVER_STATUS_IN_TRANS | StatusFlags::SERVER_STATUS_IN_TRANS_READONLY,
            )
        }))
    }
    async fn configure_limits(
        &mut self,
        lock_wait: std::time::Duration,
    ) -> Result<(), WorkbenchError> {
        let millis = lock_wait.as_millis();
        if millis == 0 || millis > 30_000 || !millis.is_multiple_of(1000) {
            return Err(WorkbenchError::Unsupported);
        }
        let connection = self
            .connection
            .as_mut()
            .ok_or(WorkbenchError::SessionStale)?;
        connection
            .query_drop(format!(
                "SET SESSION innodb_lock_wait_timeout={},lock_wait_timeout={}",
                millis / 1000,
                millis / 1000
            ))
            .await
            .map_err(|_| WorkbenchError::Io)
    }
    async fn set_read_only(&mut self, read_only: bool) -> Result<(), WorkbenchError> {
        let connection = self
            .connection
            .as_mut()
            .ok_or(WorkbenchError::SessionStale)?;
        connection
            .query_drop(if read_only {
                "SET SESSION TRANSACTION READ ONLY"
            } else {
                "SET SESSION TRANSACTION READ WRITE"
            })
            .await
            .map_err(|_| WorkbenchError::Unsupported)?;
        let observed: Option<u8> = connection
            .query_first(if self.facts.engine == Engine::Mariadb {
                "SELECT @@session.tx_read_only"
            } else {
                "SELECT @@session.transaction_read_only"
            })
            .await
            .map_err(|_| WorkbenchError::Io)?;
        if observed != Some(u8::from(read_only)) {
            return Err(WorkbenchError::Unsupported);
        }
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
    async fn transaction_control(
        &mut self,
        command: super::TransactionCommand,
    ) -> Result<(), WorkbenchError> {
        let engine = self.facts.engine;
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            let connection = self
                .connection
                .as_mut()
                .ok_or(WorkbenchError::SessionStale)?;
            // MySQL rejects some transaction commands in its prepared protocol
            // (ER_UNSUPPORTED_PS). Only closed native literals use text protocol.
            for sql in command.statements(engine)? {
                connection
                    .query_drop(sql)
                    .await
                    .map_err(|_| WorkbenchError::Io)?;
            }
            Ok(())
        })
        .await;
        match outcome {
            Ok(result) => result,
            Err(_) => {
                self.connection.take();
                Err(WorkbenchError::CancelUnsettled)
            }
        }
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
        if let Some(connection) = self.connection.take() {
            tokio::time::timeout(
                std::time::Duration::from_millis(250),
                connection.into_inner().disconnect(),
            )
            .await
            .map_err(|_| WorkbenchError::CancelUnsettled)?
            .map_err(|_| WorkbenchError::Io)?;
        }
        Ok(())
    }
}

fn mysql_statement_error(error: mysql_async::Error) -> WorkbenchError {
    match error {
        mysql_async::Error::Server(_) => {
            WorkbenchError::StatementRejected(super::DriverTransactionState::Unknown)
        }
        _ => WorkbenchError::Io,
    }
}

fn mysql_parameter(cell: &TypedCell) -> Result<Value, WorkbenchError> {
    Ok(match cell {
        TypedCell::Null => Value::NULL,
        TypedCell::Bool { value } => Value::UInt(u64::from(*value)),
        TypedCell::I64 { value } => {
            Value::Int(value.parse().map_err(|_| WorkbenchError::InvalidInput)?)
        }
        TypedCell::U64 { value } => {
            Value::UInt(value.parse().map_err(|_| WorkbenchError::InvalidInput)?)
        }
        TypedCell::Text { value } | TypedCell::Decimal { value, .. }
            if value.len() <= 64 * 1024 =>
        {
            Value::Bytes(value.as_bytes().to_vec())
        }
        TypedCell::Bytes { base64 } if base64.len() <= 88 * 1024 => Value::Bytes(
            base64::engine::general_purpose::STANDARD
                .decode(base64)
                .map_err(|_| WorkbenchError::InvalidInput)?,
        ),
        TypedCell::Float { value } => {
            let parsed: f64 = value.parse().map_err(|_| WorkbenchError::InvalidInput)?;
            if !parsed.is_finite() {
                return Err(WorkbenchError::Unsupported);
            }
            Value::Double(parsed)
        }
        _ => return Err(WorkbenchError::Unsupported),
    })
}

fn mysql_cell(value: &Value, column: Option<&mysql_async::Column>) -> TypedCell {
    use mysql_async::consts::{ColumnFlags, ColumnType};
    match value {
        Value::NULL => TypedCell::Null,
        Value::Int(value) => TypedCell::I64 {
            value: value.to_string(),
        },
        Value::UInt(value) => TypedCell::U64 {
            value: value.to_string(),
        },
        Value::Float(value) => TypedCell::Float {
            value: super::float_text(f64::from(*value)),
        },
        Value::Double(value) => TypedCell::Float {
            value: super::float_text(*value),
        },
        Value::Date(year, month, day, hour, minute, second, micros) => {
            let kind = match column.map(|column| column.column_type()) {
                Some(ColumnType::MYSQL_TYPE_DATE) => "date",
                Some(ColumnType::MYSQL_TYPE_TIMESTAMP) => "timestamp",
                _ => "datetime",
            };
            if *year > 9999
                || *month > 12
                || *day > 31
                || *hour > 23
                || *minute > 59
                || *second > 59
                || *micros >= 1_000_000
            {
                return TypedCell::Unsupported {
                    type_name: kind.into(),
                    reason: "invalid_temporal_fields".into(),
                };
            }
            let precision = temporal_precision(column, *micros);
            let value = if kind == "date" {
                format!("{year:04}-{month:02}-{day:02}")
            } else {
                format!(
                    "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}{}",
                    fraction(*micros, precision)
                )
            };
            TypedCell::Temporal {
                value,
                type_name: kind.into(),
                offset: None,
                precision: if kind == "date" { 0 } else { precision },
            }
        }
        Value::Time(negative, days, hour, minute, second, micros) => {
            let hours = u64::from(*days) * 24 + u64::from(*hour);
            if hours > 838 || *minute > 59 || *second > 59 || *micros >= 1_000_000 {
                return TypedCell::Unsupported {
                    type_name: "time".into(),
                    reason: "invalid_temporal_fields".into(),
                };
            }
            let precision = temporal_precision(column, *micros);
            TypedCell::Temporal {
                value: format!(
                    "{}{hours:02}:{minute:02}:{second:02}{}",
                    if *negative { "-" } else { "" },
                    fraction(*micros, precision)
                ),
                type_name: "time".into(),
                offset: None,
                precision,
            }
        }
        Value::Bytes(bytes) => {
            if bytes.len() > 64 * 1024 {
                return TypedCell::Unsupported {
                    type_name: "mysql".into(),
                    reason: "inline_cell_limit".into(),
                };
            }
            if column.is_some_and(|c| {
                c.character_set() == 63
                    && matches!(
                        c.column_type(),
                        ColumnType::MYSQL_TYPE_BLOB
                            | ColumnType::MYSQL_TYPE_TINY_BLOB
                            | ColumnType::MYSQL_TYPE_MEDIUM_BLOB
                            | ColumnType::MYSQL_TYPE_LONG_BLOB
                            | ColumnType::MYSQL_TYPE_STRING
                            | ColumnType::MYSQL_TYPE_VAR_STRING
                    )
            }) {
                return TypedCell::Bytes {
                    base64: base64::engine::general_purpose::STANDARD.encode(bytes),
                };
            }
            let Ok(text) = std::str::from_utf8(bytes) else {
                return TypedCell::Unsupported {
                    type_name: "mysql".into(),
                    reason: "invalid_utf8".into(),
                };
            };
            match column.map(|c| c.column_type()) {
                Some(
                    ColumnType::MYSQL_TYPE_TINY
                    | ColumnType::MYSQL_TYPE_SHORT
                    | ColumnType::MYSQL_TYPE_LONG
                    | ColumnType::MYSQL_TYPE_LONGLONG
                    | ColumnType::MYSQL_TYPE_INT24,
                ) => {
                    if column.is_some_and(|c| c.flags().contains(ColumnFlags::UNSIGNED_FLAG)) {
                        TypedCell::U64 { value: text.into() }
                    } else {
                        TypedCell::I64 { value: text.into() }
                    }
                }
                Some(ColumnType::MYSQL_TYPE_DECIMAL | ColumnType::MYSQL_TYPE_NEWDECIMAL) => {
                    text_cell(text, "decimal")
                }
                Some(ColumnType::MYSQL_TYPE_FLOAT | ColumnType::MYSQL_TYPE_DOUBLE) => {
                    text_cell(text, "double")
                }
                Some(
                    ColumnType::MYSQL_TYPE_TIMESTAMP
                    | ColumnType::MYSQL_TYPE_DATETIME
                    | ColumnType::MYSQL_TYPE_DATE
                    | ColumnType::MYSQL_TYPE_TIME,
                ) => text_cell(text, "datetime"),
                _ => TypedCell::Text { value: text.into() },
            }
        }
    }
}
fn fraction(micros: u32, precision: u32) -> String {
    if precision == 0 {
        String::new()
    } else {
        format!(
            ".{:0width$}",
            micros / 10u32.pow(6 - precision),
            width = precision as usize
        )
    }
}
fn temporal_precision(column: Option<&mysql_async::Column>, micros: u32) -> u32 {
    let declared = column.map_or(6, |column| u32::from(column.decimals()).min(6));
    if micros.is_multiple_of(10u32.pow(6 - declared)) {
        declared
    } else {
        6
    }
}

impl MySqlConnection {
    async fn execute_rows(
        &mut self,
        sql: &str,
        parameters: &[TypedCell],
        limits: QueryLimits,
        sink: &mut dyn RowSink,
    ) -> Result<DriverSummary, WorkbenchError> {
        super::validate_statement(sql, parameters)?;
        let values = parameters
            .iter()
            .map(mysql_parameter)
            .collect::<Result<Vec<_>, _>>()?;
        let result = tokio::time::timeout(limits.timeout, async {
            let connection = self
                .connection
                .as_mut()
                .ok_or(WorkbenchError::SessionStale)?;
            let mut result = connection
                .exec_iter(sql, mysql_async::Params::Positional(values))
                .await
                .map_err(mysql_statement_error)?;
            let mut summary = DriverSummary {
                rows_affected: Some(result.affected_rows()),
                ..DriverSummary::default()
            };
            while !result.is_empty() {
                let columns = result.columns_ref().to_vec();
                if columns.len() > 512 {
                    return Err(WorkbenchError::Limit);
                }
                sink.columns(
                    columns
                        .iter()
                        .map(|c| {
                            column(c.name_str().into_owned(), format!("{:?}", c.column_type()))
                        })
                        .collect(),
                )
                .await?;
                while let Some(row) = result.next().await.map_err(mysql_statement_error)? {
                    summary.rows_seen += 1;
                    if summary.capped || summary.rows_kept as usize >= limits.max_rows {
                        summary.capped = true;
                        continue;
                    }
                    let cells = (0..row.len())
                        .map(|index| {
                            mysql_cell(
                                row.as_ref(index).unwrap_or(&Value::NULL),
                                columns.get(index),
                            )
                        })
                        .collect::<Vec<_>>();
                    let bytes = encoded_row_bytes(&cells)?;
                    if summary.encoded_bytes.saturating_add(bytes) > limits.max_bytes {
                        summary.capped = true;
                        continue;
                    }
                    sink.row(cells).await?;
                    summary.rows_kept += 1;
                    summary.encoded_bytes += bytes;
                }
                summary.rows_affected = Some(result.affected_rows());
            }
            result.drop_result().await.map_err(mysql_statement_error)?;
            use mysql_async::consts::StatusFlags;
            summary.transaction_state = connection.last_ok_packet().map_or(
                super::DriverTransactionState::Unknown,
                |packet| {
                    let flags = packet.status_flags();
                    if flags.contains(StatusFlags::SERVER_STATUS_IN_TRANS) {
                        super::DriverTransactionState::Active
                    } else if flags.contains(StatusFlags::SERVER_STATUS_AUTOCOMMIT) {
                        super::DriverTransactionState::Idle
                    } else {
                        super::DriverTransactionState::Unknown
                    }
                },
            );
            Ok(summary)
        })
        .await;
        match result {
            Ok(Err(WorkbenchError::StatementRejected(_))) => {
                // ERR packets do not carry current transaction status. A closed
                // native no-op obtains a fresh OK status from this same socket.
                let connection = self
                    .connection
                    .as_mut()
                    .ok_or(WorkbenchError::SessionStale)?;
                tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    connection.query_drop("DO 0"),
                )
                .await
                .map_err(|_| WorkbenchError::Io)?
                .map_err(|_| WorkbenchError::Io)?;
                let state = connection.last_ok_packet().map_or(
                    super::DriverTransactionState::Unknown,
                    |packet| {
                        use mysql_async::consts::StatusFlags;
                        let flags = packet.status_flags();
                        if flags.contains(StatusFlags::SERVER_STATUS_IN_TRANS) {
                            super::DriverTransactionState::Active
                        } else if flags.contains(StatusFlags::SERVER_STATUS_AUTOCOMMIT) {
                            super::DriverTransactionState::Idle
                        } else {
                            super::DriverTransactionState::Unknown
                        }
                    },
                );
                if state == super::DriverTransactionState::Unknown {
                    Err(WorkbenchError::Io)
                } else {
                    Err(WorkbenchError::StatementRejected(state))
                }
            }
            Ok(result) => result,
            Err(_) => {
                // Abandon the physical session; timeout does not establish rollback.
                self.connection.take();
                Err(WorkbenchError::CancelUnsettled)
            }
        }
    }
}
