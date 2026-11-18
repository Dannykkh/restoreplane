#![cfg(windows)]
//! Actual local NTFS opened-handle file tests. No OS/service authorization claim.
mod support;
use restoreplane_domain::workbench::{Column, Engine, TypedCell};
use restoreplane_domain::workbench_movement::{FileArtifactState, MovementFormat};
use restoreplane_domain::workbench_stream::Completion;
use restoreplane_workbench::data_movement::NativeDestinationSelection;
use restoreplane_workbench::movement_codec::FormulaPolicy;
use restoreplane_workbench::movement_export::{
    remove_recorded_partial, validate_export_cells, ExportCoordinator, ExportFaultPoint,
    ExportFile, ExportFilePlan, EXPORT_STAGING_BYTES,
};
use restoreplane_workbench::result_stream::NativeBudget;
use restoreplane_workbench::WorkbenchError;
use std::path::Path;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

async fn retained_source(
    rows: Vec<Vec<TypedCell>>,
    completeness: Completion,
) -> (
    support::QueryFixture,
    restoreplane_domain::workbench_stream::StreamIdentity,
) {
    use restoreplane_domain::workbench_metadata::{Consistency, Row};
    use restoreplane_domain::workbench_stream::ResultEvent;
    use restoreplane_workbench::result_stream::StreamSpec;
    let fixture=support::QueryFixture::new(support::Fixture::sqlite("CREATE TABLE marker(id INTEGER PRIMARY KEY, value TEXT); INSERT INTO marker VALUES(1,'server-value');").await);
    let columns = columns();
    let identity = fixture
        .streams
        .create(
            &fixture.database.context,
            fixture.database.session,
            StreamSpec {
                query_run_id: Uuid::new_v4(),
                result_set_id: Uuid::new_v4(),
                schema_version: "d".repeat(64),
                columns: columns.clone(),
                consistency: Consistency::LivePerPage,
            },
        )
        .unwrap();
    let mut discard = |_: &restoreplane_domain::workbench_stream::ResultEnvelope| Ok(());
    fixture
        .streams
        .try_publish(
            &fixture.database.context,
            &identity,
            ResultEvent::Schema {
                columns,
                consistency: Consistency::LivePerPage,
            },
            &mut discard,
        )
        .unwrap();
    let count = rows.len();
    for (index, cells) in rows.into_iter().enumerate() {
        fixture
            .streams
            .try_publish(
                &fixture.database.context,
                &identity,
                ResultEvent::Rows {
                    start_row: (index as u64).into(),
                    rows: vec![Row {
                        row_handle: Uuid::new_v4(),
                        cells,
                    }],
                },
                &mut discard,
            )
            .unwrap();
    }
    fixture
        .streams
        .try_publish(
            &fixture.database.context,
            &identity,
            ResultEvent::Terminal {
                completeness,
                rows_seen: (count as u64).into(),
                operation_version: 1.into(),
                error_code: None,
            },
            &mut discard,
        )
        .unwrap();
    (fixture, identity)
}
async fn wait_terminal(
    fixture: &support::QueryFixture,
    id: Uuid,
) -> restoreplane_domain::workbench::Operation {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            let operation = fixture
                .journal
                .get(&fixture.database.context, &fixture.database.scope, id)
                .unwrap();
            if operation.state.is_terminal() {
                return operation;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}
fn shown_input(
    fixture: &support::QueryFixture,
    identity: restoreplane_domain::workbench_stream::StreamIdentity,
    destination_handle: Uuid,
    row_count: u32,
) -> restoreplane_domain::workbench_movement::ExportStartInput {
    use restoreplane_domain::workbench_movement::{
        ExportSource, ExportStartInput, FormulaPolicy, PartialPolicy, ShownRange,
    };
    ExportStartInput {
        session: fixture.database.session,
        source: ExportSource::Shown {
            identity,
            range: ShownRange {
                start_row: 0.into(),
                row_count,
            },
        },
        destination_handle,
        format: MovementFormat::Csv,
        formula_policy: FormulaPolicy::Escape,
        partial_policy: PartialPolicy::Retain,
    }
}

async fn fresh_setup(
    fixture: &support::QueryFixture,
    path: &Path,
    sql: &str,
) -> (
    ExportCoordinator,
    restoreplane_domain::workbench_movement::ExportStartInput,
) {
    let (coordinator, input, _) = fresh_setup_native(fixture, path, sql).await;
    (coordinator, input)
}
async fn fresh_setup_native(
    fixture: &support::QueryFixture,
    path: &Path,
    sql: &str,
) -> (
    ExportCoordinator,
    restoreplane_domain::workbench_movement::ExportStartInput,
    Arc<restoreplane_workbench::browse::BrowseService>,
) {
    use restoreplane_domain::workbench_movement::{
        ExportSource, ExportStartInput, FormulaPolicy, PartialPolicy,
    };
    use restoreplane_domain::workbench_query::{QueryPurpose, QueryReviewInput};
    let metadata = Arc::new(restoreplane_workbench::metadata::MetadataCatalog::new(
        fixture.database.runtime.clone(),
        fixture.streams.budget(),
    ));
    let certification = restoreplane_workbench::browse::SnapshotCertification::from_verified_build(
        "native-export-snapshot-regression".into(),
        vec![(Engine::Sqlite, rusqlite::version().into())],
    )
    .unwrap();
    let browse = Arc::new(
        restoreplane_workbench::browse::BrowseService::with_snapshot_certification(
            metadata.clone(),
            fixture.streams.clone(),
            certification,
        ),
    );
    let files = Arc::new(
        restoreplane_workbench::data_movement::FileCapabilities::new(
            fixture.database.runtime.clone(),
            fixture.streams.budget(),
        ),
    );
    let handle = files
        .register_destination(
            &fixture.database.context,
            fixture.database.session,
            NativeDestinationSelection::from_native_dialog(path).unwrap(),
        )
        .unwrap();
    let review = fixture
        .coordinator
        .review(
            &fixture.database.context,
            QueryReviewInput {
                session: fixture.database.session,
                sql: sql.into(),
                purpose: QueryPurpose::Run,
            },
        )
        .await
        .unwrap();
    let coordinator = ExportCoordinator::new(
        fixture.database.runtime.clone(),
        files,
        fixture.journal.clone(),
        fixture.streams.clone(),
    )
    .with_fresh_reads(fixture.coordinator.clone(), metadata, browse.clone());
    (
        coordinator,
        ExportStartInput {
            session: fixture.database.session,
            source: ExportSource::Fresh {
                review_id: review.review_id,
                snapshot_id: None,
            },
            destination_handle: handle,
            format: MovementFormat::Csv,
            formula_policy: FormulaPolicy::Escape,
            partial_policy: PartialPolicy::Retain,
        },
        browse,
    )
}

#[tokio::test]
async fn fresh_export_owns_snapshot_cleanup_on_success_failure_cancel_and_admission_abort() {
    use restoreplane_domain::workbench::{AuthorizePurpose, OperationState};
    use restoreplane_domain::workbench_movement::ExportSource;
    use std::sync::Condvar;
    struct CountSink;
    #[async_trait::async_trait]
    impl restoreplane_workbench::driver::RowSink for CountSink {
        async fn columns(&mut self, _: Vec<Column>) -> Result<(), WorkbenchError> {
            Ok(())
        }
        async fn row(&mut self, cells: Vec<TypedCell>) -> Result<(), WorkbenchError> {
            assert_eq!(cells, vec![TypedCell::I64 { value: "2".into() }]);
            Ok(())
        }
    }
    for case in ["complete", "source_failure", "cancel", "admission_abort"] {
        let fixture = support::QueryFixture::new(support::Fixture::sqlite(
            "CREATE TABLE records(id INTEGER PRIMARY KEY,label TEXT); INSERT INTO records VALUES(1,'exact'),(2,'other')",
        ).await);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(format!("{case}.csv"));
        let (coordinator, mut input, browse) =
            fresh_setup_native(&fixture, &path, "SELECT id,label FROM main.records;").await;
        let (snapshot_id, _) = browse
            .snapshot_begin(
                &fixture.database.context,
                fixture.database.session,
                "serializable",
                60_000,
            )
            .await
            .unwrap_or_else(|error| panic!("snapshot begin {case}: {error:?}"));
        let ExportSource::Fresh {
            snapshot_id: selected,
            ..
        } = &mut input.source
        else {
            unreachable!()
        };
        *selected = Some(snapshot_id);
        let entered = Arc::new(tokio::sync::Notify::new());
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let fault_entered = entered.clone();
        let fault_gate = gate.clone();
        let coordinator = Arc::new(coordinator.with_fault_hook(Arc::new(move |point| {
            if point == ExportFaultPoint::AfterAdmission && case == "admission_abort" {
                return Err(WorkbenchError::Io);
            }
            if point == ExportFaultPoint::BeforeRead {
                if case == "source_failure" {
                    return Err(WorkbenchError::Io);
                }
                if case == "cancel" {
                    fault_entered.notify_one();
                    let (released, wake) = &*fault_gate;
                    let mut released = released.lock().unwrap();
                    while !*released {
                        released = wake.wait(released).unwrap();
                    }
                }
            }
            Ok(())
        })));
        let admission = coordinator
            .start(fixture.database.context.clone(), input.clone())
            .await;
        let id = if case == "admission_abort" {
            admission.unwrap_err().operation_id.unwrap()
        } else {
            admission.unwrap().operation_id
        };
        if case == "cancel" {
            tokio::time::timeout(std::time::Duration::from_secs(10), entered.notified())
                .await
                .unwrap();
            assert_eq!(
                coordinator
                    .start(fixture.database.context.clone(), input)
                    .await
                    .unwrap_err()
                    .error,
                WorkbenchError::TargetBusy
            );
            assert!(coordinator.cancel(&fixture.database.context, id).unwrap());
            let (released, wake) = &*gate;
            *released.lock().unwrap() = true;
            wake.notify_all();
        }
        let operation = wait_terminal(&fixture, id).await;
        assert_eq!(
            operation.state == OperationState::Completed,
            case == "complete",
            "{case}"
        );
        assert_eq!(path.exists(), case == "complete", "{case}");
        assert!(
            !browse
                .snapshot_end(
                    &fixture.database.context,
                    fixture.database.session,
                    snapshot_id
                )
                .await
                .unwrap(),
            "snapshot must already be ended: {case}"
        );
        let observed = fixture
            .database
            .runtime
            .with_connection(
                &fixture.database.context,
                fixture.database.session,
                AuthorizePurpose::Read,
                move |connection| {
                    Box::pin(async move {
                        connection
                            .execute_bound(
                                "SELECT COUNT(*) FROM records",
                                &[],
                                restoreplane_workbench::driver::QueryLimits {
                                    max_rows: 2,
                                    max_bytes: 4096,
                                    timeout: std::time::Duration::from_secs(2),
                                },
                                &mut CountSink,
                            )
                            .await
                    })
                },
            )
            .await;
        let observed = if case == "cancel" {
            // Cancellation does not prove a reusable physical connection.
            // This native action error retires the socket even though the
            // export-owned snapshot registry has been cleaned up.
            assert!(matches!(observed, Err(WorkbenchError::SessionStale)));
            let retired = fixture
                .database
                .runtime
                .list(&fixture.database.context, &fixture.database.scope)
                .unwrap();
            assert_eq!(
                retired[0].state,
                restoreplane_domain::workbench::SessionState::Retiring
            );
            let source = &fixture.database.path;
            let parked = source.with_file_name("retired-export-source.sqlite3");
            assert!(source.is_absolute() && parked.parent() == source.parent());
            std::fs::rename(source, &parked).unwrap();
            std::fs::rename(&parked, source).unwrap();
            let reopened = fixture
                .coordinator
                .reconnect(
                    &fixture.database.context,
                    fixture.database.session,
                    &"b".repeat(64),
                )
                .await
                .unwrap();
            assert!(
                reopened.session.connection_generation.value()
                    > fixture.database.session.connection_generation.value()
            );
            fixture
                .database
                .runtime
                .with_connection(
                    &fixture.database.context,
                    reopened.session,
                    AuthorizePurpose::Read,
                    move |connection| {
                        Box::pin(async move {
                            connection
                                .execute_bound(
                                    "SELECT COUNT(*) FROM records",
                                    &[],
                                    restoreplane_workbench::driver::QueryLimits {
                                        max_rows: 2,
                                        max_bytes: 4096,
                                        timeout: std::time::Duration::from_secs(2),
                                    },
                                    &mut CountSink,
                                )
                                .await
                        })
                    },
                )
                .await
                .unwrap()
        } else {
            observed.unwrap_or_else(|error| panic!("ordinary read after {case}: {error:?}"))
        };
        assert_eq!(observed.rows_kept, 1, "confirmed cleanup keeps normal reads available (explicit new generation after cancel): {case}");
    }
}

#[tokio::test]
async fn fresh_file_stream_exceeds_interactive_64m_without_retaining_whole_result() {
    use restoreplane_domain::workbench::{Mode, OperationState};
    use restoreplane_domain::workbench_movement::MovementMetadata;
    let fixture = support::QueryFixture::new(support::Fixture::sqlite_mode("CREATE TABLE records(id INTEGER PRIMARY KEY,label TEXT); WITH RECURSIVE seq(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM seq WHERE i<6000) INSERT INTO records SELECT i,printf('%012000d',i) FROM seq", Mode::ReadOnly).await);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("fresh.csv");
    let (coordinator, input) =
        fresh_setup(&fixture, &path, "SELECT id,label FROM main.records;").await;
    let admitted = Arc::new(coordinator)
        .start(fixture.database.context.clone(), input)
        .await
        .unwrap();
    let operation = wait_terminal(&fixture, admitted.operation_id).await;
    assert_eq!(operation.state, OperationState::Completed);
    let metadata = fixture
        .journal
        .movement_metadata(
            &fixture.database.context,
            &fixture.database.scope,
            admitted.operation_id,
        )
        .unwrap()
        .unwrap();
    let MovementMetadata::Export {
        file,
        progress,
        source,
        ..
    } = metadata
    else {
        panic!("export metadata")
    };
    assert!(file.finalized);
    assert_eq!(file.source_completeness, Completion::Complete);
    assert_eq!(file.rows_written.value(), 6000);
    assert_eq!(progress.confirmed_committed_rows.value(), 0);
    assert!(std::fs::metadata(&path).unwrap().len() > 64 * 1024 * 1024);
    assert!(matches!(
        source,
        restoreplane_domain::workbench_movement::PersistedExportSource::Fresh { .. }
    ));
}

#[tokio::test]
async fn fresh_writer_failure_retains_partial_and_cannot_certify_complete() {
    use restoreplane_domain::workbench::OperationState;
    use restoreplane_domain::workbench_movement::MovementMetadata;
    let fixture = support::QueryFixture::new(support::Fixture::sqlite("CREATE TABLE records(id INTEGER PRIMARY KEY,label TEXT); INSERT INTO records VALUES(1,'exact')").await);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("fresh.csv");
    let (coordinator, input) =
        fresh_setup(&fixture, &path, "SELECT id,label FROM main.records;").await;
    let coordinator = Arc::new(coordinator.with_fault_hook(Arc::new(|point| {
        if point == ExportFaultPoint::BeforeSync {
            Err(WorkbenchError::Io)
        } else {
            Ok(())
        }
    })));
    let admitted = coordinator
        .start(fixture.database.context.clone(), input)
        .await
        .unwrap();
    assert_ne!(
        wait_terminal(&fixture, admitted.operation_id).await.state,
        OperationState::Completed
    );
    let metadata = fixture
        .journal
        .movement_metadata(
            &fixture.database.context,
            &fixture.database.scope,
            admitted.operation_id,
        )
        .unwrap()
        .unwrap();
    let MovementMetadata::Export { file, .. } = metadata else {
        panic!("export metadata")
    };
    assert!(!file.finalized);
    assert!(!path.exists());
    assert_eq!(file.state, FileArtifactState::Partial);
    assert!(Path::new(file.partial_display_path.as_ref().unwrap()).exists());
}

#[tokio::test]
async fn fresh_finalize_fault_remains_unconfirmed_after_journal_and_window_reopen() {
    use restoreplane_domain::workbench::{Mode, OperationState};
    use restoreplane_domain::workbench_movement::MovementMetadata;
    let fixture = support::QueryFixture::new(support::Fixture::sqlite_mode(
        "CREATE TABLE records(id INTEGER PRIMARY KEY,label TEXT); INSERT INTO records VALUES(1,'exact')", Mode::ReadOnly,
    ).await);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("reopened.csv");
    let (coordinator, input) =
        fresh_setup(&fixture, &path, "SELECT id,label FROM main.records;").await;
    let coordinator = Arc::new(coordinator.with_fault_hook(Arc::new(|point| {
        if point == ExportFaultPoint::AfterRename {
            Err(WorkbenchError::Io)
        } else {
            Ok(())
        }
    })));
    let admission = coordinator
        .start(fixture.database.context.clone(), input)
        .await
        .unwrap();
    assert_ne!(
        wait_terminal(&fixture, admission.operation_id).await.state,
        OperationState::Completed
    );
    let before = std::fs::read(&path).unwrap();
    drop(coordinator);
    // No source query can be replayed successfully after this independent drop.
    rusqlite::Connection::open(&fixture.database.path)
        .unwrap()
        .execute_batch("DROP TABLE records")
        .unwrap();
    let reopened =
        restoreplane_workbench::journal::OperationJournal::open(fixture.store.clone()).unwrap();
    let new_window = restoreplane_workbench::InvocationContext::new(
        fixture.database.context.principal().clone(),
        Uuid::new_v4(),
    );
    let operation = reopened
        .get(&new_window, &fixture.database.scope, admission.operation_id)
        .unwrap();
    assert_ne!(operation.state, OperationState::Completed);
    let MovementMetadata::Export { file, .. } = reopened
        .movement_metadata(&new_window, &fixture.database.scope, admission.operation_id)
        .unwrap()
        .unwrap()
    else {
        panic!("export metadata")
    };
    assert_eq!(file.state, FileArtifactState::Unconfirmed);
    assert!(!file.finalized);
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[tokio::test]
async fn fresh_unproven_query_refuses_before_file_creation_and_journal_admission() {
    let fixture = support::QueryFixture::new(
        support::Fixture::sqlite("CREATE TABLE records(id INTEGER PRIMARY KEY,label TEXT)").await,
    );
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("fresh.csv");
    let (coordinator, input) =
        fresh_setup(&fixture, &path, "SELECT upper(label) FROM main.records;").await;
    let error = Arc::new(coordinator)
        .start(fixture.database.context.clone(), input)
        .await
        .unwrap_err();
    assert_eq!(error.error, WorkbenchError::Unsupported);
    assert!(error.operation_id.is_none());
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn shown_export_streams_one_row_pages_over_512k_without_query_reexecution_or_cap_promotion() {
    use restoreplane_domain::workbench::OperationState;
    use restoreplane_domain::workbench_movement::MovementMetadata;
    let large = vec![
        TypedCell::Text {
            value: "x".repeat(64 * 1024)
        };
        3
    ];
    let (fixture, identity) = retained_source(
        vec![large.clone(), large.clone(), large],
        Completion::Capped,
    )
    .await;
    assert!(matches!(
        fixture
            .streams
            .page(&fixture.database.context, &identity, 0, 3),
        Err(WorkbenchError::Limit)
    ));
    // The actual SQLite source no longer has the table. Export remains a read
    // of retained frames; the coordinator has no SQL/query execution dependency.
    rusqlite::Connection::open(&fixture.database.path)
        .unwrap()
        .execute_batch("DROP TABLE marker")
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("selected.csv");
    let files = Arc::new(
        restoreplane_workbench::data_movement::FileCapabilities::new(
            fixture.database.runtime.clone(),
            fixture.streams.budget(),
        ),
    );
    let handle = files
        .register_destination(
            &fixture.database.context,
            fixture.database.session,
            NativeDestinationSelection::from_native_dialog(&path).unwrap(),
        )
        .unwrap();
    let coordinator = Arc::new(ExportCoordinator::new(
        fixture.database.runtime.clone(),
        files,
        fixture.journal.clone(),
        fixture.streams.clone(),
    ));
    let admitted = coordinator
        .start(
            fixture.database.context.clone(),
            shown_input(&fixture, identity, handle, 3),
        )
        .await
        .unwrap();
    let operation = wait_terminal(&fixture, admitted.operation_id).await;
    assert_eq!(operation.state, OperationState::Completed);
    let metadata = fixture
        .journal
        .movement_metadata(
            &fixture.database.context,
            &fixture.database.scope,
            admitted.operation_id,
        )
        .unwrap()
        .unwrap();
    let MovementMetadata::Export { file, progress, .. } = metadata else {
        panic!("export metadata")
    };
    assert_eq!(file.state, FileArtifactState::Complete);
    assert!(file.finalized);
    assert_eq!(file.source_completeness, Completion::Capped);
    assert_eq!(file.rows_written.value(), 3);
    assert_eq!(progress.rows_processed.value(), 3);
    assert_eq!(progress.confirmed_committed_rows.value(), 0);
    assert!(std::fs::metadata(&path).unwrap().len() > 512 * 1024);
    let raw: String = fixture
        .store
        .read(|connection| {
            connection
                .query_row(
                    "SELECT movement_metadata_json FROM operations WHERE operation_id=?1",
                    [admitted.operation_id.to_string()],
                    |row| row.get(0),
                )
                .map_err(|_| WorkbenchError::Io)
        })
        .unwrap();
    assert!(!raw.contains(&"x".repeat(1000)));
}

#[tokio::test]
async fn missing_later_retained_row_keeps_partial_and_never_requeries() {
    use restoreplane_domain::workbench_movement::MovementMetadata;
    let (fixture, identity) = retained_source(vec![row()], Completion::Interrupted).await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("short.csv");
    let files = Arc::new(
        restoreplane_workbench::data_movement::FileCapabilities::new(
            fixture.database.runtime.clone(),
            fixture.streams.budget(),
        ),
    );
    let handle = files
        .register_destination(
            &fixture.database.context,
            fixture.database.session,
            NativeDestinationSelection::from_native_dialog(&path).unwrap(),
        )
        .unwrap();
    let coordinator = Arc::new(ExportCoordinator::new(
        fixture.database.runtime.clone(),
        files,
        fixture.journal.clone(),
        fixture.streams.clone(),
    ));
    let admitted = coordinator
        .start(
            fixture.database.context.clone(),
            shown_input(&fixture, identity, handle, 2),
        )
        .await
        .unwrap();
    let operation = wait_terminal(&fixture, admitted.operation_id).await;
    assert_ne!(
        operation.state,
        restoreplane_domain::workbench::OperationState::Completed
    );
    let metadata = fixture
        .journal
        .movement_metadata(
            &fixture.database.context,
            &fixture.database.scope,
            admitted.operation_id,
        )
        .unwrap()
        .unwrap();
    let MovementMetadata::Export { file, .. } = metadata else {
        panic!("export metadata")
    };
    assert_eq!(file.state, FileArtifactState::Partial);
    assert_eq!(file.rows_written.value(), 1);
    assert_eq!(file.source_completeness, Completion::Interrupted);
    assert!(!file.finalized);
    assert!(!path.exists());
    assert!(std::path::Path::new(&file.partial_display_path.unwrap()).exists());
}

#[tokio::test]
async fn post_rename_durable_finalize_veto_keeps_unconfirmed_overlay_and_final_file() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (fixture, identity) = retained_source(vec![row()], Completion::Complete).await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("veto.csv");
    let files = Arc::new(
        restoreplane_workbench::data_movement::FileCapabilities::new(
            fixture.database.runtime.clone(),
            fixture.streams.budget(),
        ),
    );
    let handle = files
        .register_destination(
            &fixture.database.context,
            fixture.database.session,
            NativeDestinationSelection::from_native_dialog(&path).unwrap(),
        )
        .unwrap();
    let store = fixture.store.clone();
    let coordinator = Arc::new(
        ExportCoordinator::new(
            fixture.database.runtime.clone(),
            files,
            fixture.journal.clone(),
            fixture.streams.clone(),
        )
        .with_fault_hook(Arc::new(move |point| {
            if point == ExportFaultPoint::AfterRename {
                store.write(|connection| {
                    let count = AtomicUsize::new(0);
                    connection
                        .commit_hook(Some(move || count.fetch_add(1, Ordering::SeqCst) >= 1))
                        .map_err(|_| WorkbenchError::StoreUnavailable)?;
                    Ok(())
                })?;
            }
            Ok(())
        })),
    );
    let admitted = coordinator
        .start(
            fixture.database.context.clone(),
            shown_input(&fixture, identity, handle, 1),
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            if coordinator
                .observation(
                    &fixture.database.context,
                    &fixture.database.scope,
                    admitted.operation_id,
                )
                .unwrap()
                .is_some_and(|file| file.state == FileArtifactState::Unconfirmed)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!fixture.journal.write_enabled());
    assert!(path.exists());
    let file = coordinator
        .observation(
            &fixture.database.context,
            &fixture.database.scope,
            admitted.operation_id,
        )
        .unwrap()
        .unwrap();
    assert_eq!(file.state, FileArtifactState::Unconfirmed);
    assert!(!file.finalized);
    assert!(file.partial_display_path.is_none());
    let operation = fixture
        .journal
        .get(
            &fixture.database.context,
            &fixture.database.scope,
            admitted.operation_id,
        )
        .unwrap();
    assert_ne!(
        operation.state,
        restoreplane_domain::workbench::OperationState::Completed
    );
}

#[tokio::test]
async fn scoped_cancel_before_finalize_removes_only_the_job_partial_with_explicit_policy() {
    use restoreplane_domain::workbench_movement::{MovementMetadata, PartialPolicy};
    use std::sync::{mpsc, Condvar};
    let (fixture, identity) = retained_source(vec![row()], Completion::Complete).await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("cancel.csv");
    let unrelated = directory.path().join("unrelated.partial");
    std::fs::write(&unrelated, b"keep-native-canary").unwrap();
    let files = Arc::new(
        restoreplane_workbench::data_movement::FileCapabilities::new(
            fixture.database.runtime.clone(),
            fixture.streams.budget(),
        ),
    );
    let handle = files
        .register_destination(
            &fixture.database.context,
            fixture.database.session,
            NativeDestinationSelection::from_native_dialog(&path).unwrap(),
        )
        .unwrap();
    let (tx, rx) = mpsc::channel();
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let held = gate.clone();
    let coordinator = Arc::new(
        ExportCoordinator::new(
            fixture.database.runtime.clone(),
            files,
            fixture.journal.clone(),
            fixture.streams.clone(),
        )
        .with_fault_hook(Arc::new(move |point| {
            if point == ExportFaultPoint::BeforeFlush {
                tx.send(()).unwrap();
                let (lock, condition) = &*held;
                let mut ready = lock.lock().unwrap();
                while !*ready {
                    ready = condition.wait(ready).unwrap();
                }
            }
            Ok(())
        })),
    );
    let mut input = shown_input(&fixture, identity, handle, 1);
    input.partial_policy = PartialPolicy::RemoveOnCancel;
    let admitted = coordinator
        .start(fixture.database.context.clone(), input)
        .await
        .unwrap();
    rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
    let other = restoreplane_workbench::InvocationContext::new(
        fixture.database.context.principal().clone(),
        Uuid::new_v4(),
    );
    assert_eq!(
        coordinator.cancel(&other, admitted.operation_id),
        Err(WorkbenchError::Forbidden)
    );
    assert!(coordinator
        .cancel(&fixture.database.context, admitted.operation_id)
        .unwrap());
    let (lock, condition) = &*gate;
    *lock.lock().unwrap() = true;
    condition.notify_all();
    let operation = wait_terminal(&fixture, admitted.operation_id).await;
    assert_ne!(
        operation.state,
        restoreplane_domain::workbench::OperationState::Completed
    );
    let metadata = fixture
        .journal
        .movement_metadata(
            &fixture.database.context,
            &fixture.database.scope,
            admitted.operation_id,
        )
        .unwrap()
        .unwrap();
    let MovementMetadata::Export { file, .. } = metadata else {
        panic!("export metadata")
    };
    assert_eq!(file.state, FileArtifactState::Removed);
    assert_eq!(file.source_completeness, Completion::Complete);
    assert!(!file.finalized);
    assert!(!path.exists());
    assert_eq!(std::fs::read(&unrelated).unwrap(), b"keep-native-canary");
}

fn columns() -> Vec<Column> {
    ["counter", "amount", "optional"]
        .into_iter()
        .map(|name| Column {
            column_id: Uuid::new_v4(),
            name: name.into(),
            type_name: "exact".into(),
            nullable: true,
            editable: false,
            readonly_reason: None,
        })
        .collect()
}
fn row() -> Vec<TypedCell> {
    vec![
        TypedCell::U64 {
            value: "18446744073709551615".into(),
        },
        TypedCell::Decimal {
            value: "12345678901234567890.000000001".into(),
            scale: Some(9),
        },
        TypedCell::Null,
    ]
}
#[test]
fn unproved_mysql_timestamp_instants_are_refused_without_rejecting_postgresql_naive_values() {
    let timestamp = TypedCell::Temporal {
        value: "2026-11-17 12:34:56.123456".into(),
        type_name: "timestamp".into(),
        offset: None,
        precision: 6,
    };
    for engine in [Engine::Mysql, Engine::Mariadb] {
        assert_eq!(
            validate_export_cells(engine, std::slice::from_ref(&timestamp)),
            Err(WorkbenchError::Unsupported)
        );
        let datetime = TypedCell::Temporal {
            value: "2026-11-17 12:34:56.123456".into(),
            type_name: "datetime".into(),
            offset: None,
            precision: 6,
        };
        assert!(validate_export_cells(engine, &[datetime]).is_ok());
        let known = TypedCell::Temporal {
            value: "2026-11-17 12:34:56.123456".into(),
            type_name: "timestamp".into(),
            offset: Some("+00:00".into()),
            precision: 6,
        };
        assert!(validate_export_cells(engine, &[known]).is_ok());
    }
    assert!(validate_export_cells(Engine::Postgresql, &[timestamp]).is_ok());
}
fn create(path: &Path, format: MovementFormat) -> (ExportFile, Arc<NativeBudget>) {
    let native = NativeDestinationSelection::from_native_dialog(path).unwrap();
    let plan = ExportFilePlan::new(native, Uuid::new_v4(), format).unwrap();
    let budget = Arc::new(NativeBudget::default());
    (plan.create(&budget).unwrap(), budget)
}

#[test]
fn only_same_handle_rename_after_syntax_flush_and_sync_publishes_the_final_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("exact.csv");
    let native = NativeDestinationSelection::from_native_dialog(&path).unwrap();
    let plan = ExportFilePlan::new(native, Uuid::new_v4(), MovementFormat::Csv).unwrap();
    assert!(!plan.partial_path().exists());
    assert_eq!(
        plan.initial_artifact(Completion::Capped).state,
        FileArtifactState::NotStarted
    );
    let budget = Arc::new(NativeBudget::default());
    let mut output = plan.create(&budget).unwrap();
    let partial = output.partial_path().to_owned();
    let identity = output.identity().to_owned();
    assert!(!path.exists());
    assert!(partial.exists());
    output.begin(&columns(), FormulaPolicy::Escape).unwrap();
    output.write_row(&row()).unwrap();
    assert_eq!(output.stats().rows, 1);
    assert!(output.finalize().is_err());
    assert!(!path.exists());
    let stats = output.seal().unwrap();
    assert!(!path.exists());
    assert_eq!(std::fs::metadata(&partial).unwrap().len(), stats.bytes);
    output.finalize().unwrap();
    assert!(!partial.exists());
    let final_handle = std::fs::File::open(&path).unwrap();
    assert_eq!(
        restoreplane_source_sqlite::opened_file_object_id(&final_handle).unwrap(),
        identity
    );
    let bytes = std::fs::read_to_string(&path).unwrap();
    assert!(bytes.contains("18446744073709551615"));
    assert!(bytes.contains("12345678901234567890.000000001"));
    assert!(bytes.contains(r"\N"));
    let artifact = output.artifact(Completion::Capped).unwrap();
    assert_eq!(artifact.state, FileArtifactState::Complete);
    assert!(artifact.finalized);
    assert_eq!(artifact.source_completeness, Completion::Capped);
    assert_eq!(artifact.rows_written.value(), 1);
    assert!(artifact.partial_display_path.is_none());
    drop(final_handle);
    drop(output);
    assert_eq!(budget.retained_bytes(), 0);
}

#[test]
fn destination_created_after_dialog_is_never_overwritten_and_partial_is_retained() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("raced.csv");
    let (mut output, _) = create(&path, MovementFormat::Csv);
    let partial = output.partial_path().to_owned();
    output.begin(&columns(), FormulaPolicy::Escape).unwrap();
    output.write_row(&row()).unwrap();
    output.seal().unwrap();
    std::fs::write(&path, b"existing-destination-canary").unwrap();
    assert!(output.finalize().is_err());
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"existing-destination-canary"
    );
    assert!(partial.exists());
    let artifact = output.artifact(Completion::Complete).unwrap();
    assert_eq!(artifact.state, FileArtifactState::Partial);
    assert!(!artifact.finalized);
}

#[test]
fn open_partial_cannot_be_replaced_and_cleanup_deletes_only_its_owned_handle() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("selected.csv");
    let other = directory.path().join("other.csv");
    std::fs::write(&other, b"unrelated-canary").unwrap();
    let (mut output, _) = create(&path, MovementFormat::Csv);
    let partial = output.partial_path().to_owned();
    output.begin(&columns(), FormulaPolicy::Escape).unwrap();
    output.write_row(&row()).unwrap();
    assert!(std::fs::rename(&partial, directory.path().join("stolen.partial")).is_err());
    assert!(std::fs::remove_file(&partial).is_err());
    let artifact = output.discard(Completion::Cancelled).unwrap();
    assert_eq!(artifact.state, FileArtifactState::Removed);
    assert!(!partial.exists());
    assert!(!path.exists());
    assert_eq!(std::fs::read(&other).unwrap(), b"unrelated-canary");
}

#[test]
fn flush_sync_and_rename_faults_never_publish_or_claim_complete() {
    let directory = tempfile::tempdir().unwrap();
    for point in [
        ExportFaultPoint::BeforeFlush,
        ExportFaultPoint::BeforeSync,
        ExportFaultPoint::BeforeRename,
    ] {
        let path = directory.path().join(format!("{point:?}.json"));
        let (output, _) = create(&path, MovementFormat::Json);
        let partial = output.partial_path().to_owned();
        let mut output = output.with_fault_hook(Arc::new(move |next| {
            if next == point {
                Err(WorkbenchError::Io)
            } else {
                Ok(())
            }
        }));
        output.begin(&columns(), FormulaPolicy::Escape).unwrap();
        output.write_row(&row()).unwrap();
        if point == ExportFaultPoint::BeforeRename {
            output.seal().unwrap();
            assert_eq!(output.finalize(), Err(WorkbenchError::Io));
        } else {
            assert_eq!(output.seal(), Err(WorkbenchError::Io));
            assert!(output.finalize().is_err());
        }
        assert!(!path.exists());
        assert!(partial.exists());
        let artifact = output.artifact(Completion::Complete).unwrap();
        assert_eq!(artifact.state, FileArtifactState::Partial);
        assert!(!artifact.finalized);
    }
}

#[test]
fn post_rename_failure_is_unconfirmed_and_cannot_delete_the_final_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("unknown.tsv");
    let (output, _) = create(&path, MovementFormat::Tsv);
    let partial = output.partial_path().to_owned();
    let mut output = output.with_fault_hook(Arc::new(|point| {
        if point == ExportFaultPoint::AfterRename {
            Err(WorkbenchError::Io)
        } else {
            Ok(())
        }
    }));
    output.begin(&columns(), FormulaPolicy::Escape).unwrap();
    output.write_row(&row()).unwrap();
    output.seal().unwrap();
    assert_eq!(output.finalize(), Err(WorkbenchError::Io));
    let artifact = output.artifact(Completion::Interrupted).unwrap();
    assert_eq!(artifact.state, FileArtifactState::Unconfirmed);
    assert!(!artifact.finalized);
    assert!(path.exists());
    assert!(!partial.exists());
    assert!(matches!(
        output.discard(Completion::Interrupted),
        Err(WorkbenchError::Forbidden)
    ));
    assert!(path.exists());
}

#[test]
fn structural_json_complete_is_separate_from_file_finalization_and_preserves_exact_tags() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("typed.json");
    let (mut output, _) = create(&path, MovementFormat::Json);
    output.begin(&columns(), FormulaPolicy::Escape).unwrap();
    output.write_row(&row()).unwrap();
    output.seal().unwrap();
    assert!(!path.exists());
    let data: serde_json::Value =
        serde_json::from_slice(&std::fs::read(output.partial_path()).unwrap()).unwrap();
    assert_eq!(data["complete"], true);
    assert_eq!(data["rows"][0][0]["kind"], "u64");
    assert_eq!(data["rows"][0][0]["value"], "18446744073709551615");
    assert_eq!(
        data["rows"][0][1]["value"],
        "12345678901234567890.000000001"
    );
    assert!(!output.artifact(Completion::Partial).unwrap().finalized);
}

#[test]
fn unproven_large_preview_is_refused_and_native_staging_charge_is_bounded() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("preview.csv");
    let (mut output, budget) = create(&path, MovementFormat::Csv);
    assert_eq!(budget.retained_bytes(), EXPORT_STAGING_BYTES);
    output.begin(&columns(), FormulaPolicy::Escape).unwrap();
    let mut values = row();
    values[0] = TypedCell::Large {
        value_handle: Uuid::new_v4(),
        type_name: "blob".into(),
        byte_length: 1000000.into(),
        preview: "preview-is-not-source-canary".into(),
    };
    assert_eq!(output.write_row(&values), Err(WorkbenchError::Unsupported));
    assert!(output.seal().is_err());
    assert!(!std::fs::read_to_string(output.partial_path())
        .unwrap()
        .contains("preview-is-not-source-canary"));
    assert!(!path.exists());
    assert!(budget.peak_bytes() <= EXPORT_STAGING_BYTES);
    drop(output);
    assert_eq!(budget.retained_bytes(), 0);
}

#[test]
fn positive_file_phases_follow_flush_sync_rename_order_and_no_auto_finalize_on_drop() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("dropped.csv");
    let phases = Arc::new(Mutex::new(Vec::new()));
    let observed = phases.clone();
    let (output, _) = create(&path, MovementFormat::Csv);
    let mut output = output.with_fault_hook(Arc::new(move |point| {
        observed.lock().unwrap().push(point);
        Ok(())
    }));
    let partial = output.partial_path().to_owned();
    output.begin(&columns(), FormulaPolicy::Escape).unwrap();
    output.write_row(&row()).unwrap();
    output.seal().unwrap();
    assert_eq!(
        *phases.lock().unwrap(),
        [ExportFaultPoint::BeforeFlush, ExportFaultPoint::BeforeSync]
    );
    drop(output);
    assert!(partial.exists());
    assert!(!path.exists());
}

#[test]
fn journaled_partial_cleanup_checks_opened_identity_and_refuses_reparse_or_final_names() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("final.csv");
    let (output, budget) = create(&path, MovementFormat::Csv);
    let partial = output.partial_path().to_owned();
    let identity = output.identity().to_owned();
    drop(output);
    assert_eq!(
        remove_recorded_partial(&partial, "wrong-identity", budget.clone()),
        Err(WorkbenchError::Forbidden)
    );
    assert!(partial.exists());
    assert!(remove_recorded_partial(&partial, &identity, budget.clone()).unwrap());
    assert!(!partial.exists());
    assert!(!remove_recorded_partial(&partial, &identity, budget.clone()).unwrap());
    std::fs::write(&path, b"preserve-final-canary").unwrap();
    assert_eq!(
        remove_recorded_partial(&path, &identity, budget.clone()),
        Err(WorkbenchError::Forbidden)
    );
    std::os::windows::fs::symlink_file(&path, &partial).unwrap();
    assert!(remove_recorded_partial(&partial, &identity, budget.clone()).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"preserve-final-canary");
    assert_eq!(budget.retained_bytes(), 0);
}
