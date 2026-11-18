mod support;
use async_trait::async_trait;
use restoreplane_domain::workbench::*;
use restoreplane_domain::workbench_metadata::{CatalogKind, PageInput};
use restoreplane_domain::workbench_movement::*;
use restoreplane_workbench::data_movement::{
    FileCapabilities, ImportCoordinator, ImportPreviewInput, NativeSourceSelection,
};
use restoreplane_workbench::driver::{
    DriverCancel, DriverConnection, DriverFactory, DriverFacts, DriverLogin, DriverSummary,
    NativeDriverFactory, QueryLimits, RowSink, TransactionCommand,
};
use restoreplane_workbench::generated_sql::WriteCertification;
use restoreplane_workbench::metadata::MetadataCatalog;
use restoreplane_workbench::WorkbenchError;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use uuid::Uuid;

struct Harness {
    fixture: support::QueryFixture,
    files: Arc<FileCapabilities>,
    imports: Arc<ImportCoordinator>,
    metadata: Arc<MetadataCatalog>,
    _source: Option<support::Fixture>,
}

/// Actual SQLite COMMIT is forwarded before this explicit client ACK fault.
/// Synthetic principal/fault seams do not prove OS-medium IPC or packet loss.
struct CommitFault {
    commits: AtomicUsize,
    inserts: AtomicUsize,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    discard_ack: bool,
}
impl CommitFault {
    fn new(discard_ack: bool) -> Arc<Self> {
        Arc::new(Self {
            commits: AtomicUsize::new(0),
            inserts: AtomicUsize::new(0),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            discard_ack,
        })
    }
}
struct FaultFactory(Arc<CommitFault>);
struct FaultConnection {
    inner: Box<dyn DriverConnection>,
    fault: Arc<CommitFault>,
}
#[async_trait]
impl DriverFactory for FaultFactory {
    async fn connect(
        &self,
        descriptor: &ConnectDescriptor,
        login: DriverLogin,
        mode: Mode,
    ) -> Result<Box<dyn DriverConnection>, WorkbenchError> {
        Ok(Box::new(FaultConnection {
            inner: NativeDriverFactory::default()
                .connect(descriptor, login, mode)
                .await?,
            fault: self.0.clone(),
        }))
    }
}
#[async_trait]
impl DriverConnection for FaultConnection {
    fn facts(&self) -> &DriverFacts {
        self.inner.facts()
    }
    fn cancel_handle(&self) -> Arc<dyn DriverCancel> {
        self.inner.cancel_handle()
    }
    async fn set_read_only(&mut self, value: bool) -> Result<(), WorkbenchError> {
        self.inner.set_read_only(value).await
    }
    async fn transaction_read_only(&mut self) -> Result<bool, WorkbenchError> {
        self.inner.transaction_read_only().await
    }
    async fn configure_limits(&mut self, value: Duration) -> Result<(), WorkbenchError> {
        self.inner.configure_limits(value).await
    }
    async fn execute(
        &mut self,
        sql: &str,
        limits: QueryLimits,
        sink: &mut dyn RowSink,
    ) -> Result<DriverSummary, WorkbenchError> {
        self.inner.execute(sql, limits, sink).await
    }
    async fn execute_bound(
        &mut self,
        sql: &str,
        parameters: &[TypedCell],
        limits: QueryLimits,
        sink: &mut dyn RowSink,
    ) -> Result<DriverSummary, WorkbenchError> {
        if sql.starts_with("INSERT ") {
            self.fault.inserts.fetch_add(1, Ordering::SeqCst);
        }
        self.inner
            .execute_bound(sql, parameters, limits, sink)
            .await
    }
    async fn transaction_control(
        &mut self,
        command: TransactionCommand,
    ) -> Result<(), WorkbenchError> {
        self.inner.transaction_control(command).await?;
        if matches!(command, TransactionCommand::Commit)
            && self.fault.commits.fetch_add(1, Ordering::SeqCst) == 0
        {
            self.fault.entered.notify_one();
            self.fault.release.notified().await;
            if self.fault.discard_ack {
                return Err(WorkbenchError::CancelUnsettled);
            }
        }
        Ok(())
    }
    async fn close(&mut self) -> Result<(), WorkbenchError> {
        self.inner.close().await
    }
}

async fn start_rows(h: &Harness, path: &std::path::Path, rows: usize) -> Admission {
    let mut body = String::from("id,value\n");
    for id in 1..=rows {
        body.push_str(&format!("{id},owned-import-{id}\n"));
    }
    std::fs::write(path, body).unwrap();
    let preview = h
        .imports
        .preview(
            &h.fixture.database.context,
            h.input(path, MovementFormat::Csv).await,
        )
        .await
        .unwrap();
    let input = ImportStartInput {
        session: h.fixture.database.session,
        preview_id: preview.preview_id,
        confirmed_warnings: preview.warnings,
    };
    let token = h
        .imports
        .prepare_start(&h.fixture.database.context, &input)
        .await
        .unwrap();
    h.imports
        .start(&h.fixture.database.context, &token.attempt_token, input)
        .await
        .map_err(|error| error.error)
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_child_commit_ack_loss_stops_next_and_acknowledgment_cannot_resume() {
    let fault = CommitFault::new(true);
    let h = Harness::with_commit_fault(fault.clone()).await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("source.csv");
    let receipt = h
        .fixture
        .database
        .runtime
        .binding_receipt(
            &h.fixture.database.context,
            h.fixture.database.session,
            AuthorizePurpose::Mutate,
        )
        .await
        .unwrap();
    let admitted = start_rows(&h, &path, 251).await;
    tokio::time::timeout(Duration::from_secs(30), fault.entered.notified())
        .await
        .unwrap();
    assert_eq!(
        h.count(),
        200,
        "independent observer sees actual first COMMIT"
    );
    let (key, source_digest, batch_digest, byte_start, byte_end, row_start, row_end): (String,String,String,i64,i64,i64,i64) =
        h.fixture.store.read(|connection| connection.query_row(
            "SELECT idempotency_key,source_digest,batch_digest,source_byte_start,source_byte_end,source_row_start,source_row_end FROM operations WHERE parent_operation_id=?1 AND batch_index=0",
            [admitted.operation_id.to_string()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?)))
            .map_err(|_| WorkbenchError::StoreUnavailable)).unwrap();
    let (byte_start, byte_end, row_start, row_end) = (
        u64::try_from(byte_start).unwrap(),
        u64::try_from(byte_end).unwrap(),
        u64::try_from(row_start).unwrap(),
        u64::try_from(row_end).unwrap(),
    );
    let exact_payload = serde_json::json!({"parentOperationId":admitted.operation_id,"batchIndex":0,"sourceDigest":source_digest,"batchDigest":batch_digest,"byteStart":byte_start.to_string(),"byteEnd":byte_end.to_string(),"rowStart":row_start.to_string(),"rowEnd":row_end.to_string()});
    let exact_spec = restoreplane_workbench::journal::AdmissionSpec {
        kind: OperationKind::ImportBatch,
        session: Some(h.fixture.database.session),
        transaction_id: None,
        statements: vec![restoreplane_workbench::journal::StatementSpec {
            digest: batch_digest.clone(),
            effect_class: EffectClass::Write,
            certified_read_only: false,
            transaction_id: None,
        }],
        batch: Some(restoreplane_workbench::journal::ImportBatch {
            parent_operation_id: admitted.operation_id,
            batch_index: 0,
            source_digest: source_digest.clone(),
            batch_digest,
            byte_start,
            byte_end,
            row_start,
            row_end,
        }),
    };
    let same = h
        .fixture
        .journal
        .admit(
            &h.fixture.database.context,
            &key,
            "import.continue",
            &exact_payload,
            &receipt,
            None,
            exact_spec.clone(),
        )
        .unwrap();
    assert!(same.deduplicated);
    let mut changed_range = exact_payload.clone();
    changed_range["rowEnd"] = serde_json::Value::String((row_end + 1).to_string());
    assert_eq!(
        h.fixture
            .journal
            .admit(
                &h.fixture.database.context,
                &key,
                "import.continue",
                &changed_range,
                &receipt,
                None,
                exact_spec
            )
            .err(),
        Some(WorkbenchError::IdempotencyConflict)
    );
    // A fresh native key cannot rebind the actual child's parent/index to
    // another batch identity while its COMMIT response remains withheld.
    let changed_payload = serde_json::json!({"parentOperationId":admitted.operation_id,"batchIndex":0,"batchDigest":"f".repeat(64)});
    let changed_token = h
        .fixture
        .journal
        .prepare(
            &h.fixture.database.context,
            &receipt,
            Some(h.fixture.database.session),
            "import.continue",
            &changed_payload,
        )
        .unwrap();
    let changed = h.fixture.journal.admit(
        &h.fixture.database.context,
        &changed_token.attempt_token,
        "import.continue",
        &changed_payload,
        &receipt,
        None,
        restoreplane_workbench::journal::AdmissionSpec {
            kind: OperationKind::ImportBatch,
            session: Some(h.fixture.database.session),
            transaction_id: None,
            statements: vec![restoreplane_workbench::journal::StatementSpec {
                digest: "f".repeat(64),
                effect_class: EffectClass::Write,
                certified_read_only: false,
                transaction_id: None,
            }],
            batch: Some(restoreplane_workbench::journal::ImportBatch {
                parent_operation_id: admitted.operation_id,
                batch_index: 0,
                source_digest: "d".repeat(64),
                batch_digest: "f".repeat(64),
                byte_start: 0,
                byte_end: 1,
                row_start: 0,
                row_end: 1,
            }),
        },
    );
    assert_eq!(changed.err(), Some(WorkbenchError::IdempotencyConflict));
    fault.release.notify_one();
    let operation = h.terminal(admitted.operation_id).await;
    assert_eq!(operation.state, OperationState::OutcomeUnknown);
    assert_eq!(fault.inserts.load(Ordering::SeqCst), 1);
    assert_eq!(fault.commits.load(Ordering::SeqCst), 1);
    let ctx = &h.fixture.database.context;
    let scope = &h.fixture.database.scope;
    let status = h
        .fixture
        .journal
        .movement_status(
            ctx,
            scope,
            operation.operation_id,
            PageInput {
                cursor: None,
                limit: 100,
            },
        )
        .unwrap();
    assert_eq!(status.progress.confirmed_committed_rows.value(), 0);
    assert_eq!(status.progress.unresolved_batches.value(), 1);
    assert_eq!(status.children.items.len(), 1);
    let child = h
        .fixture
        .journal
        .get(ctx, scope, status.children.items[0].operation_id)
        .unwrap();
    h.fixture
        .journal
        .acknowledge_unknown(ctx, scope, child.operation_id, child.version.value())
        .unwrap();
    let parent = h
        .fixture
        .journal
        .get(ctx, scope, operation.operation_id)
        .unwrap();
    h.fixture
        .journal
        .acknowledge_unknown(ctx, scope, parent.operation_id, parent.version.value())
        .unwrap();
    assert_eq!(
        h.fixture
            .journal
            .import_next_unsent(ctx, scope, parent.operation_id)
            .err(),
        Some(WorkbenchError::OutcomeUnknown)
    );
    assert_eq!(h.count(), 200);
    assert_eq!(fault.inserts.load(Ordering::SeqCst), 1);
    let parent_id = parent.operation_id;
    let context = ctx.clone();
    let scope = scope.clone();
    let Harness {
        fixture,
        imports,
        files,
        metadata,
        _source,
    } = h;
    drop(imports);
    drop(files);
    drop(metadata);
    let support::QueryFixture {
        coordinator,
        streams,
        journal,
        store,
        journal_root,
        database,
    } = fixture;
    drop(coordinator);
    drop(streams);
    drop(journal);
    drop(store);
    struct ReopenRoot;
    impl restoreplane_workbench::store::UserRootPolicy for ReopenRoot {
        fn ensure_private_root(&self, _: &std::path::Path) -> Result<(), WorkbenchError> {
            Ok(())
        }
    }
    let reopened_store = Arc::new(
        restoreplane_workbench::store::ClientStore::open(
            journal_root.path(),
            context.principal(),
            &ReopenRoot,
        )
        .unwrap(),
    );
    let reopened = restoreplane_workbench::journal::OperationJournal::open(reopened_store).unwrap();
    assert_eq!(
        reopened.get(&context, &scope, parent_id).unwrap().state,
        OperationState::OutcomeUnknown
    );
    assert_eq!(
        reopened
            .import_next_unsent(&context, &scope, parent_id)
            .err(),
        Some(WorkbenchError::OutcomeUnknown)
    );
    let observed: i64 = rusqlite::Connection::open(&database.path)
        .unwrap()
        .query_row("SELECT count(*) FROM imported", [], |row| row.get(0))
        .unwrap();
    assert_eq!(observed, 200);
    assert_eq!(fault.inserts.load(Ordering::SeqCst), 1);
    eprintln!("M3 boundary_violations=0; actual_sqlite_commit_rows=200; discarded_client_ack=true; child_dispatches=1; changed_child_identity_rejected=true; next_batch_dispatches=0; acknowledged_unknown_resumable=false; reopened_auto_replays=0; packet_loss=false; os_standard_integrity_e2e=false");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(windows)] // Continuation incarnation proof is certified only on local NTFS.
async fn cancellation_after_positive_commit_preserves_prior_batch_and_continues_unsent_only() {
    let fault = CommitFault::new(false);
    let mut h = Harness::with_commit_fault(fault.clone()).await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("source.csv");
    let revision = h
        .fixture
        .database
        .runtime
        .binding_receipt(
            &h.fixture.database.context,
            h.fixture.database.session,
            AuthorizePurpose::Read,
        )
        .await
        .unwrap()
        .descriptor_revision;
    let admitted = start_rows(&h, &path, 251).await;
    tokio::time::timeout(Duration::from_secs(30), fault.entered.notified())
        .await
        .unwrap();
    assert_eq!(h.count(), 200);
    assert!(h
        .imports
        .cancel(&h.fixture.database.context, admitted.operation_id)
        .unwrap());
    fault.release.notify_one();
    let terminal = h.terminal(admitted.operation_id).await;
    assert_eq!(terminal.state, OperationState::Partial);
    assert_eq!(h.count(), 200);
    assert_eq!(fault.inserts.load(Ordering::SeqCst), 1);
    // The production reconnect coordinator must recover the native basis of
    // this import-only retired physical connection, without query/manual caches.
    let old = h.fixture.database.session;
    let other_window = restoreplane_workbench::InvocationContext::new(
        h.fixture.database.context.principal().clone(),
        Uuid::new_v4(),
    );
    assert!(matches!(
        h.fixture
            .coordinator
            .reconnect(&other_window, old, &revision)
            .await,
        Err(WorkbenchError::SessionStale | WorkbenchError::Forbidden)
    ));
    let changed_generation = SessionRef {
        connection_generation: (old.connection_generation.value() + 1).into(),
        ..old
    };
    assert!(matches!(
        h.fixture
            .coordinator
            .reconnect(&h.fixture.database.context, changed_generation, &revision)
            .await,
        Err(WorkbenchError::SessionStale | WorkbenchError::Forbidden)
    ));
    let session = h
        .fixture
        .coordinator
        .reconnect(&h.fixture.database.context, old, &revision)
        .await
        .unwrap()
        .session;
    assert_ne!(session, old);
    h.fixture.database.session = session;
    let preview = h
        .imports
        .preview(
            &h.fixture.database.context,
            h.input(&path, MovementFormat::Csv).await,
        )
        .await
        .unwrap();
    let continued = h
        .imports
        .continue_review(
            &h.fixture.database.context,
            session,
            admitted.operation_id,
            preview.preview_id,
        )
        .await
        .unwrap();
    assert_eq!(continued.confirmed_committed_rows.value(), 200);
    assert_eq!(
        continued.next_unsent.as_ref().unwrap().row_start.value(),
        200
    );
    let input = ImportContinueInput {
        session,
        parent_operation_id: admitted.operation_id,
        review_id: continued.review_id,
    };
    let token = h
        .imports
        .prepare_continue(&h.fixture.database.context, &input)
        .await
        .unwrap();
    let resumed = h
        .imports
        .continue_import(
            &h.fixture.database.context,
            &token.attempt_token,
            input.clone(),
        )
        .await
        .map_err(|error| error.error)
        .unwrap();
    assert_eq!(resumed.operation_id, admitted.operation_id);
    assert_eq!(
        h.terminal(admitted.operation_id).await.state,
        OperationState::Completed
    );
    assert_eq!(h.count(), 251);
    assert_eq!(fault.inserts.load(Ordering::SeqCst), 2);
    let duplicate = h
        .imports
        .continue_import(
            &h.fixture.database.context,
            &token.attempt_token,
            input.clone(),
        )
        .await
        .map_err(|error| error.error)
        .unwrap();
    assert!(duplicate.deduplicated);
    assert_eq!(h.count(), 251);
    let changed = ImportContinueInput {
        parent_operation_id: Uuid::new_v4(),
        ..input
    };
    assert_eq!(
        h.imports
            .continue_import(&h.fixture.database.context, &token.attempt_token, changed)
            .await
            .err()
            .unwrap()
            .error,
        WorkbenchError::IdempotencyConflict
    );
    assert_eq!(fault.inserts.load(Ordering::SeqCst), 2);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admitted_parent_without_a_source_settles_failed_without_target_dispatch() {
    let h = Harness::new().await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("source.csv");
    std::fs::write(&path, "id,value\n1,value\n").unwrap();
    let preview_input = h.input(&path, MovementFormat::Csv).await;
    let handle = preview_input.source_handle;
    let preview = h
        .imports
        .preview(&h.fixture.database.context, preview_input)
        .await
        .unwrap();
    let input = ImportStartInput {
        session: h.fixture.database.session,
        preview_id: preview.preview_id,
        confirmed_warnings: preview.warnings,
    };
    let attempt = h
        .imports
        .prepare_start(&h.fixture.database.context, &input)
        .await
        .unwrap();
    assert!(h
        .files
        .release(&h.fixture.database.context, handle)
        .unwrap());
    let error = h
        .imports
        .start(
            &h.fixture.database.context,
            &attempt.attempt_token,
            input.clone(),
        )
        .await
        .err()
        .unwrap();
    let id = error
        .operation_id
        .expect("admitted operation remains inspectable");
    let operation = h
        .fixture
        .journal
        .get(&h.fixture.database.context, &h.fixture.database.scope, id)
        .unwrap();
    assert_eq!(operation.state, OperationState::Failed);
    assert_eq!(operation.transaction_outcome, TransactionOutcome::None);
    assert_eq!(h.count(), 0);
    let dispatch: i64 = h
        .fixture
        .store
        .read(|connection| {
            connection
                .query_row(
                    "SELECT dispatch_count FROM operations WHERE operation_id=?1",
                    [id.to_string()],
                    |row| row.get(0),
                )
                .map_err(|_| WorkbenchError::StoreUnavailable)
        })
        .unwrap();
    assert_eq!(dispatch, 0);
    h.imports
        .retire_session(&h.fixture.database.context, input.session)
        .unwrap();
    let duplicate = h
        .imports
        .start(
            &h.fixture.database.context,
            &attempt.attempt_token,
            input.clone(),
        )
        .await
        .map_err(|error| error.error)
        .unwrap();
    assert_eq!(duplicate.operation_id, id);
    assert!(duplicate.deduplicated);
    assert_eq!(h.count(), 0);
    let changed = ImportStartInput {
        preview_id: Uuid::new_v4(),
        ..input.clone()
    };
    assert_eq!(
        h.imports
            .start(&h.fixture.database.context, &attempt.attempt_token, changed)
            .await
            .err()
            .unwrap()
            .error,
        WorkbenchError::IdempotencyConflict
    );
    let public = serde_json::to_value(&input).unwrap();
    assert!(h
        .fixture
        .journal
        .query_status(
            &h.fixture.database.context,
            &attempt.attempt_token,
            &public,
            input.session
        )
        .is_err());
    assert!(h
        .fixture
        .journal
        .continuation_status(
            &h.fixture.database.context,
            &attempt.attempt_token,
            &public,
            input.session
        )
        .is_err());
    assert_eq!(h.count(), 0);
}
impl Harness {
    async fn new() -> Self {
        Self::from_database(
            support::Fixture::sqlite_mode(
                "CREATE TABLE imported(id INTEGER PRIMARY KEY,value TEXT NOT NULL)",
                Mode::ReadWrite,
            )
            .await,
        )
        .await
    }
    async fn from_database(database: support::Fixture) -> Self {
        let fixture = support::QueryFixture::new(database);
        let metadata = Arc::new(MetadataCatalog::new(
            fixture.database.runtime.clone(),
            fixture.streams.budget(),
        ));
        let files = Arc::new(FileCapabilities::new(
            fixture.database.runtime.clone(),
            fixture.streams.budget(),
        ));
        let facts = fixture
            .database
            .runtime
            .with_connection(
                &fixture.database.context,
                fixture.database.session,
                AuthorizePurpose::Read,
                |connection| Box::pin(async move { Ok(connection.facts().clone()) }),
            )
            .await
            .unwrap();
        // Native fixture certification only; this is not OS-medium IPC proof
        // and does not promote a production matrix before actual execution.
        let certification = WriteCertification::from_verified_build(
            "section07-import-owned-fixture-candidate".into(),
            vec![(facts.engine, facts.server_version)],
        )
        .unwrap();
        let imports = Arc::new(ImportCoordinator::new(
            fixture.database.runtime.clone(),
            metadata.clone(),
            files.clone(),
            fixture.journal.clone(),
            Some(certification),
        ));
        Self {
            fixture,
            files,
            imports,
            metadata,
            _source: None,
        }
    }
    async fn with_commit_fault(fault: Arc<CommitFault>) -> Self {
        let source = support::Fixture::sqlite_mode(
            "CREATE TABLE imported(id INTEGER PRIMARY KEY,value TEXT NOT NULL)",
            Mode::ReadWrite,
        )
        .await;
        let descriptor = source
            .runtime
            .binding_receipt(&source.context, source.session, AuthorizePurpose::Read)
            .await
            .unwrap()
            .descriptor;
        let mut database = support::Fixture::connect_with_factory(
            descriptor,
            String::new(),
            zeroize::Zeroizing::new(String::new()),
            Mode::ReadWrite,
            Arc::new(FaultFactory(fault)),
        )
        .await;
        database.path = source.path.clone();
        let mut harness = Self::from_database(database).await;
        harness._source = Some(source);
        harness
    }
    async fn input(&self, path: &std::path::Path, format: MovementFormat) -> ImportPreviewInput {
        let ctx = &self.fixture.database.context;
        let session = self.fixture.database.session;
        let handle = self
            .files
            .register_source(
                ctx,
                session,
                NativeSourceSelection::from_native_dialog(path).unwrap(),
            )
            .unwrap();
        let source = self
            .files
            .source_columns(ctx, session, handle, format)
            .unwrap();
        assert_eq!(source.columns.len(), 2);
        let object = self
            .metadata
            .page(
                ctx,
                session,
                None,
                CatalogKind::Relations,
                "imported".into(),
                PageInput {
                    cursor: None,
                    limit: 100,
                },
            )
            .await
            .unwrap()
            .items
            .into_iter()
            .find(|item| item.name == "imported")
            .unwrap()
            .object;
        let relation = self
            .metadata
            .resolve_relation(ctx, session, &object)
            .await
            .unwrap();
        let mut mapping = BTreeMap::new();
        mapping.insert(
            "c0".into(),
            relation
                .columns
                .iter()
                .find(|column| column.column.name == "id")
                .unwrap()
                .column
                .column_id,
        );
        mapping.insert(
            "c1".into(),
            relation
                .columns
                .iter()
                .find(|column| column.column.name == "value")
                .unwrap()
                .column
                .column_id,
        );
        ImportPreviewInput {
            session,
            object,
            source_handle: handle,
            format,
            mapping,
            page: PageInput {
                cursor: None,
                limit: 100,
            },
        }
    }
    fn count(&self) -> i64 {
        rusqlite::Connection::open(&self.fixture.database.path)
            .unwrap()
            .query_row("SELECT count(*) FROM imported", [], |row| row.get(0))
            .unwrap()
    }
    async fn terminal(&self, id: Uuid) -> Operation {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let operation = self
                    .fixture
                    .journal
                    .get(
                        &self.fixture.database.context,
                        &self.fixture.database.scope,
                        id,
                    )
                    .unwrap();
                if operation.state.is_terminal() {
                    break operation;
                }
                tokio::task::yield_now().await
            }
        })
        .await
        .unwrap()
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn csv_whole_preflight_then_independent_batches_and_same_token_deduplicate() {
    let h = Harness::new().await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("source.csv");
    let mut body = String::from("id,value\r\n");
    for id in 1..=251 {
        body.push_str(&format!("{id},canary_import_value_{id}\r\n"));
    }
    std::fs::write(&path, body).unwrap();
    let preview = h
        .imports
        .preview(
            &h.fixture.database.context,
            h.input(&path, MovementFormat::Csv).await,
        )
        .await
        .unwrap();
    assert_eq!(h.count(), 0);
    assert!(preview.rows.items.len() <= 100);
    let input = ImportStartInput {
        session: h.fixture.database.session,
        preview_id: preview.preview_id,
        confirmed_warnings: preview.warnings,
    };
    let attempt = h
        .imports
        .prepare_start(&h.fixture.database.context, &input)
        .await
        .unwrap();
    let admission = h
        .imports
        .start(
            &h.fixture.database.context,
            &attempt.attempt_token,
            input.clone(),
        )
        .await
        .map_err(|error| error.error)
        .unwrap();
    let operation = h.terminal(admission.operation_id).await;
    assert_eq!(operation.state, OperationState::Completed);
    assert_eq!(h.count(), 251);
    let duplicate = h
        .imports
        .start(&h.fixture.database.context, &attempt.attempt_token, input)
        .await
        .map_err(|error| error.error)
        .unwrap();
    assert!(duplicate.deduplicated);
    assert_eq!(duplicate.operation_id, admission.operation_id);
    assert_eq!(h.count(), 251);
    let metadata = h
        .fixture
        .journal
        .movement_metadata(
            &h.fixture.database.context,
            &h.fixture.database.scope,
            admission.operation_id,
        )
        .unwrap()
        .unwrap();
    let MovementMetadata::Import { progress, .. } = metadata else {
        panic!("expected import metadata")
    };
    assert_eq!(progress.confirmed_committed_rows.value(), 251);
    assert_eq!(progress.unresolved_batches.value(), 0);
    assert!(progress.confirmed_committed_batches.value() >= 2);
    for name in ["client-state.sqlite3", "client-state.sqlite3-wal"] {
        if let Ok(bytes) = std::fs::read(h.fixture.store.root().join(name)) {
            assert!(!bytes
                .windows(b"canary_import_value".len())
                .any(|window| window == b"canary_import_value"));
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_late_row_is_rejected_before_any_child_send() {
    let h = Harness::new().await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("bad.csv");
    let mut body = String::from("id,value\n");
    for id in 1..=250 {
        body.push_str(&format!("{id},valid\n"));
    }
    body.push_str("not-an-integer,late-invalid\n");
    std::fs::write(&path, body).unwrap();
    let result = h
        .imports
        .preview(
            &h.fixture.database.context,
            h.input(&path, MovementFormat::Csv).await,
        )
        .await;
    assert!(matches!(result, Err(WorkbenchError::InvalidInput)));
    assert_eq!(h.count(), 0);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_and_preview_are_bound_to_owner_window_and_physical_generation() {
    let h = Harness::new().await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("source.csv");
    std::fs::write(&path, "id,value\n1,value\n").unwrap();
    let input = h.input(&path, MovementFormat::Csv).await;
    let preview = h
        .imports
        .preview(&h.fixture.database.context, input.clone())
        .await
        .unwrap();
    let other = restoreplane_workbench::InvocationContext::new(
        h.fixture.database.context.principal().clone(),
        Uuid::new_v4(),
    );
    assert!(matches!(
        h.files.source_columns(
            &other,
            input.session,
            input.source_handle,
            MovementFormat::Csv
        ),
        Err(WorkbenchError::Forbidden | WorkbenchError::SessionStale)
    ));
    let prepared = h
        .imports
        .prepare_start(
            &other,
            &ImportStartInput {
                session: input.session,
                preview_id: preview.preview_id,
                confirmed_warnings: preview.warnings,
            },
        )
        .await;
    assert!(matches!(prepared, Err(WorkbenchError::Forbidden)));
    assert_eq!(h.count(), 0);
}
