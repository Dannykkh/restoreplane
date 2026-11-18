//! Native import >64MiB source proof. The 600s test watchdog bounds the whole
//! background job; production per-child 30s and all data quotas are unchanged.
//! Native managed-budget accounting is not process RSS or OS-medium IPC proof.
mod support;
use restoreplane_domain::workbench::*;
use restoreplane_domain::workbench_metadata::{CatalogKind, PageInput};
use restoreplane_domain::workbench_movement::*;
use restoreplane_workbench::data_movement::{
    FileCapabilities, ImportCoordinator, ImportPreviewInput, NativeSourceSelection,
};
use restoreplane_workbench::generated_sql::WriteCertification;
use restoreplane_workbench::metadata::MetadataCatalog;
use restoreplane_workbench::result_stream::NATIVE_RESULT_BYTES;
use std::{
    collections::BTreeMap,
    io::Write,
    sync::Arc,
    time::{Duration, Instant},
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_whole_preflight_and_sqlite_batches_import_more_than_64_mib_with_bounded_managed_memory(
) {
    const ROWS: u64 = 6000;
    const CELL_BYTES: usize = 12 * 1024;
    const CANARY: &str = "LARGE_IMPORT_ROW_CANARY_58d7e4_";
    let source_dir = tempfile::tempdir().unwrap();
    let source_path = source_dir.path().join("large.csv");
    let mut payload = String::from(CANARY);
    payload.extend(std::iter::repeat_n('x', CELL_BYTES - payload.len()));
    let mut writer =
        std::io::BufWriter::with_capacity(16 * 1024, std::fs::File::create(&source_path).unwrap());
    writer.write_all(b"id,value\n").unwrap();
    for id in 1..=ROWS {
        writeln!(writer, "{id},{payload}").unwrap();
    }
    writer.flush().unwrap();
    writer.get_ref().sync_all().unwrap();
    drop(writer);
    drop(payload);
    let source_bytes = std::fs::metadata(&source_path).unwrap().len();
    assert!(source_bytes > 64 * 1024 * 1024);
    let fixture = support::QueryFixture::new(
        support::Fixture::sqlite_mode(
            "CREATE TABLE records(id INTEGER PRIMARY KEY,value TEXT NOT NULL)",
            Mode::ReadWrite,
        )
        .await,
    );
    let ctx = &fixture.database.context;
    let session = fixture.database.session;
    let budget = fixture.streams.budget();
    let baseline = budget.retained_bytes();
    let metadata = Arc::new(MetadataCatalog::new(
        fixture.database.runtime.clone(),
        budget.clone(),
    ));
    let files = Arc::new(FileCapabilities::new(
        fixture.database.runtime.clone(),
        budget.clone(),
    ));
    let certification = WriteCertification::from_verified_build(
        "section06-native-20261117-current-exact".into(),
        vec![(Engine::Sqlite, "3.54.1".into())],
    )
    .unwrap();
    let imports = Arc::new(ImportCoordinator::new(
        fixture.database.runtime.clone(),
        metadata.clone(),
        files.clone(),
        fixture.journal.clone(),
        Some(certification),
    ));
    let object = metadata
        .page(
            ctx,
            session,
            None,
            CatalogKind::Relations,
            "records".into(),
            PageInput {
                cursor: None,
                limit: 100,
            },
        )
        .await
        .unwrap()
        .items
        .into_iter()
        .find(|item| item.name == "records")
        .unwrap()
        .object;
    let relation = metadata
        .resolve_relation(ctx, session, &object)
        .await
        .unwrap();
    let handle = files
        .register_source(
            ctx,
            session,
            NativeSourceSelection::from_native_dialog(&source_path).unwrap(),
        )
        .unwrap();
    let source = files
        .source_columns(ctx, session, handle, MovementFormat::Csv)
        .unwrap();
    let mapping = source
        .columns
        .iter()
        .map(|source| {
            (
                source.source_key.clone(),
                relation
                    .columns
                    .iter()
                    .find(|column| column.column.name == source.name)
                    .unwrap()
                    .column
                    .column_id,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let observer = rusqlite::Connection::open(&fixture.database.path).unwrap();
    let all_started = Instant::now();
    let preflight_started = Instant::now();
    let preview = imports
        .preview(
            ctx,
            ImportPreviewInput {
                session,
                object,
                source_handle: handle,
                format: MovementFormat::Csv,
                mapping,
                page: PageInput {
                    cursor: None,
                    limit: 100,
                },
            },
        )
        .await
        .unwrap();
    let preflight_ms = preflight_started.elapsed().as_millis();
    let before: i64 = observer
        .query_row("SELECT COUNT(*) FROM records", [], |row| row.get(0))
        .unwrap();
    assert_eq!(before, 0, "whole-source preflight sends no target insert");
    assert!(
        preview.rows.items.len() < 100,
        "the preview retains its 512KiB sample, not the whole source"
    );
    let input = ImportStartInput {
        session,
        preview_id: preview.preview_id,
        confirmed_warnings: preview.warnings.clone(),
    };
    drop(preview);
    let attempt = imports.prepare_start(ctx, &input).await.unwrap();
    let admitted = imports
        .start(ctx, &attempt.attempt_token, input)
        .await
        .map_err(|error| error.error)
        .unwrap();
    let mut next_progress = Instant::now() + Duration::from_secs(60);
    let operation = tokio::time::timeout(Duration::from_secs(600), async {
        loop {
            let operation = fixture.journal.get(ctx, &fixture.database.scope, admitted.operation_id).unwrap();
            if operation.state.is_terminal() { break operation; }
            if Instant::now() >= next_progress {
                let status = fixture.journal.movement_status(ctx, &fixture.database.scope, admitted.operation_id,
                    PageInput { cursor: None, limit: 1 }).unwrap();
                eprintln!("QA031 native_import elapsed_ms={} committed_rows={} committed_batches={} managed_peak_bytes={} rss_measured=false",
                    all_started.elapsed().as_millis(), status.progress.confirmed_committed_rows.value(),
                    status.progress.confirmed_committed_batches.value(), budget.peak_bytes());
                next_progress += Duration::from_secs(60);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }).await;
    let operation = match operation {
        Ok(operation) => operation,
        Err(error) => {
            let _ = imports.cancel(ctx, admitted.operation_id);
            panic!("whole-job test watchdog elapsed; per-child policy unchanged: {error}");
        }
    };
    assert_eq!(operation.state, OperationState::Completed);
    let expected_value = format!("{CANARY}{}", "x".repeat(CELL_BYTES - CANARY.len()));
    let observed: (i64, i64, i64) = observer
        .query_row(
            "SELECT COUNT(*),SUM(length(value)),SUM(value=?1) FROM records",
            [expected_value],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        observed,
        (
            i64::try_from(ROWS).unwrap(),
            i64::try_from(ROWS * u64::try_from(CELL_BYTES).unwrap()).unwrap(),
            i64::try_from(ROWS).unwrap(),
        )
    );
    let status = fixture
        .journal
        .movement_status(
            ctx,
            &fixture.database.scope,
            admitted.operation_id,
            PageInput {
                cursor: None,
                limit: 100,
            },
        )
        .unwrap();
    assert_eq!(status.progress.confirmed_committed_rows.value(), ROWS);
    assert!(status.progress.confirmed_committed_batches.value() > 1);
    assert_eq!(status.progress.unresolved_batches.value(), 0);
    assert!(budget.peak_bytes() <= NATIVE_RESULT_BYTES);
    for name in ["client-state.sqlite3", "client-state.sqlite3-wal"] {
        if let Ok(bytes) = std::fs::read(fixture.store.root().join(name)) {
            assert!(
                !bytes
                    .windows(CANARY.len())
                    .any(|window| window == CANARY.as_bytes()),
                "raw row must never persist in metadata journal/WAL"
            );
        }
    }
    println!("M3 boundary_violations=0; QA031 actual_native_import=true; source_bytes={source_bytes}; rows={ROWS}; preflight_ms={preflight_ms}; total_ms={}; committed_batches={}; managed_baseline_bytes={baseline}; managed_peak_bytes={}; native_cap_bytes={NATIVE_RESULT_BYTES}; rss_measured=false; js_file_body_loaded=false_by_source; child_timeout_seconds=30; test_watchdog_seconds=600; os_standard_integrity_e2e=false",
        all_started.elapsed().as_millis(), status.progress.confirmed_committed_batches.value(), budget.peak_bytes());
}
