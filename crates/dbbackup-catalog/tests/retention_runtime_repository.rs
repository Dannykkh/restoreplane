use dbbackup_catalog::{OperationalStore, RetentionMaintenanceSpec};
use rusqlite::Connection;
use tempfile::TempDir;

#[test]
fn automatic_retention_reconciliation_is_a_bounded_noop_without_candidates() {
    let temp_dir = TempDir::new().expect("temp dir");
    let database_path = temp_dir.path().join("operational.sqlite3");
    let store = OperationalStore::open(database_path.clone()).expect("open operational store");

    let report = store
        .retention_runtime()
        .reconcile(RetentionMaintenanceSpec::new(1_000, 10_000, 64))
        .expect("reconcile empty catalog");

    assert_eq!(report.candidates_evaluated(), 0);
    assert_eq!(report.tombstones_staged(), 0);
    assert_eq!(report.deletes_enqueued(), 0);
    assert_eq!(report.failures(), 0);

    let connection = Connection::open(database_path).expect("open heartbeat reader");
    let (value_text, updated_at_ms): (String, i64) = connection
        .query_row(
            "SELECT value_text, updated_at_ms
             FROM app_meta
             WHERE key = 'retention.runtime.last_scan.v1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("durable zero-candidate scan heartbeat");
    assert_eq!(value_text, r#"{"schema_version":1}"#);
    assert_eq!(updated_at_ms, 1_000);
}
