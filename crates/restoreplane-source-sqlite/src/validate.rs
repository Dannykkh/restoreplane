use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use rusqlite::{types::ValueRef, Connection, OpenFlags};
use serde::{Deserialize, Serialize};

use crate::SqliteProviderError;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScalarOracle {
    query: String,
    expected: String,
}

impl ScalarOracle {
    pub fn new(
        query: impl Into<String>,
        expected: impl Into<String>,
    ) -> Result<Self, SqliteProviderError> {
        let query = query.into();
        let expected = expected.into();
        let normalized = query.trim().to_ascii_uppercase();
        if query.len() > 4_096
            || expected.len() > 4_096
            || !(normalized.starts_with("SELECT ") || normalized.starts_with("WITH "))
            || query.contains(['\0', '\r', '\n', ';'])
        {
            return Err(SqliteProviderError::OracleFailed(
                "oracle query must be one bounded read-only statement".to_owned(),
            ));
        }
        Ok(Self { query, expected })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationEvidence {
    integrity_ok: bool,
    foreign_keys_ok: bool,
    oracle_ok: Option<bool>,
    oracle_observed: Option<String>,
}

impl ValidationEvidence {
    pub const fn integrity_ok(&self) -> bool {
        self.integrity_ok
    }

    pub const fn foreign_keys_ok(&self) -> bool {
        self.foreign_keys_ok
    }

    pub const fn oracle_ok(&self) -> bool {
        matches!(self.oracle_ok, Some(true))
    }

    pub fn oracle_observed(&self) -> Option<&str> {
        self.oracle_observed.as_deref()
    }
}

pub fn validate_database(
    path: &Path,
    oracle: Option<&ScalarOracle>,
) -> Result<ValidationEvidence, SqliteProviderError> {
    validate_database_observed(path, oracle, || false)
}

pub fn validate_database_observed(
    path: &Path,
    oracle: Option<&ScalarOracle>,
    mut control: impl FnMut() -> bool + Send + 'static,
) -> Result<ValidationEvidence, SqliteProviderError> {
    if control() {
        return Err(SqliteProviderError::Cancelled);
    }
    let mut connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let interrupted = Arc::new(AtomicBool::new(false));
    let handler_interrupted = Arc::clone(&interrupted);
    let mut control = ProgressControl(AssertUnwindSafe(control));
    connection.progress_handler(
        1_000,
        Some(move || {
            let should_interrupt = control.call();
            if should_interrupt {
                handler_interrupted.store(true, Ordering::Release);
            }
            should_interrupt
        }),
    )?;
    let result = (|| {
        let mut integrity = connection.prepare("PRAGMA integrity_check")?;
        let messages = integrity
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if messages.len() != 1 || messages[0] != "ok" {
            return Err(SqliteProviderError::IntegrityFailed(messages.join("; ")));
        }
        let mut foreign_keys = connection.prepare("PRAGMA foreign_key_check")?;
        if foreign_keys.query([])?.next()?.is_some() {
            return Err(SqliteProviderError::ForeignKeyFailed);
        }
        let oracle_observed = oracle
            .map(|oracle| {
                connection
                    .query_row(&oracle.query, [], |row| value_to_string(row.get_ref(0)?))
                    .map_err(SqliteProviderError::from)
            })
            .transpose()?;
        let oracle_ok = match (oracle, oracle_observed.as_deref()) {
            (Some(oracle), Some(observed)) if observed == oracle.expected => Some(true),
            (Some(oracle), Some(observed)) => {
                return Err(SqliteProviderError::OracleFailed(format!(
                    "expected {:?}, observed {:?}",
                    oracle.expected, observed
                )))
            }
            (Some(_), None) => {
                return Err(SqliteProviderError::OracleFailed(
                    "oracle returned no value".to_owned(),
                ))
            }
            (None, _) => None,
        };
        Ok(ValidationEvidence {
            integrity_ok: true,
            foreign_keys_ok: true,
            oracle_ok,
            oracle_observed,
        })
    })();
    if interrupted.load(Ordering::Acquire) {
        Err(SqliteProviderError::Cancelled)
    } else {
        result
    }
}

struct ProgressControl<F>(AssertUnwindSafe<F>);

impl<F> ProgressControl<F>
where
    F: FnMut() -> bool,
{
    fn call(&mut self) -> bool {
        (self.0 .0)()
    }
}

fn value_to_string(value: ValueRef<'_>) -> rusqlite::Result<String> {
    Ok(match value {
        ValueRef::Null => "null".to_owned(),
        ValueRef::Integer(value) => value.to_string(),
        ValueRef::Real(value) => value.to_string(),
        ValueRef::Text(value) => String::from_utf8_lossy(value).into_owned(),
        ValueRef::Blob(value) => value.iter().map(|byte| format!("{byte:02x}")).collect(),
    })
}
