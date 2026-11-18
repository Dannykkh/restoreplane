//! Private-driver values stop here; consumers receive the shared lossless DTOs.
mod mysql;
mod pg_frame;
mod postgres;
mod sqlite;

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use restoreplane_domain::workbench::{
    ActualContext, Column, ConnectDescriptor, Engine, Mode, RootTrust, Route, TypedCell,
};
use zeroize::Zeroizing;

use crate::WorkbenchError;

pub struct DriverLogin {
    pub username: String,
    pub secret: Zeroizing<String>,
}

impl fmt::Debug for DriverLogin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DriverLogin([redacted])")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriverFacts {
    pub context: ActualContext,
    pub engine: Engine,
    pub server_version: String,
    pub server_identity: Option<String>,
    /// Observed database incarnation; never inferred from the registered name.
    pub actual_database_identity: Option<String>,
    pub read_only_held: bool,
    pub verified_tls: bool,
}

/// These flags are runtime certification, not an inference from a DB logo.
#[derive(Clone, Debug, Default)]
pub struct CertifiedCapabilities {
    pub read_only_lease: bool,
    pub verified_editing: bool,
    pub snapshot: bool,
}

#[derive(Clone, Debug)]
pub struct QueryLimits {
    pub max_rows: usize,
    pub max_bytes: usize,
    pub timeout: Duration,
}

impl Default for QueryLimits {
    fn default() -> Self {
        Self {
            max_rows: 1000,
            max_bytes: 64 * 1024 * 1024,
            timeout: Duration::from_secs(30),
        }
    }
}

impl QueryLimits {
    pub fn validate(&self) -> Result<(), WorkbenchError> {
        if self.max_rows == 0
            || self.max_rows > 1_000_000
            || self.max_bytes == 0
            || self.max_bytes > 64 * 1024 * 1024
            || self.timeout.is_zero()
            || self.timeout > Duration::from_secs(300)
        {
            return Err(WorkbenchError::Limit);
        }
        Ok(())
    }
}
/// Native file-read totals, independent of the interactive retained-result
/// budget. No wire DTO deserializes this type; row/packet buffers keep their
/// existing limits while a bounded sink streams to its destination.
#[derive(Clone, Debug)]
pub struct FileReadLimits(QueryLimits);
impl Default for FileReadLimits {
    fn default() -> Self {
        Self(QueryLimits {
            max_rows: 1_000_000,
            max_bytes: 1024 * 1024 * 1024,
            timeout: Duration::from_secs(300),
        })
    }
}
impl FileReadLimits {
    pub fn bounded(
        max_rows: usize,
        max_bytes: usize,
        timeout: Duration,
    ) -> Result<Self, WorkbenchError> {
        if max_rows == 0
            || max_rows > 1_000_000
            || max_bytes == 0
            || max_bytes > 1024 * 1024 * 1024
            || timeout.is_zero()
            || timeout > Duration::from_secs(300)
        {
            return Err(WorkbenchError::Limit);
        }
        Ok(Self(QueryLimits {
            max_rows,
            max_bytes,
            timeout,
        }))
    }
    pub fn timeout(&self) -> Duration {
        self.0.timeout
    }
    pub(crate) fn query_limits(&self) -> QueryLimits {
        self.0.clone()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DriverSummary {
    pub rows_seen: u64,
    pub rows_kept: u64,
    pub encoded_bytes: usize,
    pub rows_affected: Option<u64>,
    pub capped: bool,
    pub transaction_state: DriverTransactionState,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DriverTransactionState {
    #[default]
    Unknown,
    Idle,
    Active,
    Failed,
}

/// The consumer awaits its own byte-credit reservation before returning.
#[async_trait]
pub trait RowSink: Send {
    async fn columns(&mut self, columns: Vec<Column>) -> Result<(), WorkbenchError>;
    async fn row(&mut self, cells: Vec<TypedCell>) -> Result<(), WorkbenchError>;
}

#[async_trait]
pub trait DriverCancel: Send + Sync {
    /// A successful request is not evidence of rollback or cancellation settlement.
    async fn request(&self) -> Result<(), WorkbenchError>;
}

/// Closed native transaction control. No frontend or arbitrary SQL text is accepted.
#[derive(Clone, Copy)]
pub enum TransactionCommand {
    StandaloneWriteBegin,
    SnapshotBegin {
        serializable: bool,
    },
    ReadOnlyStatementBegin,
    ManualBegin {
        isolation: TransactionIsolation,
        read_only: bool,
    },
    Commit,
    Rollback,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TransactionIsolation {
    Default,
    ReadCommitted,
    RepeatableRead,
    Serializable,
}
impl TransactionIsolation {
    pub fn parse(value: &str) -> Result<Self, WorkbenchError> {
        match value {
            "default" => Ok(Self::Default),
            "read_committed" => Ok(Self::ReadCommitted),
            "repeatable_read" => Ok(Self::RepeatableRead),
            "serializable" => Ok(Self::Serializable),
            _ => Err(WorkbenchError::Unsupported),
        }
    }
    fn sql(self) -> &'static str {
        match self {
            Self::Default => "",
            Self::ReadCommitted => "READ COMMITTED",
            Self::RepeatableRead => "REPEATABLE READ",
            Self::Serializable => "SERIALIZABLE",
        }
    }
}
impl TransactionCommand {
    pub fn statements(self, engine: Engine) -> Result<Vec<String>, WorkbenchError> {
        if let Self::ManualBegin {
            isolation,
            read_only,
        } = self
        {
            let access = if read_only { "READ ONLY" } else { "READ WRITE" };
            return Ok(match engine {
                Engine::Postgresql => vec![format!(
                    "BEGIN {} {access}",
                    if isolation == TransactionIsolation::Default {
                        String::new()
                    } else {
                        format!("ISOLATION LEVEL {}", isolation.sql())
                    }
                )],
                Engine::Sqlite => {
                    if !matches!(
                        isolation,
                        TransactionIsolation::Default | TransactionIsolation::Serializable
                    ) {
                        return Err(WorkbenchError::Unsupported);
                    }
                    vec!["BEGIN DEFERRED".into()]
                }
                _ => {
                    let mut statements = Vec::with_capacity(2);
                    if isolation != TransactionIsolation::Default {
                        statements.push(format!(
                            "SET TRANSACTION ISOLATION LEVEL {}",
                            isolation.sql()
                        ));
                    }
                    statements.push(format!("START TRANSACTION {access}"));
                    statements
                }
            });
        }
        let statements: &[&str] = match (self, engine) {
            (Self::StandaloneWriteBegin, Engine::Sqlite) => &["BEGIN IMMEDIATE"],
            (Self::StandaloneWriteBegin, Engine::Postgresql) => &["BEGIN"],
            (Self::StandaloneWriteBegin, _) => &["START TRANSACTION READ WRITE"],
            (Self::ReadOnlyStatementBegin, Engine::Postgresql) => &["BEGIN READ ONLY"],
            (Self::ReadOnlyStatementBegin, Engine::Sqlite) => &["BEGIN DEFERRED"],
            (Self::ReadOnlyStatementBegin, _) => &["START TRANSACTION READ ONLY"],
            (
                Self::SnapshotBegin {
                    serializable: false,
                },
                Engine::Postgresql,
            ) => &["BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY"],
            (Self::SnapshotBegin { serializable: true }, Engine::Postgresql) => {
                &["BEGIN ISOLATION LEVEL SERIALIZABLE READ ONLY"]
            }
            (Self::SnapshotBegin { .. }, Engine::Sqlite) => &["BEGIN DEFERRED"],
            (
                Self::SnapshotBegin {
                    serializable: false,
                },
                _,
            ) => &[
                "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
                "START TRANSACTION WITH CONSISTENT SNAPSHOT, READ ONLY",
            ],
            (Self::SnapshotBegin { serializable: true }, _) => {
                return Err(WorkbenchError::Unsupported)
            }
            (Self::Commit, _) => &["COMMIT"],
            (Self::Rollback, _) => &["ROLLBACK"],
            (Self::ManualBegin { .. }, _) => return Err(WorkbenchError::Unsupported),
        };
        Ok(statements.iter().map(|sql| (*sql).to_owned()).collect())
    }
}
struct ControlSink;
#[async_trait]
impl RowSink for ControlSink {
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

#[async_trait]
pub trait DriverConnection: Send {
    fn facts(&self) -> &DriverFacts;
    fn cancel_handle(&self) -> std::sync::Arc<dyn DriverCancel>;
    async fn set_read_only(&mut self, read_only: bool) -> Result<(), WorkbenchError>;
    async fn transaction_read_only(&mut self) -> Result<bool, WorkbenchError> {
        Ok(false)
    }
    async fn configure_limits(&mut self, lock_wait: Duration) -> Result<(), WorkbenchError> {
        if lock_wait == Duration::from_secs(2) {
            Ok(())
        } else {
            Err(WorkbenchError::Unsupported)
        }
    }
    async fn transaction_control(
        &mut self,
        command: TransactionCommand,
    ) -> Result<(), WorkbenchError> {
        for sql in command.statements(self.facts().engine)? {
            self.execute(&sql, QueryLimits::default(), &mut ControlSink)
                .await?;
        }
        Ok(())
    }
    /// Internal Rust contract only. The query-policy/journal module owns admission.
    async fn execute(
        &mut self,
        sql: &str,
        limits: QueryLimits,
        sink: &mut dyn RowSink,
    ) -> Result<DriverSummary, WorkbenchError>;
    async fn execute_bound(
        &mut self,
        sql: &str,
        parameters: &[TypedCell],
        limits: QueryLimits,
        sink: &mut dyn RowSink,
    ) -> Result<DriverSummary, WorkbenchError> {
        if !parameters.is_empty() {
            return Err(WorkbenchError::Unsupported);
        }
        self.execute(sql, limits, sink).await
    }
    /// Only the native approved file-read resolver calls this larger total
    /// budget. Default adapters fail closed until their same decoder is wired.
    async fn execute_file_read(
        &mut self,
        _sql: &str,
        _limits: FileReadLimits,
        _sink: &mut dyn RowSink,
    ) -> Result<DriverSummary, WorkbenchError> {
        Err(WorkbenchError::Unsupported)
    }
    async fn close(&mut self) -> Result<(), WorkbenchError>;
}

#[async_trait]
pub trait DriverFactory: Send + Sync {
    async fn connect(
        &self,
        descriptor: &ConnectDescriptor,
        login: DriverLogin,
        mode: Mode,
    ) -> Result<Box<dyn DriverConnection>, WorkbenchError>;
}

#[derive(Default)]
pub struct NativeDriverFactory {
    explicit_plaintext_tests: bool,
}

impl NativeDriverFactory {
    /// Integration fixtures only. Production composition always uses Default.
    /// Plaintext still requires an explicit disabled mode and loopback TCP route.
    pub fn explicit_loopback_test_transport() -> Self {
        Self {
            explicit_plaintext_tests: true,
        }
    }

    fn plaintext_allowed(&self, descriptor: &ConnectDescriptor) -> bool {
        self.explicit_plaintext_tests
            && descriptor.tls.mode == "disable"
            && matches!(&descriptor.route, Route::Tcp { host, .. } if host.parse::<std::net::IpAddr>().is_ok_and(|address| address.is_loopback()))
    }
}

#[async_trait]
impl DriverFactory for NativeDriverFactory {
    async fn connect(
        &self,
        descriptor: &ConnectDescriptor,
        login: DriverLogin,
        mode: Mode,
    ) -> Result<Box<dyn DriverConnection>, WorkbenchError> {
        if !descriptor.transport_options.is_empty() {
            return Err(WorkbenchError::Unsupported);
        }
        if descriptor.engine == Engine::Sqlite {
            return sqlite::connect(descriptor, mode).await;
        }
        let plaintext = self.plaintext_allowed(descriptor);
        if !plaintext && descriptor.tls.mode != "verify-full" {
            return Err(WorkbenchError::Unsupported);
        }
        match descriptor.engine {
            Engine::Postgresql => postgres::connect(descriptor, login, mode, plaintext).await,
            Engine::Mysql | Engine::Mariadb => {
                mysql::connect(descriptor, login, mode, plaintext).await
            }
            Engine::Sqlite => unreachable!("handled above"),
        }
    }
}

pub(crate) fn root_certificate(
    trust: Option<&RootTrust>,
) -> Result<Option<Vec<u8>>, WorkbenchError> {
    let Some(RootTrust::Custom {
        canonical_path,
        content_digest,
    }) = trust
    else {
        return Ok(None);
    };
    let file = std::fs::File::open(canonical_path).map_err(|_| WorkbenchError::Io)?;
    let mut bytes = Vec::new();
    std::io::Read::take(file, 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| WorkbenchError::Io)?;
    if bytes.len() > 1024 * 1024 || blake3::hash(&bytes).to_hex().as_str() != content_digest {
        return Err(WorkbenchError::BindingStale);
    }
    Ok(Some(bytes))
}

pub(crate) fn column(name: String, type_name: String) -> Column {
    Column {
        column_id: uuid::Uuid::new_v4(),
        name,
        type_name,
        nullable: true,
        editable: false,
        readonly_reason: Some("origin_and_key_not_certified".into()),
    }
}

pub(crate) fn check_identity(
    descriptor: &ConnectDescriptor,
    facts: &DriverFacts,
) -> Result<(), WorkbenchError> {
    if descriptor.database.name != facts.context.database
        || descriptor
            .expected_server_identity
            .as_ref()
            .is_some_and(|expected| facts.server_identity.as_ref() != Some(expected))
    {
        return Err(WorkbenchError::BindingStale);
    }
    Ok(())
}

pub(crate) fn encoded_row_bytes(cells: &[TypedCell]) -> Result<usize, WorkbenchError> {
    let bytes = serde_json::to_vec(cells)
        .map_err(|_| WorkbenchError::InvalidInput)?
        .len();
    if bytes > 256 * 1024 || cells.len() > 512 {
        return Err(WorkbenchError::Limit);
    }
    Ok(bytes)
}

pub(crate) fn validate_statement(
    sql: &str,
    parameters: &[TypedCell],
) -> Result<(), WorkbenchError> {
    if sql.len() > 1024 * 1024 || parameters.len() > 16384 {
        return Err(WorkbenchError::Limit);
    }
    let mut encoded = 0_usize;
    for cell in parameters {
        crate::types::validate_cells(std::slice::from_ref(cell))?;
        match cell {
            TypedCell::Unsupported { .. } | TypedCell::Large { .. } => {
                return Err(WorkbenchError::Unsupported)
            }
            TypedCell::Text { value } | TypedCell::Decimal { value, .. }
                if value.len() > 64 * 1024 =>
            {
                return Err(WorkbenchError::Limit)
            }
            TypedCell::Bytes { base64 } if base64.len() > 88 * 1024 => {
                return Err(WorkbenchError::Limit)
            }
            _ => {}
        }
        encoded = encoded.saturating_add(
            serde_json::to_vec(cell)
                .map_err(|_| WorkbenchError::InvalidInput)?
                .len(),
        );
        if encoded > 512 * 1024 {
            return Err(WorkbenchError::Limit);
        }
    }
    Ok(())
}

pub(crate) fn float_text(value: f64) -> String {
    if value.is_nan() {
        "NaN".into()
    } else if value == f64::INFINITY {
        "Infinity".into()
    } else if value == f64::NEG_INFINITY {
        "-Infinity".into()
    } else {
        value.to_string()
    }
}

pub(crate) fn text_cell(value: &str, type_name: &str) -> TypedCell {
    if value.len() > 64 * 1024 {
        return TypedCell::Unsupported {
            type_name: type_name.into(),
            reason: "inline_cell_limit".into(),
        };
    }
    match type_name {
        "bool" => TypedCell::Bool {
            value: value == "t" || value == "1",
        },
        "int2" | "int4" | "int8" => match value.parse::<i64>() {
            Ok(value) => TypedCell::I64 {
                value: value.to_string(),
            },
            Err(_) => TypedCell::Unsupported {
                type_name: type_name.into(),
                reason: "invalid_integer".into(),
            },
        },
        "numeric" | "decimal" | "newdecimal" => TypedCell::Decimal {
            value: value.into(),
            scale: value
                .split_once('.')
                .map(|(_, fractional)| fractional.len() as u32),
        },
        "float4" | "float8" | "float" | "double" => TypedCell::Float {
            value: value.into(),
        },
        "date" | "time" | "timestamp" | "timestamptz" | "timetz" | "datetime" => {
            TypedCell::Temporal {
                value: value.into(),
                type_name: type_name.into(),
                offset: None,
                precision: 0,
            }
        }
        _ => TypedCell::Text {
            value: value.into(),
        },
    }
}

use std::io::Read;
