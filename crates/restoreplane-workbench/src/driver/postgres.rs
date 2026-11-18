use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use futures::{pin_mut, TryStreamExt};
use restoreplane_domain::workbench::{
    ActualContext, ConnectDescriptor, Engine, Mode, Route, TargetPurpose, TypedCell,
};
use tokio_postgres::Client;

use super::pg_frame::{BoundedPgStream, BoundedTls};
use super::{
    check_identity, column, encoded_row_bytes, root_certificate, DriverCancel, DriverConnection,
    DriverFacts, DriverLogin, DriverSummary, QueryLimits, RowSink,
};
use crate::WorkbenchError;

struct PostgresConnection {
    client: Client,
    task: tokio::task::JoinSet<()>,
    facts: DriverFacts,
    cancel: Arc<PgCancel>,
    exceeded: Arc<AtomicBool>,
    transaction_status: Arc<AtomicU8>,
}
struct PgCancel {
    token: tokio_postgres::CancelToken,
    tls: Option<native_tls::TlsConnector>,
    host: String,
    port: u16,
    server_name: String,
}

#[async_trait]
impl DriverCancel for PgCancel {
    async fn request(&self) -> Result<(), WorkbenchError> {
        let tcp = tokio::net::TcpStream::connect((self.host.as_str(), self.port))
            .await
            .map_err(|_| WorkbenchError::CancelUnsettled)?;
        match &self.tls {
            Some(tls) => {
                self.token
                    .cancel_query_raw(
                        tcp,
                        postgres_native_tls::TlsConnector::new(tls.clone(), &self.server_name),
                    )
                    .await
            }
            None => {
                self.token
                    .cancel_query_raw(tcp, tokio_postgres::NoTls)
                    .await
            }
        }
        .map_err(|_| WorkbenchError::CancelUnsettled)
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
    let mut config = tokio_postgres::Config::new();
    config
        .host(host)
        .port(*port)
        .user(&login.username)
        .password(login.secret.as_bytes())
        .dbname(&descriptor.database.name)
        .connect_timeout(std::time::Duration::from_secs(15));
    let tls = if plaintext {
        None
    } else {
        let root = tokio::task::spawn_blocking({
            let trust = descriptor.tls.root_trust.clone();
            move || root_certificate(trust.as_ref())
        })
        .await
        .map_err(|_| WorkbenchError::Io)??;
        let mut builder = native_tls::TlsConnector::builder();
        if let Some(bytes) = root {
            builder.disable_built_in_roots(true);
            builder.add_root_certificate(
                native_tls::Certificate::from_pem(&bytes)
                    .map_err(|_| WorkbenchError::InvalidInput)?,
            );
        }
        Some(builder.build().map_err(|_| WorkbenchError::Io)?)
    };
    let tcp = tokio::net::TcpStream::connect((host.as_str(), *port))
        .await
        .map_err(|_| WorkbenchError::Io)?;
    let exceeded = Arc::new(AtomicBool::new(false));
    let transaction_status = Arc::new(AtomicU8::new(0));
    let (client, mut task) = match &tls {
        Some(tls) => {
            config.ssl_mode(tokio_postgres::config::SslMode::Require);
            let server_name = descriptor.tls.server_name.as_deref().unwrap_or(host);
            let connector = BoundedTls {
                connector: postgres_native_tls::TlsConnector::new(tls.clone(), server_name),
                exceeded: exceeded.clone(),
                transaction_status: transaction_status.clone(),
            };
            let (client, connection) = config
                .connect_raw(tcp, connector)
                .await
                .map_err(|_| WorkbenchError::Io)?;
            let mut task = tokio::task::JoinSet::new();
            task.spawn(async move {
                let _ = connection.await;
            });
            (client, task)
        }
        None => {
            config.ssl_mode(tokio_postgres::config::SslMode::Disable);
            let (client, connection) = config
                .connect_raw(
                    BoundedPgStream::new(tcp, exceeded.clone())
                        .with_transaction_status(transaction_status.clone()),
                    tokio_postgres::NoTls,
                )
                .await
                .map_err(|_| WorkbenchError::Io)?;
            let mut task = tokio::task::JoinSet::new();
            task.spawn(async move {
                let _ = connection.await;
            });
            (client, task)
        }
    };
    let cancel = Arc::new(PgCancel {
        token: client.cancel_token(),
        tls,
        host: host.clone(),
        port: *port,
        server_name: descriptor
            .tls
            .server_name
            .clone()
            .unwrap_or_else(|| host.clone()),
    });
    let row = client
        .query_one(
            "SELECT pg_catalog.current_database()::pg_catalog.text, current_user::pg_catalog.text, pg_catalog.version(),(SELECT d.oid::pg_catalog.text FROM pg_catalog.pg_database d WHERE d.datname OPERATOR(pg_catalog.=) pg_catalog.current_database())",
            &[],
        )
        .await
        .map_err(|_| WorkbenchError::Io)?;
    let identity = client
        .query_opt(
            "SELECT system_identifier::pg_catalog.text FROM pg_catalog.pg_control_system()",
            &[],
        )
        .await
        .ok()
        .flatten()
        .map(|row| format!("postgresql-system-id:{}", row.get::<_, String>(0)));
    let facts = DriverFacts {
        context: ActualContext {
            endpoint: format!("{host}:{port}"),
            database: row.get(0),
            role: row.get(1),
            purpose: TargetPurpose::Source,
        },
        engine: Engine::Postgresql,
        server_version: row.get(2),
        server_identity: identity,
        actual_database_identity: row
            .get::<_, Option<String>>(3)
            .map(|oid| format!("postgresql-database-oid:{oid}")),
        read_only_held: false,
        verified_tls: !plaintext,
    };
    if let Err(error) = check_identity(descriptor, &facts) {
        task.abort_all();
        return Err(error);
    }
    let mut connection = PostgresConnection {
        client,
        task,
        facts,
        cancel,
        exceeded,
        transaction_status,
    };
    connection.set_read_only(mode == Mode::ReadOnly).await?;
    Ok(Box::new(connection))
}

#[async_trait]
impl DriverConnection for PostgresConnection {
    fn facts(&self) -> &DriverFacts {
        &self.facts
    }
    fn cancel_handle(&self) -> Arc<dyn DriverCancel> {
        self.cancel.clone()
    }
    async fn transaction_read_only(&mut self) -> Result<bool, WorkbenchError> {
        let value: String = self
            .client
            .query_one("SHOW transaction_read_only", &[])
            .await
            .map_err(|_| WorkbenchError::Io)?
            .get(0);
        Ok(value == "on" && self.transaction_status.load(Ordering::SeqCst) == b'T')
    }
    async fn configure_limits(
        &mut self,
        lock_wait: std::time::Duration,
    ) -> Result<(), WorkbenchError> {
        let ms = lock_wait.as_millis();
        if ms == 0 || ms > 30_000 {
            return Err(WorkbenchError::Limit);
        }
        self.client
            .query_one(
                "SELECT pg_catalog.set_config('lock_timeout',$1::pg_catalog.text,false)",
                &[&ms.to_string()],
            )
            .await
            .map_err(|_| WorkbenchError::Io)?;
        Ok(())
    }
    async fn set_read_only(&mut self, read_only: bool) -> Result<(), WorkbenchError> {
        self.client.batch_execute(if read_only { "SET default_transaction_read_only = on; SET statement_timeout = 30000; SET lock_timeout = 2000" } else { "SET default_transaction_read_only = off; SET statement_timeout = 30000; SET lock_timeout = 2000" }).await.map_err(|_| WorkbenchError::Io)?;
        let observed: String = self
            .client
            .query_one("SHOW default_transaction_read_only", &[])
            .await
            .map_err(|_| WorkbenchError::Io)?
            .get(0);
        if (observed == "on") != read_only {
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
        self.task.abort_all();
        Ok(())
    }
}

impl Drop for PostgresConnection {
    fn drop(&mut self) {
        self.task.abort_all();
    }
}

impl PostgresConnection {
    async fn observed_query_error(&self, error: tokio_postgres::Error) -> WorkbenchError {
        if self.exceeded.load(Ordering::SeqCst) {
            return WorkbenchError::Limit;
        }
        let Some(database) = error.as_db_error() else {
            return WorkbenchError::Io;
        };
        // Cancellation, connection failures and shutdowns remain uncertain. A
        // server ErrorResponse alone is insufficient: wait for this command's
        // fresh ReadyForQuery state, without sending another user statement.
        let code = database.code().code();
        if code.starts_with("08") || code.starts_with("57") {
            return WorkbenchError::Io;
        }
        // ErrorResponse may precede the command's ReadyForQuery. The public
        // check_connection API sends Sync and awaits its own Ready, so stale
        // prepare/Close messages cannot be mistaken for current transaction state.
        match tokio::time::timeout(
            std::time::Duration::from_secs(1),
            self.client.check_connection(),
        )
        .await
        {
            Ok(Ok(())) => match self.transaction_status.load(Ordering::SeqCst) {
                b'I' => WorkbenchError::StatementRejected(super::DriverTransactionState::Idle),
                b'T' => WorkbenchError::StatementRejected(super::DriverTransactionState::Active),
                b'E' => WorkbenchError::StatementRejected(super::DriverTransactionState::Failed),
                _ => WorkbenchError::Io,
            },
            _ => WorkbenchError::Io,
        }
    }
}

struct RawPgValue(Vec<u8>);
impl<'a> tokio_postgres::types::FromSql<'a> for RawPgValue {
    fn from_sql(
        _: &tokio_postgres::types::Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Self(raw.into()))
    }
    fn accepts(_: &tokio_postgres::types::Type) -> bool {
        true
    }
}

fn pg_cell(native_kind: &tokio_postgres::types::Type, raw: &[u8]) -> TypedCell {
    let kind = native_kind.name();
    let unsupported = || TypedCell::Unsupported {
        type_name: kind.into(),
        reason: "codec_or_inline_limit".into(),
    };
    if native_kind.schema() != "pg_catalog"
        || !crate::types::postgresql_builtin_type_id(kind, native_kind.oid())
    {
        return unsupported();
    }
    if raw.len() > 64 * 1024 {
        return unsupported();
    }
    match kind {
        "bool" if raw.len() == 1 => TypedCell::Bool { value: raw[0] != 0 },
        "int2" if raw.len() == 2 => TypedCell::I64 {
            value: i16::from_be_bytes(raw.try_into().expect("length checked")).to_string(),
        },
        "int4" if raw.len() == 4 => TypedCell::I64 {
            value: i32::from_be_bytes(raw.try_into().expect("length checked")).to_string(),
        },
        "int8" if raw.len() == 8 => TypedCell::I64 {
            value: i64::from_be_bytes(raw.try_into().expect("length checked")).to_string(),
        },
        "float4" if raw.len() == 4 => TypedCell::Float {
            value: super::float_text(f64::from(f32::from_be_bytes(
                raw.try_into().expect("length checked"),
            ))),
        },
        "float8" if raw.len() == 8 => TypedCell::Float {
            value: super::float_text(f64::from_be_bytes(raw.try_into().expect("length checked"))),
        },
        "bytea" => TypedCell::Bytes {
            base64: base64::engine::general_purpose::STANDARD.encode(raw),
        },
        "numeric" => match numeric(raw) {
            Some((value, scale)) => TypedCell::Decimal {
                value,
                scale: Some(scale),
            },
            None => unsupported(),
        },
        "text" | "varchar" | "bpchar" | "name" | "char" | "json" | "xml" => {
            match std::str::from_utf8(raw) {
                Ok(value) => TypedCell::Text {
                    value: value.into(),
                },
                Err(_) => unsupported(),
            }
        }
        "jsonb" if raw.first() == Some(&1) => match std::str::from_utf8(&raw[1..]) {
            Ok(value) => TypedCell::Text {
                value: value.into(),
            },
            Err(_) => unsupported(),
        },
        "uuid" if raw.len() == 16 => TypedCell::Text {
            value: uuid::Uuid::from_slice(raw)
                .expect("length checked")
                .to_string(),
        },
        "timestamp" | "timestamptz" if raw.len() == 8 => {
            let micros = i64::from_be_bytes(raw.try_into().expect("length checked"));
            let base = chrono::NaiveDate::from_ymd_opt(2000, 1, 1)
                .expect("valid epoch")
                .and_hms_opt(0, 0, 0)
                .expect("valid epoch");
            match base.checked_add_signed(chrono::TimeDelta::microseconds(micros)) {
                Some(value) => TypedCell::Temporal {
                    value: value.to_string(),
                    type_name: kind.into(),
                    offset: (kind == "timestamptz").then(|| "+00:00".into()),
                    precision: 6,
                },
                None => unsupported(),
            }
        }
        "time" | "timetz" if raw.len() == if kind == "time" { 8 } else { 12 } => {
            let micros = i64::from_be_bytes(raw[..8].try_into().expect("length checked"));
            if !(0..=86_400_000_000).contains(&micros) {
                return unsupported();
            }
            let hour = micros / 3_600_000_000;
            let minute = (micros / 60_000_000) % 60;
            let second = (micros / 1_000_000) % 60;
            let offset = if kind == "timetz" {
                let seconds = -i64::from(i32::from_be_bytes(
                    raw[8..].try_into().expect("length checked"),
                ));
                if seconds.abs() > 57_600 {
                    return unsupported();
                }
                Some(format!(
                    "{}{:02}:{:02}:{:02}",
                    if seconds < 0 { "-" } else { "+" },
                    seconds.abs() / 3600,
                    seconds.abs() / 60 % 60,
                    seconds.abs() % 60
                ))
            } else {
                None
            };
            TypedCell::Temporal {
                value: format!(
                    "{hour:02}:{minute:02}:{second:02}.{:06}",
                    micros % 1_000_000
                ),
                type_name: kind.into(),
                offset,
                precision: 6,
            }
        }
        "date" if raw.len() == 4 => {
            let days = i32::from_be_bytes(raw.try_into().expect("length checked"));
            match chrono::NaiveDate::from_ymd_opt(2000, 1, 1)
                .and_then(|base| base.checked_add_signed(chrono::TimeDelta::days(i64::from(days))))
            {
                Some(value) => TypedCell::Temporal {
                    value: value.to_string(),
                    type_name: kind.into(),
                    offset: None,
                    precision: 0,
                },
                None => unsupported(),
            }
        }
        _ => unsupported(),
    }
}

fn numeric(raw: &[u8]) -> Option<(String, u32)> {
    if raw.len() < 8 {
        return None;
    }
    let count = u16::from_be_bytes(raw[0..2].try_into().ok()?) as usize;
    let weight = i16::from_be_bytes(raw[2..4].try_into().ok()?) as i32;
    let sign = u16::from_be_bytes(raw[4..6].try_into().ok()?);
    let scale = u16::from_be_bytes(raw[6..8].try_into().ok()?) as usize;
    if raw.len() != 8 + count * 2
        || weight.abs() > 4096
        || scale > 16384
        || !matches!(sign, 0 | 0x4000)
    {
        return None;
    }
    let digits = raw[8..]
        .chunks_exact(2)
        .map(|bytes| u16::from_be_bytes([bytes[0], bytes[1]]))
        .collect::<Vec<_>>();
    if digits.iter().any(|digit| *digit >= 10000) {
        return None;
    }
    let digit = |position: i32| -> u16 {
        let index = weight - position;
        if index >= 0 {
            digits.get(index as usize).copied().unwrap_or(0)
        } else {
            0
        }
    };
    let mut integer = if weight < 0 {
        "0".into()
    } else {
        let mut value = digit(weight).to_string();
        for position in (0..weight).rev() {
            value.push_str(&format!("{:04}", digit(position)));
        }
        value
    };
    if sign == 0x4000 && digits.iter().any(|digit| *digit != 0) {
        integer.insert(0, '-');
    }
    if scale != 0 {
        let mut fractional = String::new();
        for group in 1..=scale.div_ceil(4) {
            fractional.push_str(&format!("{:04}", digit(-(group as i32))));
        }
        fractional.truncate(scale);
        integer.push('.');
        integer.push_str(&fractional);
    }
    Some((integer, scale as u32))
}

struct PgParameter {
    kind: tokio_postgres::types::Type,
    bytes: Option<Vec<u8>>,
}
impl std::fmt::Debug for PgParameter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PgParameter([redacted])")
    }
}
impl tokio_postgres::types::ToSql for PgParameter {
    fn to_sql(
        &self,
        kind: &tokio_postgres::types::Type,
        out: &mut bytes::BytesMut,
    ) -> Result<tokio_postgres::types::IsNull, Box<dyn std::error::Error + Sync + Send>> {
        if kind != &self.kind {
            return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput).into());
        }
        match &self.bytes {
            None => Ok(tokio_postgres::types::IsNull::Yes),
            Some(bytes) => {
                out.extend_from_slice(bytes);
                Ok(tokio_postgres::types::IsNull::No)
            }
        }
    }
    fn accepts(_: &tokio_postgres::types::Type) -> bool {
        true
    }
    tokio_postgres::types::to_sql_checked!();
}

fn pg_parameter(
    cell: &TypedCell,
    kind: &tokio_postgres::types::Type,
) -> Result<PgParameter, WorkbenchError> {
    if kind.schema() != "pg_catalog"
        || !crate::types::postgresql_builtin_type_id(kind.name(), kind.oid())
    {
        return Err(WorkbenchError::Unsupported);
    }
    let bytes = match (cell, kind.name()) {
        (TypedCell::Null, _) => None,
        (TypedCell::Bool { value }, "bool") => Some(vec![u8::from(*value)]),
        (TypedCell::I64 { value }, "int2") => Some(
            value
                .parse::<i16>()
                .map_err(|_| WorkbenchError::InvalidInput)?
                .to_be_bytes()
                .to_vec(),
        ),
        (TypedCell::I64 { value }, "int4") => Some(
            value
                .parse::<i32>()
                .map_err(|_| WorkbenchError::InvalidInput)?
                .to_be_bytes()
                .to_vec(),
        ),
        (TypedCell::I64 { value }, "int8") => Some(
            value
                .parse::<i64>()
                .map_err(|_| WorkbenchError::InvalidInput)?
                .to_be_bytes()
                .to_vec(),
        ),
        (
            TypedCell::I64 { value } | TypedCell::U64 { value } | TypedCell::Decimal { value, .. },
            "numeric",
        ) => Some(encode_numeric(value).ok_or(WorkbenchError::InvalidInput)?),
        (TypedCell::Text { value }, "text" | "varchar" | "bpchar" | "name" | "json" | "xml")
            if value.len() <= 64 * 1024 =>
        {
            Some(value.as_bytes().to_vec())
        }
        (TypedCell::Text { value }, "uuid") => Some(
            uuid::Uuid::parse_str(value)
                .map_err(|_| WorkbenchError::InvalidInput)?
                .as_bytes()
                .to_vec(),
        ),
        (TypedCell::Bytes { base64 }, "bytea") if base64.len() <= 88 * 1024 => Some(
            base64::engine::general_purpose::STANDARD
                .decode(base64)
                .map_err(|_| WorkbenchError::InvalidInput)?,
        ),
        (TypedCell::Float { value }, "float8") => Some(
            value
                .parse::<f64>()
                .map_err(|_| WorkbenchError::InvalidInput)?
                .to_be_bytes()
                .to_vec(),
        ),
        (TypedCell::Float { value }, "float4") => Some(
            value
                .parse::<f32>()
                .map_err(|_| WorkbenchError::InvalidInput)?
                .to_be_bytes()
                .to_vec(),
        ),
        _ => return Err(WorkbenchError::Unsupported),
    };
    Ok(PgParameter {
        kind: kind.clone(),
        bytes,
    })
}

fn encode_numeric(text: &str) -> Option<Vec<u8>> {
    if text.len() > 16 * 1024 || text.is_empty() {
        return None;
    }
    let negative = text.starts_with('-');
    let unsigned = text.strip_prefix('-').unwrap_or(text);
    let (whole, fractional) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    if whole.is_empty()
        || !whole
            .bytes()
            .chain(fractional.bytes())
            .all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let scale = u16::try_from(fractional.len()).ok()?;
    let mut padded = "0".repeat((4 - whole.len() % 4) % 4);
    padded.push_str(whole);
    padded.push_str(fractional);
    padded.push_str(&"0".repeat((4 - fractional.len() % 4) % 4));
    let mut digits = padded
        .as_bytes()
        .chunks_exact(4)
        .map(|chunk| std::str::from_utf8(chunk).ok()?.parse::<u16>().ok())
        .collect::<Option<Vec<_>>>()?;
    let mut weight = i16::try_from(whole.len().div_ceil(4)).ok()? - 1;
    let leading = digits.iter().take_while(|digit| **digit == 0).count();
    digits.drain(..leading);
    weight -= i16::try_from(leading).ok()?;
    while digits.last() == Some(&0) {
        digits.pop();
    }
    if digits.is_empty() {
        weight = 0;
    }
    let mut encoded = Vec::new();
    encoded.extend_from_slice(&u16::try_from(digits.len()).ok()?.to_be_bytes());
    encoded.extend_from_slice(&weight.to_be_bytes());
    encoded.extend_from_slice(
        &(if negative && !digits.is_empty() {
            0x4000_u16
        } else {
            0
        })
        .to_be_bytes(),
    );
    encoded.extend_from_slice(&scale.to_be_bytes());
    for digit in digits {
        encoded.extend_from_slice(&digit.to_be_bytes());
    }
    Some(encoded)
}

impl PostgresConnection {
    async fn execute_rows(
        &mut self,
        sql: &str,
        parameters: &[TypedCell],
        limits: QueryLimits,
        sink: &mut dyn RowSink,
    ) -> Result<DriverSummary, WorkbenchError> {
        super::validate_statement(sql, parameters)?;
        let result = tokio::time::timeout(limits.timeout, async {
            self.transaction_status.store(0, Ordering::SeqCst);
            // Exactly one Parse, followed by execution of that same prepared unit.
            // Its metadata also describes a successful result containing zero rows.
            let statement = match self.client.prepare(sql).await {
                Ok(statement) => statement,
                Err(error) => return Err(self.observed_query_error(error).await),
            };
            // prepare() returns after RowDescription, before its ReadyForQuery.
            // A protocol Sync barrier consumes that response (and any queued
            // statement-close response) before observing this execution's state.
            // This does not Parse or execute the user's SQL a second time.
            self.client
                .check_connection()
                .await
                .map_err(|_| WorkbenchError::Io)?;
            if statement.columns().len() > 512 {
                return Err(WorkbenchError::Limit);
            }
            if statement.params().len() != parameters.len() {
                return Err(WorkbenchError::InvalidInput);
            }
            sink.columns(
                statement
                    .columns()
                    .iter()
                    .map(|c| column(c.name().into(), c.type_().name().into()))
                    .collect(),
            )
            .await?;
            let parameters = parameters
                .iter()
                .zip(statement.params())
                .map(|(cell, kind)| pg_parameter(cell, kind))
                .collect::<Result<Vec<_>, _>>()?;
            self.transaction_status.store(0, Ordering::SeqCst);
            let stream = match self
                .client
                .query_raw(
                    &statement,
                    parameters
                        .iter()
                        .map(|parameter| parameter as &(dyn tokio_postgres::types::ToSql + Sync)),
                )
                .await
            {
                Ok(stream) => stream,
                Err(error) => return Err(self.observed_query_error(error).await),
            };
            pin_mut!(stream);
            let mut summary = DriverSummary::default();
            loop {
                let row = match stream.try_next().await {
                    Ok(Some(row)) => row,
                    Ok(None) => break,
                    Err(error) => return Err(self.observed_query_error(error).await),
                };
                summary.rows_seen += 1;
                if summary.capped || summary.rows_kept as usize >= limits.max_rows {
                    summary.capped = true;
                    continue;
                }
                let cells = (0..row.len())
                    .map(|index| match row.try_get::<_, Option<RawPgValue>>(index) {
                        Ok(None) => TypedCell::Null,
                        Ok(Some(value)) => pg_cell(row.columns()[index].type_(), &value.0),
                        Err(_) => TypedCell::Unsupported {
                            type_name: row.columns()[index].type_().name().into(),
                            reason: "codec_not_certified".into(),
                        },
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
            summary.rows_affected = stream.rows_affected();
            summary.transaction_state = match self.transaction_status.load(Ordering::SeqCst) {
                b'I' => super::DriverTransactionState::Idle,
                b'T' => super::DriverTransactionState::Active,
                b'E' => super::DriverTransactionState::Failed,
                _ => super::DriverTransactionState::Unknown,
            };
            Ok(summary)
        })
        .await;
        match result {
            Ok(result) => result,
            Err(_) => {
                // Dropping the transport is not proof that a remote write rolled back.
                self.task.abort_all();
                Err(WorkbenchError::CancelUnsettled)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn numeric_binary_roundtrip_keeps_sign_precision_and_scale() {
        for value in [
            "0",
            "0.000000000000",
            "-0.0001",
            "123456789012345678.123456789012",
            "-9223372036854775808",
            "18446744073709551615",
        ] {
            assert_eq!(numeric(&encode_numeric(value).unwrap()).unwrap().0, value);
        }
        assert!(encode_numeric("1; DROP TABLE x").is_none());
    }
}
