mod v2;
mod v3;
mod v4;

use rusqlite::Connection;

pub use v2::{
    apply_operational_store_v2, operational_schema_v2_checksum, MigrationError, MigrationOutcome,
    OPERATIONAL_SCHEMA_V2,
};
pub use v3::{apply_operational_store_v3, operational_schema_v3_checksum, OPERATIONAL_SCHEMA_V3};
pub use v4::{
    apply_operational_store_v4, operational_schema_v4_checksum, OPERATIONAL_SCHEMA_V4,
    WHOLE_DATABASE_SELECTOR_HASH_BLAKE3, WHOLE_DATABASE_SELECTOR_JSON,
};

pub fn apply_operational_store_migrations(
    conn: &Connection,
) -> Result<MigrationOutcome, MigrationError> {
    let user_version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    match user_version {
        0 => {
            apply_operational_store_v2(conn)?;
        }
        2..=4 => verify_v2_journal_checksum(conn)?,
        actual => {
            return Err(MigrationError::VersionDrift {
                expected: 4,
                actual,
            });
        }
    }
    let current_version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if current_version == 2 {
        apply_operational_store_v3(conn)?;
    } else {
        verify_v3_journal_checksum(conn)?;
    }
    apply_operational_store_v4(conn)
}

fn verify_v2_journal_checksum(conn: &Connection) -> Result<(), MigrationError> {
    let expected = operational_schema_v2_checksum();
    let actual = conn
        .query_row(
            "SELECT checksum_blake3 FROM schema_migrations WHERE version = 2",
            [],
            |row| row.get::<_, String>(0),
        )
        .map_err(MigrationError::from)?;
    if actual != expected {
        return Err(MigrationError::ChecksumDrift { expected, actual });
    }
    Ok(())
}

fn verify_v3_journal_checksum(conn: &Connection) -> Result<(), MigrationError> {
    let expected = operational_schema_v3_checksum();
    let actual = conn
        .query_row(
            "SELECT checksum_blake3 FROM schema_migrations WHERE version = 3",
            [],
            |row| row.get::<_, String>(0),
        )
        .map_err(MigrationError::from)?;
    if actual != expected {
        return Err(MigrationError::ChecksumDrift { expected, actual });
    }
    Ok(())
}
