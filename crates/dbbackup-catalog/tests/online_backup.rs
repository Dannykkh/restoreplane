use std::thread;

use dbbackup_catalog::{
    ActivityEventSpec, ControlBackupSpec, OperationalStore, OPERATIONAL_SCHEMA_V2,
};
use tempfile::TempDir;

#[test]
fn live_wal_online_backup_and_separate_restore_rehearsal_are_valid() {
    let fixture = Fixture::new();
    let activity = fixture.store.activity();
    for batch_no in 0..10 {
        let events = (0..100)
            .map(|index| {
                ActivityEventSpec::info(
                    format!("seed-{batch_no}-{index}"),
                    1_000 + batch_no,
                    "SEED",
                    "test",
                    "online-backup",
                    "x".repeat(512),
                )
            })
            .collect();
        activity.append_batch(events).expect("seed WAL pages");
    }

    let concurrent_store = fixture.store.clone();
    let writer = thread::spawn(move || {
        for batch_no in 0..5 {
            let events = (0..20)
                .map(|index| {
                    ActivityEventSpec::info(
                        format!("concurrent-{batch_no}-{index}"),
                        2_000 + batch_no,
                        "CONCURRENT_WRITE",
                        "test",
                        "online-backup",
                        "write while backup reader is active",
                    )
                })
                .collect();
            concurrent_store
                .activity()
                .append_batch(events)
                .expect("concurrent activity append");
        }
    });

    let backup_path = fixture.temp_dir.path().join("control-backup.db");
    let rehearsal_path = fixture.temp_dir.path().join("control-rehearsal.db");
    let report = fixture
        .store
        .control_store()
        .backup_and_rehearse(ControlBackupSpec::new(
            "control-backup-01",
            backup_path.clone(),
            rehearsal_path.clone(),
            3_000,
        ))
        .expect("online backup and rehearsal");
    writer.join().expect("concurrent writer");

    assert_eq!(report.integrity_result(), "passed");
    assert_eq!(report.rehearsal_result(), "passed");
    assert_eq!(report.schema_version(), 4);
    assert_eq!(report.application_table_count(), 67);
    assert_eq!(report.checksum_blake3().len(), 64);
    assert!(backup_path.is_file());
    assert!(rehearsal_path.is_file());
    assert_ne!(backup_path, rehearsal_path);
    assert_eq!(
        fixture
            .store
            .control_store()
            .backup_status("control-backup-01")
            .expect("backup status"),
        "succeeded"
    );
    assert!(
        OPERATIONAL_SCHEMA_V2.contains("CREATE TABLE control_store_backups"),
        "rehearsed schema is the canonical embedded schema"
    );
}

#[test]
fn rehearsal_io_failure_records_no_success_and_leaves_production_healthy() {
    let fixture = Fixture::new();
    let backup_path = fixture.temp_dir.path().join("failed-rehearsal-backup.db");
    let rehearsal_path = fixture
        .temp_dir
        .path()
        .join("missing-parent")
        .join("rehearsal.db");

    let error = fixture
        .store
        .control_store()
        .backup_and_rehearse(ControlBackupSpec::new(
            "control-backup-failed",
            backup_path,
            rehearsal_path,
            4_000,
        ))
        .expect_err("missing rehearsal parent must fail");
    assert_eq!(error.code(), "CONTROL_STORE_BACKUP_IO");
    assert_eq!(
        fixture
            .store
            .control_store()
            .backup_status("control-backup-failed")
            .expect("failed status"),
        "failed"
    );
    assert_eq!(
        fixture
            .store
            .control_store()
            .successful_backup_count()
            .expect("successful count"),
        0
    );
    let diagnostics = fixture
        .store
        .writer_diagnostics()
        .expect("source diagnostics");
    assert_eq!(diagnostics.user_version, 4);
    let reader = fixture
        .store
        .open_reader()
        .expect("source remains readable");
    assert_eq!(
        reader
            .diagnostics()
            .expect("source integrity context")
            .foreign_keys,
        1
    );
}

struct Fixture {
    temp_dir: TempDir,
    store: OperationalStore,
}

impl Fixture {
    fn new() -> Self {
        let temp_dir = TempDir::new().expect("temp directory");
        let store = OperationalStore::open(temp_dir.path().join("operational.db"))
            .expect("open operational store");
        Self { temp_dir, store }
    }
}
