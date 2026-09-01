use dbbackup_catalog::{
    apply_operational_store_v2, operational_schema_v2_checksum, rusqlite::Connection,
    MigrationError,
};

#[test]
fn canonical_v2_schema_is_strict_complete_and_journaled() {
    let conn = Connection::open_in_memory().expect("open in-memory operational store");

    let outcome = apply_operational_store_v2(&conn).expect("apply canonical v2 migration");
    assert!(outcome.applied);
    assert_eq!(outcome.version, 2);

    let (table_count, strict_count): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), SUM(strict)
             FROM pragma_table_list
             WHERE schema = 'main'
               AND type = 'table'
               AND name NOT LIKE 'sqlite_%'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("count strict application tables");
    assert_eq!(table_count, 53);
    assert_eq!(strict_count, 53);

    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("run integrity_check");
    assert_eq!(integrity, "ok");

    let mut foreign_key_check = conn
        .prepare("PRAGMA foreign_key_check")
        .expect("prepare foreign_key_check");
    assert!(
        foreign_key_check
            .query([])
            .expect("run foreign_key_check")
            .next()
            .expect("read foreign_key_check")
            .is_none(),
        "fresh operational store must have no FK violations"
    );

    let (journal_count, journal_checksum): (i64, String) = conn
        .query_row(
            "SELECT COUNT(*), MIN(checksum_blake3)
             FROM schema_migrations
             WHERE version = 2",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read authoritative migration journal");
    assert_eq!(journal_count, 1);
    assert_eq!(journal_checksum, operational_schema_v2_checksum());

    let user_version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read user_version");
    assert_eq!(user_version, 2);

    let foreign_keys: i64 = conn
        .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
        .expect("read foreign_keys pragma");
    let synchronous: i64 = conn
        .query_row("PRAGMA synchronous", [], |row| row.get(0))
        .expect("read synchronous pragma");
    let busy_timeout: i64 = conn
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .expect("read busy_timeout pragma");
    assert_eq!(foreign_keys, 1);
    assert_eq!(synchronous, 2, "FULL synchronous must remain enabled");
    assert_eq!(busy_timeout, 5000);
}

#[test]
fn migration_is_idempotent_and_rejects_checksum_or_version_drift() {
    let conn = Connection::open_in_memory().expect("open in-memory operational store");
    assert!(
        apply_operational_store_v2(&conn)
            .expect("first migration")
            .applied
    );
    assert!(
        !apply_operational_store_v2(&conn)
            .expect("idempotent migration")
            .applied
    );

    conn.execute(
        "UPDATE schema_migrations
         SET checksum_blake3 = ?1
         WHERE version = 2",
        ["0".repeat(64)],
    )
    .expect("corrupt migration journal checksum");
    let checksum_error =
        apply_operational_store_v2(&conn).expect_err("checksum drift must block write mode");
    assert_eq!(
        checksum_error.code(),
        "OPERATIONAL_MIGRATION_CHECKSUM_DRIFT"
    );
    assert!(matches!(
        checksum_error,
        MigrationError::ChecksumDrift { .. }
    ));

    conn.execute(
        "UPDATE schema_migrations
         SET checksum_blake3 = ?1
         WHERE version = 2",
        [operational_schema_v2_checksum()],
    )
    .expect("restore migration checksum");
    conn.pragma_update(None, "user_version", 1)
        .expect("corrupt user_version");
    let version_error =
        apply_operational_store_v2(&conn).expect_err("version drift must block write mode");
    assert_eq!(version_error.code(), "OPERATIONAL_MIGRATION_VERSION_DRIFT");
    assert!(matches!(
        version_error,
        MigrationError::VersionDrift {
            expected: 2,
            actual: 1
        }
    ));
}

#[test]
fn bootstrap_rejects_unjournaled_nonzero_schema_version() {
    let conn = Connection::open_in_memory().expect("open in-memory operational store");
    conn.pragma_update(None, "user_version", 99)
        .expect("write future schema marker");

    let error = apply_operational_store_v2(&conn)
        .expect_err("an unjournaled future schema must not be overwritten");
    assert_eq!(error.code(), "OPERATIONAL_MIGRATION_VERSION_DRIFT");
    assert!(matches!(
        error,
        MigrationError::VersionDrift {
            expected: 2,
            actual: 99
        }
    ));
}

#[test]
fn failed_ddl_rolls_back_every_new_table_and_journal_change() {
    let conn = Connection::open_in_memory().expect("open in-memory operational store");
    conn.execute_batch("CREATE TABLE destinations(sentinel INTEGER) STRICT;")
        .expect("create deterministic schema collision");

    let error =
        apply_operational_store_v2(&conn).expect_err("DDL collision must fail the migration");
    assert_eq!(error.code(), "OPERATIONAL_MIGRATION_SQLITE");

    let migration_table_exists: bool = conn
        .query_row(
            "SELECT EXISTS(
                 SELECT 1
                 FROM sqlite_schema
                 WHERE type = 'table' AND name = 'schema_migrations'
             )",
            [],
            |row| row.get(0),
        )
        .expect("inspect rolled-back schema");
    assert!(!migration_table_exists);

    let sentinel_sql: String = conn
        .query_row(
            "SELECT sql
             FROM sqlite_schema
             WHERE type = 'table' AND name = 'destinations'",
            [],
            |row| row.get(0),
        )
        .expect("pre-existing collision table must survive");
    assert!(sentinel_sql.contains("sentinel"));
    let user_version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read rolled-back user_version");
    assert_eq!(user_version, 0);
}
