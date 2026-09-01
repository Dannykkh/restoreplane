use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};

use super::v2::{MigrationError, MigrationOutcome};

pub const OPERATIONAL_SCHEMA_V4: &str =
    include_str!("../../../../docs/plan/connected-backup-platform-v2/schema-v4-backup-scopes.sql");

pub const WHOLE_DATABASE_SELECTOR_JSON: &str = r#"{"mode":"whole_database","objects":[]}"#;
pub const WHOLE_DATABASE_SELECTOR_HASH_BLAKE3: &str =
    "6ce25010c90ed719a256a023e91191516ac5ece6007e7cd786a599f30b156ccb";

const VERSION: i64 = 4;
const NAME: &str = "operational_store_v4_backup_scopes";
const USER_VERSION_STATEMENT: &str = "PRAGMA user_version = 4;";

pub fn operational_schema_v4_checksum() -> String {
    blake3::hash(OPERATIONAL_SCHEMA_V4.as_bytes())
        .to_hex()
        .to_string()
}

pub fn apply_operational_store_v4(conn: &Connection) -> Result<MigrationOutcome, MigrationError> {
    let checksum = operational_schema_v4_checksum();
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
    if user_version != 3 {
        return Err(MigrationError::VersionDrift {
            expected: 3,
            actual: user_version,
        });
    }

    let body = migration_body()?;
    let started = Instant::now();
    if let Err(source) = conn.execute_batch(body) {
        rollback_open_transaction(conn);
        return Err(MigrationError::Sqlite(source));
    }
    if let Err(source) = backfill_v3_backup_scopes(conn) {
        rollback_open_transaction(conn);
        return Err(MigrationError::Sqlite(source));
    }

    let applied_at_ms = unix_time_ms()?;
    let execution_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    if let Err(source) = conn.execute(
        "INSERT INTO schema_migrations(
             version, name, checksum_blake3, applied_at_ms, app_version, execution_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            VERSION,
            NAME,
            checksum,
            applied_at_ms,
            env!("CARGO_PKG_VERSION"),
            execution_ms
        ],
    ) {
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

fn backfill_v3_backup_scopes(conn: &Connection) -> rusqlite::Result<()> {
    let selector_hash = blake3::hash(WHOLE_DATABASE_SELECTOR_JSON.as_bytes())
        .to_hex()
        .to_string();

    let targets = {
        let mut statement = conn.prepare(
            "SELECT id, source_fingerprint, selected_at_ms, created_at_ms, updated_at_ms
             FROM database_targets
             ORDER BY id",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    for (target_id, source_fingerprint, selected_at_ms, created_at_ms, updated_at_ms) in targets {
        conn.execute(
            "INSERT INTO backup_scopes(
                 id, database_target_id, display_name, normalized_name, mode, state,
                 selector_json, selector_hash_blake3, source_fingerprint,
                 database_source_fingerprint,
                 resolved_object_count, last_resolved_at_ms, created_at_ms, updated_at_ms
             ) VALUES (?1, ?1, 'Whole database', 'whole database', 'whole_database', 'active',
                       ?2, ?3, ?4, ?4, 0, ?5, ?6, ?7)",
            params![
                target_id,
                WHOLE_DATABASE_SELECTOR_JSON,
                selector_hash,
                source_fingerprint,
                selected_at_ms,
                created_at_ms,
                updated_at_ms
            ],
        )?;
    }

    conn.execute(
        "INSERT INTO plan_revision_scopes(
             plan_revision_id, database_target_id, backup_scope_id,
             scope_mode, source_fingerprint, resolved_object_count,
             database_source_fingerprint,
             scope_snapshot_json, scope_snapshot_hash_blake3, created_at_ms
         )
         SELECT target.plan_revision_id, target.database_target_id, scope.id,
                scope.mode, scope.source_fingerprint, scope.resolved_object_count,
                scope.database_source_fingerprint,
                scope.selector_json, scope.selector_hash_blake3, target.created_at_ms
         FROM plan_revision_targets AS target
         JOIN backup_scopes AS scope
           ON scope.id = target.database_target_id
          AND scope.database_target_id = target.database_target_id",
        [],
    )?;

    conn.execute(
        "INSERT INTO backup_run_scopes(
             backup_run_id, database_target_id, backup_scope_id,
             scope_mode, source_fingerprint, resolved_object_count,
             database_source_fingerprint,
             scope_snapshot_json, scope_snapshot_hash_blake3, created_at_ms
         )
         SELECT run.id, run.database_target_id, scope.backup_scope_id,
                scope.scope_mode, scope.source_fingerprint, scope.resolved_object_count,
                scope.database_source_fingerprint,
                scope.scope_snapshot_json, scope.scope_snapshot_hash_blake3, run.created_at_ms
         FROM backup_runs AS run
         JOIN plan_revision_scopes AS scope
           ON scope.plan_revision_id = run.plan_revision_id
          AND scope.database_target_id = run.database_target_id",
        [],
    )?;
    Ok(())
}

fn applied_checksum(conn: &Connection) -> Result<Option<String>, MigrationError> {
    let journal_exists: bool = conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM sqlite_schema
             WHERE type = 'table' AND name = 'schema_migrations'
         )",
        [],
        |row| row.get(0),
    )?;
    if !journal_exists {
        return Ok(None);
    }
    conn.query_row(
        "SELECT checksum_blake3 FROM schema_migrations WHERE version = ?1",
        [VERSION],
        |row| row.get(0),
    )
    .optional()
    .map_err(MigrationError::from)
}

fn migration_body() -> Result<&'static str, MigrationError> {
    let user_version_offset = OPERATIONAL_SCHEMA_V4
        .rfind(USER_VERSION_STATEMENT)
        .ok_or(MigrationError::InvalidTemplate)?;
    let tail = &OPERATIONAL_SCHEMA_V4[user_version_offset + USER_VERSION_STATEMENT.len()..];
    if !tail.contains("COMMIT;") {
        return Err(MigrationError::InvalidTemplate);
    }
    Ok(&OPERATIONAL_SCHEMA_V4[..user_version_offset])
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

#[cfg(test)]
mod tests {
    use super::WHOLE_DATABASE_SELECTOR_JSON;

    #[test]
    fn reports_canonical_whole_database_selector_hash() {
        let actual = blake3::hash(WHOLE_DATABASE_SELECTOR_JSON.as_bytes())
            .to_hex()
            .to_string();
        assert_eq!(actual, super::WHOLE_DATABASE_SELECTOR_HASH_BLAKE3);
        assert!(super::OPERATIONAL_SCHEMA_V4.contains(super::WHOLE_DATABASE_SELECTOR_HASH_BLAKE3));
    }
}
