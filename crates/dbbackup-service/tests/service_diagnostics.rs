use dbbackup_catalog::OperationalStore;
use dbbackup_service::diagnostics::collect_service_diagnostics;
use tempfile::TempDir;

#[test]
fn diagnostics_cover_all_local_operational_health_domains_without_mutating_store() {
    let temp = TempDir::new().expect("temp directory");
    let store =
        OperationalStore::open(temp.path().join("operational.db")).expect("open operational store");

    let first = collect_service_diagnostics(&store, 10_000).expect("first diagnostics");
    let second = collect_service_diagnostics(&store, 10_001).expect("second diagnostics");

    assert_eq!(first.schema().user_version(), 4);
    assert_eq!(first.schema().application_table_count(), 67);
    assert_eq!(first.schema().journal_mode(), "wal");
    assert!(first.schema().foreign_keys());
    assert_eq!(first.queue().total(), 0);
    assert_eq!(first.leases().active(), 0);
    assert_eq!(first.leases().stale(), 0);
    assert_eq!(first.permits().active_allocations(), 0);
    assert_eq!(first.spool().active_reservations(), 0);
    assert_eq!(first.tools().trusted(), 0);
    assert_eq!(first.destinations().active(), 0);
    assert_eq!(first.notifications().pending(), 0);
    assert_eq!(first.attention().open(), 0);
    assert_eq!(first.activity().retained(), 0);
    assert_eq!(second.generated_at_ms(), 10_001);
    assert_eq!(first.queue(), second.queue());
}
