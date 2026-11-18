use restoreplane_workbench::store::{ClientStore, UserRootPolicy};
use restoreplane_workbench::{IntegrityLevel, NativePrincipal, WorkbenchError};
struct Policy;
impl UserRootPolicy for Policy {
    fn ensure_private_root(&self, path: &std::path::Path) -> Result<(), WorkbenchError> {
        std::fs::create_dir_all(path).map_err(|_| WorkbenchError::Io)
    }
}
fn owner() -> NativePrincipal {
    NativePrincipal::new("a".repeat(64), 7, IntegrityLevel::Standard).unwrap()
}
fn open(path: &std::path::Path) -> ClientStore {
    ClientStore::open(path, &owner(), &Policy).unwrap()
}
fn revision(store: &ClientStore) -> i64 {
    store
        .read(|c| {
            c.query_row("SELECT storage_revision FROM journal_header", [], |r| {
                r.get(0)
            })
            .map_err(|_| WorkbenchError::StoreUnavailable)
        })
        .unwrap()
}
#[test]
fn clean_pair_revision_and_committed_business_errors_survive_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let store = open(directory.path());
    assert!(store.write_enabled());
    assert_eq!(revision(&store), 0);
    let result: Result<(), _> = store.write(|c| {
        c.execute("UPDATE journal_header SET updated_at_ms=1", [])
            .unwrap();
        Err(WorkbenchError::OutcomeUnknown)
    });
    assert_eq!(result, Err(WorkbenchError::OutcomeUnknown));
    assert!(store.write_enabled());
    assert_eq!(revision(&store), 1);
    drop(store);
    let reopened = open(directory.path());
    assert!(reopened.write_enabled());
    assert_eq!(revision(&reopened), 1);
}
#[test]
fn independently_restored_valid_database_is_readable_but_never_automatically_resealed() {
    let directory = tempfile::tempdir().unwrap();
    let store = open(directory.path());
    drop(store);
    let database = directory.path().join("client-state.sqlite3");
    let historical = directory.path().join("historical.sqlite3");
    std::fs::copy(&database, &historical).unwrap();
    let store = open(directory.path());
    store
        .write(|c| {
            c.execute("UPDATE journal_header SET updated_at_ms=7", [])
                .unwrap();
            Ok(())
        })
        .unwrap();
    drop(store);
    let anchor = directory.path().join("client-state.anchor");
    let sealed = std::fs::read(&anchor).unwrap();
    std::fs::copy(historical, &database).unwrap();
    let restored = open(directory.path());
    assert!(!restored.write_enabled());
    assert_eq!(revision(&restored), 0);
    assert_eq!(
        restored.write(|_| Ok(())),
        Err(WorkbenchError::StoreUnavailable)
    );
    drop(restored);
    assert_eq!(std::fs::read(&anchor).unwrap(), sealed);
}
#[test]
fn missing_database_or_anchor_is_never_silently_recreated() {
    let directory = tempfile::tempdir().unwrap();
    drop(open(directory.path()));
    let database = directory.path().join("client-state.sqlite3");
    std::fs::remove_file(&database).unwrap();
    assert!(matches!(
        ClientStore::open(directory.path(), &owner(), &Policy),
        Err(WorkbenchError::StoreUnavailable)
    ));
    assert!(!database.exists());
    let other = tempfile::tempdir().unwrap();
    drop(open(other.path()));
    std::fs::remove_file(other.path().join("client-state.anchor")).unwrap();
    let disabled = open(other.path());
    assert!(!disabled.write_enabled());
    assert_eq!(revision(&disabled), 0);
    assert!(!other.path().join("client-state.anchor").exists());
}
#[test]
fn dirty_anchor_and_storage_failure_disable_writes_while_reads_survive() {
    let directory = tempfile::tempdir().unwrap();
    let store = open(directory.path());
    let result: Result<(), _> = store.write(|c| {
        c.execute("UPDATE journal_header SET updated_at_ms=9", [])
            .unwrap();
        Err(WorkbenchError::StoreUnavailable)
    });
    assert_eq!(result, Err(WorkbenchError::StoreUnavailable));
    assert!(!store.write_enabled());
    assert_eq!(revision(&store), 0);
    drop(store);
    let reopened = open(directory.path());
    assert!(!reopened.write_enabled());
    assert_eq!(revision(&reopened), 0);
}
#[test]
fn revision_overflow_refuses_before_invoking_mutation() {
    let directory = tempfile::tempdir().unwrap();
    drop(open(directory.path()));
    let database =
        rusqlite::Connection::open(directory.path().join("client-state.sqlite3")).unwrap();
    database
        .execute("UPDATE journal_header SET storage_revision=?1", [i64::MAX])
        .unwrap();
    drop(database);
    let anchor_path = directory.path().join("client-state.anchor");
    let mut anchor: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&anchor_path).unwrap()).unwrap();
    anchor["revision"] = i64::MAX.into();
    std::fs::write(anchor_path, serde_json::to_vec(&anchor).unwrap()).unwrap();
    let store = open(directory.path());
    let called = std::cell::Cell::new(false);
    assert_eq!(
        store.write(|_| {
            called.set(true);
            Ok(())
        }),
        Err(WorkbenchError::StoreUnavailable)
    );
    assert!(!called.get());
    assert!(!store.write_enabled());
    assert_eq!(revision(&store), i64::MAX);
}
