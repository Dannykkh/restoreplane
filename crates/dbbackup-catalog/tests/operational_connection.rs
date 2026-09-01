use dbbackup_catalog::OperationalStore;
use tempfile::TempDir;

#[test]
fn operational_writer_applies_wal_full_and_v3_migrations() {
    let temp = TempDir::new().expect("temp dir");
    let db_path = temp.path().join("operational.sqlite");
    let store = OperationalStore::open(db_path).expect("open operational store");

    let diagnostics = store
        .writer_diagnostics()
        .expect("read writer diagnostics through transaction owner");
    assert_eq!(diagnostics.journal_mode, "wal");
    assert_eq!(diagnostics.synchronous, 2);
    assert_eq!(diagnostics.foreign_keys, 1);
    assert_eq!(diagnostics.busy_timeout_ms, 5000);
    assert_eq!(diagnostics.query_only, 0);
    assert_eq!(diagnostics.user_version, 4);
    assert_eq!(diagnostics.application_table_count, 67);
}

#[test]
fn operational_reader_is_bounded_query_only_and_foreign_key_aware() {
    let temp = TempDir::new().expect("temp dir");
    let db_path = temp.path().join("operational.sqlite");
    let store = OperationalStore::open(db_path).expect("open operational store");
    let reader = store.open_reader().expect("open operational reader");

    let diagnostics = reader.diagnostics().expect("read reader diagnostics");
    assert_eq!(diagnostics.journal_mode, "wal");
    assert_eq!(diagnostics.foreign_keys, 1);
    assert_eq!(diagnostics.busy_timeout_ms, 5000);
    assert_eq!(diagnostics.query_only, 1);
    assert_eq!(diagnostics.user_version, 4);
    assert_eq!(diagnostics.application_table_count, 67);
    assert!(reader.is_read_only().expect("check SQLite read-only flag"));
}
