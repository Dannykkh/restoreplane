use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dbbackup_core::SupportStatus;
use dbbackup_pipeline::{BackupSource, DumpArtifact};
use dbbackup_source_sqlite::{
    online_backup_to_path, online_backup_to_path_observed, probe_sqlite_file, restore_create_new,
    restore_overwrite, restore_overwrite_with_replacer, sqlite_certification,
    validate_database_observed, AtomicReplacer, BackupCancellationToken, OnlineBackupOptions,
    ScalarOracle, SqliteProviderError, SqliteSource,
};
use rusqlite::{params, Connection};

fn create_fixture(path: &Path, journal_mode: &str, rows: usize) {
    let connection = Connection::open(path).expect("open fixture");
    connection
        .pragma_update(None, "journal_mode", journal_mode)
        .expect("set journal mode");
    connection
        .execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE parent(id INTEGER PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE child(
               id INTEGER PRIMARY KEY,
               parent_id INTEGER NOT NULL REFERENCES parent(id)
             );",
        )
        .expect("create schema");
    let transaction = connection.unchecked_transaction().expect("begin fixture");
    for index in 0..rows {
        transaction
            .execute(
                "INSERT INTO parent(id, value) VALUES (?1, ?2)",
                params![
                    index as i64 + 1,
                    format!("value-{index:05}-{}", "x".repeat(128))
                ],
            )
            .expect("insert parent");
        transaction
            .execute(
                "INSERT INTO child(id, parent_id) VALUES (?1, ?1)",
                [index as i64 + 1],
            )
            .expect("insert child");
    }
    transaction.commit().expect("commit fixture");
}

fn count_oracle(expected: usize) -> ScalarOracle {
    ScalarOracle::new("SELECT COUNT(*) FROM parent", expected.to_string())
        .expect("valid scalar oracle")
}

#[test]
fn integrity_validation_is_interruptible_during_sqlite_vm_execution() {
    let temp = tempfile::tempdir().expect("temp");
    let source = temp.path().join("validation-cancel.sqlite");
    create_fixture(&source, "DELETE", 5_000);
    let calls = Arc::new(AtomicUsize::new(0));
    let callback_calls = Arc::clone(&calls);

    let error = validate_database_observed(&source, None, move || {
        callback_calls.fetch_add(1, Ordering::SeqCst) >= 1
    })
    .expect_err("progress callback must interrupt integrity validation");

    assert_eq!(error.code(), "SQLITE_BACKUP_CANCELLED");
    assert!(calls.load(Ordering::SeqCst) >= 2);
}

#[test]
fn wal_active_writer_backup_and_create_new_restore_pass_integrity_fk_and_oracle() {
    let temp = tempfile::tempdir().expect("temp");
    let source = temp.path().join("source.sqlite");
    let artifact = temp.path().join("artifact.sqlite");
    let restored = temp.path().join("restored.sqlite");
    create_fixture(&source, "WAL", 300);

    let writer = Connection::open(&source).expect("open writer");
    writer
        .execute_batch("BEGIN IMMEDIATE")
        .expect("begin writer");
    writer
        .execute(
            "INSERT INTO parent(id, value) VALUES (9999, 'uncommitted')",
            [],
        )
        .expect("uncommitted write");

    let evidence = online_backup_to_path(
        &source,
        &artifact,
        &OnlineBackupOptions {
            pages_per_step: 1,
            pause_between_steps: Duration::from_millis(1),
            ..OnlineBackupOptions::default()
        },
        &BackupCancellationToken::default(),
    )
    .expect("online backup with active WAL writer");
    assert_eq!(evidence.journal_mode(), "wal");
    assert!(evidence.steps() > 1);

    let validation = restore_create_new(&artifact, &restored, Some(&count_oracle(300)))
        .expect("create-new restore");
    assert!(validation.integrity_ok());
    assert!(validation.foreign_keys_ok());
    assert!(validation.oracle_ok());
    writer.execute_batch("ROLLBACK").expect("release writer");
}

#[test]
fn exclusive_lock_and_disk_budget_leave_no_artifact_and_do_not_mutate_source() {
    let temp = tempfile::tempdir().expect("temp");
    let source = temp.path().join("source.sqlite");
    let locked_artifact = temp.path().join("locked.sqlite");
    let pressure_artifact = temp.path().join("pressure.sqlite");
    create_fixture(&source, "DELETE", 100);
    let original = probe_sqlite_file(&source).expect("source identity");

    let writer = Connection::open(&source).expect("open writer");
    writer
        .execute_batch("BEGIN EXCLUSIVE; UPDATE parent SET value='held' WHERE id=1")
        .expect("exclusive writer");
    let locked = online_backup_to_path(
        &source,
        &locked_artifact,
        &OnlineBackupOptions {
            busy_timeout: Duration::from_millis(5),
            max_transient_retries: 2,
            ..OnlineBackupOptions::default()
        },
        &BackupCancellationToken::default(),
    )
    .expect_err("bounded lock must fail");
    assert_eq!(locked.code(), "SQLITE_BACKUP_BUSY_TIMEOUT");
    assert!(!locked_artifact.exists());
    writer.execute_batch("ROLLBACK").expect("release lock");

    let pressure = online_backup_to_path(
        &source,
        &pressure_artifact,
        &OnlineBackupOptions {
            max_output_bytes: Some(1),
            ..OnlineBackupOptions::default()
        },
        &BackupCancellationToken::default(),
    )
    .expect_err("disk budget must fail before commit");
    assert_eq!(pressure.code(), "SQLITE_BACKUP_SPACE_INSUFFICIENT");
    assert!(!pressure_artifact.exists());
    assert!(original.same_file(&probe_sqlite_file(&source).expect("source after failures")));
}

struct RejectReplace;

impl AtomicReplacer for RejectReplace {
    fn replace(&self, _target: &Path, _replacement: &Path) -> Result<(), SqliteProviderError> {
        Err(SqliteProviderError::AtomicReplaceFailed(
            "injected pre-replace fault".to_owned(),
        ))
    }
}

#[test]
fn overwrite_rechecks_identity_and_pre_replace_fault_preserves_original() {
    let temp = tempfile::tempdir().expect("temp");
    let source = temp.path().join("source.sqlite");
    let replacement = temp.path().join("replacement.sqlite");
    let target = temp.path().join("target.sqlite");
    create_fixture(&source, "WAL", 10);
    create_fixture(&target, "DELETE", 2);
    online_backup_to_path(
        &source,
        &replacement,
        &OnlineBackupOptions::default(),
        &BackupCancellationToken::default(),
    )
    .expect("backup replacement");

    let bound = probe_sqlite_file(&target).expect("bound target");
    let error = restore_overwrite_with_replacer(
        &replacement,
        &target,
        &bound,
        Some(&count_oracle(10)),
        &RejectReplace,
    )
    .expect_err("injected replace fault");
    assert_eq!(error.code(), "SQLITE_RESTORE_ATOMIC_REPLACE_FAILED");
    let original_count: i64 = Connection::open(&target)
        .expect("target preserved")
        .query_row("SELECT COUNT(*) FROM parent", [], |row| row.get(0))
        .expect("count original");
    assert_eq!(original_count, 2);

    std::fs::remove_file(&target).expect("replace target identity");
    create_fixture(&target, "DELETE", 3);
    let mismatch = restore_overwrite_with_replacer(
        &replacement,
        &target,
        &bound,
        Some(&count_oracle(10)),
        &RejectReplace,
    )
    .expect_err("identity drift must fail before replacement");
    assert_eq!(mismatch.code(), "SQLITE_RESTORE_TARGET_IDENTITY_CHANGED");
}

#[test]
fn backup_source_emits_a_pipeline_stream_without_a_native_process() {
    let temp = tempfile::tempdir().expect("temp");
    let source_path = temp.path().join("source.sqlite");
    create_fixture(&source_path, "WAL", 20);
    let source = SqliteSource::new("plan-a", "app-file", source_path);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let plan = runtime.block_on(source.plan()).expect("plan");
    assert!(plan.globals_part.is_none());
    assert_eq!(plan.db_parts.len(), 1);
    assert_eq!(plan.db_parts[0].dump_tool, "sqlite-online-backup");

    let artifact = source
        .spawn(&plan.db_parts[0])
        .expect("spawn bounded backup");
    let DumpArtifact::Stream(mut stream) = artifact else {
        panic!("SQLite provider must use the common stream pipeline");
    };
    let mut bytes = Vec::new();
    runtime
        .block_on(async {
            use tokio::io::AsyncReadExt;
            stream.reader.read_to_end(&mut bytes).await?;
            let exit = stream.wait.await?;
            assert!(exit.success);
            anyhow::Ok(())
        })
        .expect("consume pipeline stream");
    assert!(bytes.starts_with(b"SQLite format 3\0"));
}

#[test]
fn cancellation_never_publishes_a_partial_artifact() {
    let temp = tempfile::tempdir().expect("temp");
    let source = temp.path().join("source.sqlite");
    let artifact = temp.path().join("cancelled.sqlite");
    create_fixture(&source, "WAL", 1_000);
    let token = Arc::new(BackupCancellationToken::default());
    let error = online_backup_to_path_observed(
        &source,
        &artifact,
        &OnlineBackupOptions {
            pages_per_step: 1,
            pause_between_steps: Duration::from_millis(1),
            ..OnlineBackupOptions::default()
        },
        token.as_ref(),
        |_| token.cancel(),
    )
    .expect_err("cancel while backup is running");
    assert_eq!(error.code(), "SQLITE_BACKUP_CANCELLED");
    assert!(!artifact.exists());
}

#[test]
fn system_atomic_overwrite_installs_only_the_fully_validated_database() {
    let temp = tempfile::tempdir().expect("temp");
    let source = temp.path().join("source.sqlite");
    let artifact = temp.path().join("artifact.sqlite");
    let target = temp.path().join("target.sqlite");
    create_fixture(&source, "WAL", 25);
    create_fixture(&target, "DELETE", 2);
    online_backup_to_path(
        &source,
        &artifact,
        &OnlineBackupOptions::default(),
        &BackupCancellationToken::default(),
    )
    .expect("create replacement artifact");
    let bound = probe_sqlite_file(&target).expect("bind target");
    let validation = restore_overwrite(&artifact, &target, &bound, Some(&count_oracle(25)))
        .expect("atomic overwrite");
    assert!(validation.integrity_ok());
    assert!(validation.foreign_keys_ok());
    assert!(validation.oracle_ok());
    let count: i64 = Connection::open(&target)
        .expect("open replaced target")
        .query_row("SELECT COUNT(*) FROM parent", [], |row| row.get(0))
        .expect("count replaced rows");
    assert_eq!(count, 25);
}

#[test]
fn symlink_or_reparse_source_is_rejected_before_sqlite_open() {
    let temp = tempfile::tempdir().expect("temp");
    let source = temp.path().join("source.sqlite");
    let link = temp.path().join("linked.sqlite");
    create_fixture(&source, "WAL", 1);
    create_file_symlink(&source, &link).expect("create test symlink");
    let error = probe_sqlite_file(&link).expect_err("linked source must fail closed");
    assert_eq!(error.code(), "SQLITE_PATH_REPARSE_POINT");
}

#[cfg(windows)]
fn create_file_symlink(source: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(source, link)
}

#[cfg(unix)]
fn create_file_symlink(source: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(source, link)
}

#[test]
fn locked_embedded_dependency_and_complete_evidence_publish_sqlite_supported() {
    let certification = sqlite_certification(
        "0123456789abcdef0123456789abcdef01234567",
        &"d".repeat(64),
        1_785_280_000_000,
    )
    .expect("SQLite certification");
    assert_eq!(certification.status(), SupportStatus::Supported);
    assert!(certification.is_publishable_supported());
    assert!(certification.missing_capabilities().is_empty());
    assert_eq!(certification.capabilities().len(), 9);
}
