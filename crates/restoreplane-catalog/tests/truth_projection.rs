use restoreplane_catalog::{artifacts_for_backup, latest_run_per_job, open_writer_conn};

#[test]
fn latest_failed_attempt_is_not_hidden_by_an_older_successful_backup() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open_writer_conn(&dir.path().join("catalog.sqlite")).unwrap();
    conn.execute_batch(
        "INSERT INTO backups (
             id, job, source, engine, kind, started_at, finished_at, status,
             manifest_signed, schema_version
         ) VALUES (
             'backup-ok', 'nightly', 'primary', 'postgresql', 'dump',
             '2026-07-28T03:00:00Z', '2026-07-28T03:05:00Z', 'success', 1, 1
         );
         INSERT INTO runs (id, job, started_at, status)
         VALUES
             ('run-ok', 'nightly', '2026-07-28T03:00:00Z', 'success'),
             ('run-failed', 'nightly', '2026-07-29T03:00:00Z', 'failed');",
    )
    .unwrap();

    let attempts = latest_run_per_job(&conn).unwrap();

    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].id, "run-failed");
    assert_eq!(attempts[0].job, "nightly");
    assert_eq!(attempts[0].status, "failed");
    assert_eq!(attempts[0].started_at, "2026-07-29T03:00:00Z");
}

#[test]
fn equal_timestamps_choose_the_last_inserted_attempt_deterministically() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open_writer_conn(&dir.path().join("catalog.sqlite")).unwrap();
    conn.execute_batch(
        "INSERT INTO runs (id, job, started_at, status)
         VALUES
             ('run-running', 'nightly', '2026-07-29T03:00:00Z', 'running'),
             ('run-failed', 'nightly', '2026-07-29T03:00:00Z', 'failed');",
    )
    .unwrap();

    let attempts = latest_run_per_job(&conn).unwrap();

    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].id, "run-failed");
    assert_eq!(attempts[0].status, "failed");
}

#[test]
fn artifact_projection_contains_only_persisted_rows() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open_writer_conn(&dir.path().join("catalog.sqlite")).unwrap();
    conn.execute_batch(
        "INSERT INTO backups (
             id, job, source, engine, kind, started_at, finished_at, status,
             manifest_signed, schema_version
         ) VALUES (
             'backup-ok', 'nightly', 'primary', 'postgresql', 'dump',
             '2026-07-29T03:00:00Z', '2026-07-29T03:05:00Z', 'success', 1, 1
         );
         INSERT INTO artifacts (
             backup_id, dest_name, dest_kind, object_key, object_checksum,
             immutability, retention_until, status, uploaded_at
         ) VALUES (
             'backup-ok', 's3-primary', 's3', 'nightly/backup-ok.enc', 'sha256:abc',
             NULL, NULL, 'committed', '2026-07-29T03:05:00Z'
         );",
    )
    .unwrap();

    let artifacts = artifacts_for_backup(&conn, "backup-ok").unwrap();

    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0].dest_name, "s3-primary");
    assert_eq!(artifacts[0].status, "committed");
    assert_eq!(artifacts[0].immutability, None);
    assert_eq!(artifacts[0].retention_until, None);
}
