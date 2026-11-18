//! Lossless cells and bounded native metadata reads; display formatting is UI-owned.
use crate::driver::{DriverConnection, QueryLimits, RowSink};
use crate::result_stream::{encoded_size, MemoryReservation, NativeBudget};
use crate::WorkbenchError;
use async_trait::async_trait;
use base64::Engine as _;
use restoreplane_domain::workbench::{Column, Engine, TypedCell};
use restoreplane_domain::workbench_metadata::Row;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}
/// Builtin identity, rather than a potentially shadowed PostgreSQL type name.
pub fn postgresql_builtin_type_id(name: &str, oid: u32) -> bool {
    matches!(
        (name, oid),
        ("bool", 16)
            | ("bytea", 17)
            | ("char", 18)
            | ("name", 19)
            | ("int8", 20)
            | ("int2", 21)
            | ("int4", 23)
            | ("text", 25)
            | ("json", 114)
            | ("xml", 142)
            | ("float4", 700)
            | ("float8", 701)
            | ("bpchar", 1042)
            | ("varchar", 1043)
            | ("date", 1082)
            | ("time", 1083)
            | ("timestamp", 1114)
            | ("timestamptz", 1184)
            | ("timetz", 1266)
            | ("numeric", 1700)
            | ("uuid", 2950)
            | ("jsonb", 3802)
    )
}
pub fn validate_cells(cells: &[TypedCell]) -> Result<(), WorkbenchError> {
    if cells.len() > 512 {
        return Err(WorkbenchError::Limit);
    }
    for cell in cells {
        match cell {
            TypedCell::Decimal { value, scale } => {
                validate_decimal(value, *scale)?;
            }
            TypedCell::I64 { value } => {
                if value
                    .parse::<i64>()
                    .ok()
                    .is_none_or(|n| n.to_string() != *value)
                {
                    return Err(WorkbenchError::InvalidInput);
                }
            }
            TypedCell::U64 { value } => {
                if value
                    .parse::<u64>()
                    .ok()
                    .is_none_or(|n| n.to_string() != *value)
                {
                    return Err(WorkbenchError::InvalidInput);
                }
            }
            TypedCell::Text { value } | TypedCell::Temporal { value, .. }
                if value.len() > 64 * 1024 =>
            {
                return Err(WorkbenchError::Limit)
            }
            TypedCell::Bytes { base64 } => {
                if base64.len() > 88 * 1024
                    || base64::engine::general_purpose::STANDARD
                        .decode(base64)
                        .map_err(|_| WorkbenchError::InvalidInput)?
                        .len()
                        > 64 * 1024
                {
                    return Err(WorkbenchError::Limit);
                }
            }
            TypedCell::Float { value } => {
                if !matches!(value.as_str(), "NaN" | "Infinity" | "-Infinity")
                    && !value.parse::<f64>().is_ok_and(|n| n.is_finite())
                {
                    return Err(WorkbenchError::InvalidInput);
                }
            }
            TypedCell::Unsupported { type_name, reason }
                if type_name.len() > 1024 || reason.len() > 1024 =>
            {
                return Err(WorkbenchError::Limit)
            }
            TypedCell::Large {
                preview, type_name, ..
            } if preview.len() > 4096 || type_name.len() > 1024 => {
                return Err(WorkbenchError::Limit)
            }
            _ => {}
        }
    }
    encoded_size(&cells, 256 * 1024)?;
    Ok(())
}
pub fn validate_decimal(value: &str, scale: Option<u32>) -> Result<(), WorkbenchError> {
    if value.is_empty() || value.len() > 64 * 1024 {
        return Err(WorkbenchError::InvalidInput);
    }
    let body = value.strip_prefix('-').unwrap_or(value);
    let mut parts = body.split('.');
    let integer = parts.next().unwrap_or("");
    let fraction = parts.next();
    if integer.is_empty()
        || !integer.bytes().all(|byte| byte.is_ascii_digit())
        || parts.next().is_some()
        || fraction
            .is_some_and(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
        || scale.is_some_and(|scale| scale as usize != fraction.map_or(0, str::len))
    {
        return Err(WorkbenchError::InvalidInput);
    }
    Ok(())
}
pub(crate) fn cell_heap(value: &TypedCell) -> usize {
    match value {
        TypedCell::I64 { value }
        | TypedCell::U64 { value }
        | TypedCell::Decimal { value, .. }
        | TypedCell::Float { value }
        | TypedCell::Text { value } => value.capacity(),
        TypedCell::Bytes { base64 } => base64.capacity(),
        TypedCell::Temporal {
            value,
            type_name,
            offset,
            ..
        } => value.capacity() + type_name.capacity() + offset.as_ref().map_or(0, String::capacity),
        TypedCell::Unsupported { type_name, reason } => type_name.capacity() + reason.capacity(),
        TypedCell::Large {
            type_name, preview, ..
        } => type_name.capacity() + preview.capacity(),
        _ => 0,
    }
}
pub(crate) fn cells_memory(cells: &[TypedCell], capacity: usize) -> usize {
    capacity
        .saturating_mul(std::mem::size_of::<TypedCell>())
        .saturating_add(cells.iter().map(cell_heap).sum::<usize>())
}
pub(crate) fn columns_memory(columns: &[Column], capacity: usize) -> usize {
    capacity
        .saturating_mul(std::mem::size_of::<Column>())
        .saturating_add(
            columns
                .iter()
                .map(|column| {
                    column.name.capacity()
                        + column.type_name.capacity()
                        + column.readonly_reason.as_ref().map_or(0, String::capacity)
                })
                .sum::<usize>(),
        )
}
pub(crate) fn rows_memory(rows: &[Row], capacity: usize) -> usize {
    capacity
        .saturating_mul(std::mem::size_of::<Row>())
        .saturating_add(
            rows.iter()
                .map(|row| cells_memory(&row.cells, row.cells.capacity()))
                .sum::<usize>(),
        )
}
pub fn exact_key_cell(cell: &TypedCell) -> bool {
    matches!(
        cell,
        TypedCell::Bool { .. }
            | TypedCell::I64 { .. }
            | TypedCell::U64 { .. }
            | TypedCell::Decimal { .. }
            | TypedCell::Text { .. }
            | TypedCell::Bytes { .. }
    ) && validate_cells(std::slice::from_ref(cell)).is_ok()
}
pub fn quote_identifier(engine: Engine, name: &str) -> Result<String, WorkbenchError> {
    if name.is_empty() || name.len() > 1024 || name.contains('\0') {
        return Err(WorkbenchError::InvalidInput);
    }
    let quote = if matches!(engine, Engine::Mysql | Engine::Mariadb) {
        // This generated path has no certificate for mode-sensitive backslash
        // identifiers; user SQL still follows its explicitly observed dialect.
        if name.contains('\\') {
            return Err(WorkbenchError::Unsupported);
        }
        '`'
    } else {
        '"'
    };
    Ok(format!(
        "{quote}{}{quote}",
        name.replace(quote, &format!("{quote}{quote}"))
    ))
}
pub fn placeholder(engine: Engine, ordinal: usize) -> String {
    match engine {
        Engine::Postgresql => format!("${ordinal}"),
        Engine::Sqlite => format!("?{ordinal}"),
        _ => "?".into(),
    }
}
pub(crate) fn text(value: &TypedCell) -> Result<&str, WorkbenchError> {
    match value {
        TypedCell::Text { value }
        | TypedCell::I64 { value }
        | TypedCell::U64 { value }
        | TypedCell::Decimal { value, .. }
        | TypedCell::Temporal { value, .. } => Ok(value),
        _ => Err(WorkbenchError::Unsupported),
    }
}
pub(crate) fn integer(value: &TypedCell) -> Result<i64, WorkbenchError> {
    text(value)?
        .parse()
        .map_err(|_| WorkbenchError::Unsupported)
}
pub(crate) fn boolean(value: &TypedCell) -> Result<bool, WorkbenchError> {
    match value {
        TypedCell::Bool { value } => Ok(*value),
        _ => match text(value)? {
            "1" | "YES" | "true" | "t" => Ok(true),
            "0" | "NO" | "false" | "f" => Ok(false),
            _ => Err(WorkbenchError::Unsupported),
        },
    }
}
pub(crate) fn optional_text(value: &TypedCell) -> Result<Option<&str>, WorkbenchError> {
    if matches!(value, TypedCell::Null) {
        Ok(None)
    } else {
        text(value).map(Some)
    }
}

pub(crate) struct BoundedRows {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<TypedCell>>,
    bytes: usize,
    max_rows: usize,
    max_bytes: usize,
    _memory: MemoryReservation,
    budget: Arc<NativeBudget>,
    row_memory: Vec<MemoryReservation>,
    prefix: bool,
    pub(crate) truncated: bool,
}
impl BoundedRows {
    pub(crate) fn new(
        budget: &Arc<NativeBudget>,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<Self, WorkbenchError> {
        if max_rows == 0 || max_rows > 1024 || max_bytes > 512 * 1024 {
            return Err(WorkbenchError::Limit);
        }
        Ok(Self {
            columns: Vec::new(),
            rows: Vec::new(),
            bytes: 0,
            max_rows,
            max_bytes,
            _memory: budget.reserve(4096)?,
            budget: budget.clone(),
            row_memory: Vec::new(),
            prefix: false,
            truncated: false,
        })
    }
    /// Generated browse reads may drain their finite LIMIT, while retaining
    /// only one contiguous prefix. Once full, later smaller rows cannot fill
    /// holes that would make a keyset cursor skip an unreturned row.
    pub(crate) fn prefix(
        budget: &Arc<NativeBudget>,
        max_rows: usize,
        max_bytes: usize,
        page_header_bytes: usize,
    ) -> Result<Self, WorkbenchError> {
        if page_header_bytes >= max_bytes {
            return Err(WorkbenchError::Limit);
        }
        let mut rows = Self::new(budget, max_rows, max_bytes)?;
        rows.prefix = true;
        rows.bytes = page_header_bytes;
        Ok(rows)
    }
}
#[async_trait]
impl RowSink for BoundedRows {
    async fn columns(&mut self, columns: Vec<Column>) -> Result<(), WorkbenchError> {
        if columns.len() > 512 {
            return Err(WorkbenchError::Limit);
        }
        let memory = self.budget.reserve(
            columns_memory(&columns, columns.capacity())
                + encoded_size(&columns, 256 * 1024)? * 2
                + 4096,
        )?;
        self._memory = memory;
        self.columns = columns;
        Ok(())
    }
    async fn row(&mut self, cells: Vec<TypedCell>) -> Result<(), WorkbenchError> {
        validate_cells(&cells)?;
        let bytes = encoded_size(
            &cells,
            if self.prefix {
                256 * 1024
            } else {
                self.max_bytes
            },
        )?;
        if self.prefix && self.truncated {
            return Ok(());
        }
        let charged = if self.prefix { bytes + 64 } else { bytes };
        if self.rows.len() >= self.max_rows
            || self
                .bytes
                .checked_add(charged)
                .is_none_or(|n| n > self.max_bytes)
        {
            if self.prefix && !self.rows.is_empty() {
                self.truncated = true;
                return Ok(());
            }
            return Err(WorkbenchError::Limit);
        }
        self.bytes += charged;
        self.row_memory.push(
            self.budget
                .reserve(cells_memory(&cells, cells.capacity()) + bytes * 2 + 256)?,
        );
        self.rows.push(cells);
        Ok(())
    }
}
/// Only generated catalog reads may temporarily raise GROUP_CONCAT's bound.
/// Both success and failure restore the exact observed setting; inability to
/// restore is a physical-connection failure, never a reusable session.
pub(crate) async fn read_catalog(
    connection: &mut dyn DriverConnection,
    budget: &Arc<NativeBudget>,
    sql: &str,
    parameters: &[TypedCell],
    max_rows: usize,
) -> Result<BoundedRows, WorkbenchError> {
    if !matches!(connection.facts().engine, Engine::Mysql | Engine::Mariadb) {
        return read(connection, budget, sql, parameters, max_rows).await;
    }
    let observed = read_unscoped(
        connection,
        budget,
        "SELECT @@session.group_concat_max_len",
        &[],
        1,
    )
    .await?;
    let previous = text(
        observed
            .rows
            .first()
            .and_then(|row| row.first())
            .ok_or(WorkbenchError::Unsupported)?,
    )?
    .parse::<u64>()
    .map_err(|_| WorkbenchError::Unsupported)?;
    read_unscoped(
        connection,
        budget,
        "SET SESSION group_concat_max_len=1048576",
        &[],
        1,
    )
    .await?;
    let result = read_unscoped(connection, budget, sql, parameters, max_rows).await;
    // Interpolate only the canonical integer observed on this held socket.
    let restore = format!("SET SESSION group_concat_max_len={previous}");
    if read_unscoped(connection, budget, &restore, &[], 1)
        .await
        .is_err()
    {
        return Err(WorkbenchError::Io);
    }
    result
}
pub(crate) async fn read(
    connection: &mut dyn DriverConnection,
    budget: &Arc<NativeBudget>,
    sql: &str,
    parameters: &[TypedCell],
    max_rows: usize,
) -> Result<BoundedRows, WorkbenchError> {
    if connection.facts().engine == Engine::Postgresql {
        // Generated catalog SQL must not resolve operator/type/function names
        // through a caller-controlled search_path. Restore exact prior settings
        // before returning the physical connection to any user-SQL consumer.
        let observed = read_unscoped(
            connection,
            budget,
            "SELECT pg_catalog.current_setting('search_path')",
            &[],
            1,
        )
        .await?;
        let previous = text(&observed.rows[0][0])?.to_owned();
        read_unscoped(
            connection,
            budget,
            "SELECT pg_catalog.set_config('search_path','pg_catalog',false)",
            &[],
            1,
        )
        .await?;
        let result = read_unscoped(connection, budget, sql, parameters, max_rows).await;
        // If either query fails, session.rs retires the physical connection.
        read_unscoped(
            connection,
            budget,
            "SELECT pg_catalog.set_config('search_path',$1::pg_catalog.text,false)",
            &[TypedCell::Text { value: previous }],
            1,
        )
        .await?;
        return result;
    }
    read_unscoped(connection, budget, sql, parameters, max_rows).await
}
pub(crate) async fn read_unscoped(
    connection: &mut dyn DriverConnection,
    budget: &Arc<NativeBudget>,
    sql: &str,
    parameters: &[TypedCell],
    max_rows: usize,
) -> Result<BoundedRows, WorkbenchError> {
    let mut sink = BoundedRows::new(budget, max_rows, 512 * 1024)?;
    let summary = connection
        .execute_bound(
            sql,
            parameters,
            QueryLimits {
                max_rows,
                max_bytes: 512 * 1024,
                timeout: Duration::from_secs(30),
            },
            &mut sink,
        )
        .await?;
    if summary.capped || summary.rows_seen > max_rows as u64 {
        return Err(WorkbenchError::Limit);
    }
    Ok(sink)
}
