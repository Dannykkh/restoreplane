use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use thiserror::Error;

pub const OPERATIONAL_SCHEMA_V2: &str =
    include_str!("../../../../docs/plan/connected-backup-platform-v2/schema-v2.sql");

const VERSION: i64 = 2;
const NAME: &str = "operational_store_v2";
const USER_VERSION_STATEMENT: &str = "PRAGMA user_version = 2;";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationOutcome {
    pub version: i64,
    pub applied: bool,
    pub checksum_blake3: String,
}

#[derive(Debug, Error)]
pub enum MigrationError {
    #[error("OPERATIONAL_MIGRATION_CHECKSUM_DRIFT")]
    ChecksumDrift { expected: String, actual: String },
    #[error("OPERATIONAL_MIGRATION_VERSION_DRIFT")]
    VersionDrift { expected: i64, actual: i64 },
    #[error("OPERATIONAL_MIGRATION_TEMPLATE_INVALID")]
    InvalidTemplate,
    #[error("OPERATIONAL_MIGRATION_CLOCK_INVALID")]
    InvalidClock,
    #[error("OPERATIONAL_MIGRATION_SQLITE")]
    Sqlite(#[source] rusqlite::Error),
}

impl MigrationError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::ChecksumDrift { .. } => "OPERATIONAL_MIGRATION_CHECKSUM_DRIFT",
            Self::VersionDrift { .. } => "OPERATIONAL_MIGRATION_VERSION_DRIFT",
            Self::InvalidTemplate => "OPERATIONAL_MIGRATION_TEMPLATE_INVALID",
            Self::InvalidClock => "OPERATIONAL_MIGRATION_CLOCK_INVALID",
            Self::Sqlite(_) => "OPERATIONAL_MIGRATION_SQLITE",
        }
    }
}

impl From<rusqlite::Error> for MigrationError {
    fn from(source: rusqlite::Error) -> Self {
        Self::Sqlite(source)
    }
}

pub fn operational_schema_v2_checksum() -> String {
    blake3::hash(OPERATIONAL_SCHEMA_V2.as_bytes())
        .to_hex()
        .to_string()
}

pub fn apply_operational_store_v2(conn: &Connection) -> Result<MigrationOutcome, MigrationError> {
    let checksum = operational_schema_v2_checksum();
    if let Some(applied_checksum) = applied_checksum(conn)? {
        if applied_checksum != checksum {
            return Err(MigrationError::ChecksumDrift {
                expected: applied_checksum,
                actual: checksum,
            });
        }
        let user_version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if user_version != VERSION {
            return Err(MigrationError::VersionDrift {
                expected: VERSION,
                actual: user_version,
            });
        }
        return Ok(MigrationOutcome {
            version: VERSION,
            applied: false,
            checksum_blake3: checksum,
        });
    }
    let user_version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if user_version != 0 {
        return Err(MigrationError::VersionDrift {
            expected: VERSION,
            actual: user_version,
        });
    }

    let body = migration_body()?;
    let started = Instant::now();
    if let Err(source) = conn.execute_batch(body) {
        rollback_open_transaction(conn);
        return Err(MigrationError::Sqlite(source));
    }

    let applied_at_ms = unix_time_ms()?;
    let execution_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    let journal_result = conn.execute(
        "INSERT INTO schema_migrations(
             version,
             name,
             checksum_blake3,
             applied_at_ms,
             app_version,
             execution_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            VERSION,
            NAME,
            checksum,
            applied_at_ms,
            env!("CARGO_PKG_VERSION"),
            execution_ms
        ],
    );
    if let Err(source) = journal_result {
        rollback_open_transaction(conn);
        return Err(MigrationError::Sqlite(source));
    }
    if let Err(source) = conn.pragma_update(None, "user_version", VERSION) {
        rollback_open_transaction(conn);
        return Err(MigrationError::Sqlite(source));
    }
    if let Err(source) = conn.execute_batch("COMMIT;") {
        rollback_open_transaction(conn);
        return Err(MigrationError::Sqlite(source));
    }

    Ok(MigrationOutcome {
        version: VERSION,
        applied: true,
        checksum_blake3: checksum,
    })
}

fn applied_checksum(conn: &Connection) -> Result<Option<String>, MigrationError> {
    let journal_exists: bool = conn.query_row(
        "SELECT EXISTS(
             SELECT 1
             FROM sqlite_schema
             WHERE type = 'table' AND name = 'schema_migrations'
         )",
        [],
        |row| row.get(0),
    )?;
    if !journal_exists {
        return Ok(None);
    }
    conn.query_row(
        "SELECT checksum_blake3
         FROM schema_migrations
         WHERE version = ?1",
        [VERSION],
        |row| row.get(0),
    )
    .optional()
    .map_err(MigrationError::from)
}

fn migration_body() -> Result<&'static str, MigrationError> {
    let user_version_offset = OPERATIONAL_SCHEMA_V2
        .rfind(USER_VERSION_STATEMENT)
        .ok_or(MigrationError::InvalidTemplate)?;
    let tail = &OPERATIONAL_SCHEMA_V2[user_version_offset + USER_VERSION_STATEMENT.len()..];
    if !tail.contains("COMMIT;") {
        return Err(MigrationError::InvalidTemplate);
    }
    Ok(&OPERATIONAL_SCHEMA_V2[..user_version_offset])
}

fn unix_time_ms() -> Result<i64, MigrationError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| MigrationError::InvalidClock)?
        .as_millis();
    Ok(i64::try_from(millis).unwrap_or(i64::MAX))
}

fn rollback_open_transaction(conn: &Connection) {
    if !conn.is_autocommit() {
        let _ = conn.execute_batch("ROLLBACK;");
    }
}
