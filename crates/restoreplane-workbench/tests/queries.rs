mod support;
use async_trait::async_trait;
use restoreplane_domain::workbench::*;
use restoreplane_domain::workbench_metadata::{Limits, PageInput};
use restoreplane_domain::workbench_query::*;
use restoreplane_workbench::driver::{
    DriverConnection, DriverFactory, DriverLogin, DriverSummary, NativeDriverFactory, QueryLimits,
    RowSink, TransactionCommand,
};
use restoreplane_workbench::WorkbenchError;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
const CACHE_SQL: &str = "SELECT 42 /* native cache lifetime scenario */";
struct CountFactory {
    calls: Arc<AtomicUsize>,
    discard_control_ack: Option<ControlAck>,
}
struct CountConnection {
    inner: Box<dyn DriverConnection>,
    calls: Arc<AtomicUsize>,
    discard_control_ack: Option<ControlAck>,
}
#[derive(Clone, Copy)]
enum ControlAck {
    Begin,
    Rollback,
}
#[async_trait]
impl DriverFactory for CountFactory {
    async fn connect(
        &self,
        descriptor: &ConnectDescriptor,
        login: DriverLogin,
        mode: Mode,
    ) -> Result<Box<dyn DriverConnection>, WorkbenchError> {
        Ok(Box::new(CountConnection {
            inner: NativeDriverFactory::default()
                .connect(descriptor, login, mode)
                .await?,
            calls: self.calls.clone(),
            discard_control_ack: self.discard_control_ack,
        }))
    }
}
#[async_trait]
impl DriverConnection for CountConnection {
    fn facts(&self) -> &restoreplane_workbench::driver::DriverFacts {
        self.inner.facts()
    }
    fn cancel_handle(&self) -> Arc<dyn restoreplane_workbench::driver::DriverCancel> {
        self.inner.cancel_handle()
    }
    async fn set_read_only(&mut self, value: bool) -> Result<(), WorkbenchError> {
        self.inner.set_read_only(value).await
    }
    async fn transaction_read_only(&mut self) -> Result<bool, WorkbenchError> {
        self.inner.transaction_read_only().await
    }
    async fn configure_limits(&mut self, value: std::time::Duration) -> Result<(), WorkbenchError> {
        self.inner.configure_limits(value).await
    }
    async fn transaction_control(
        &mut self,
        command: TransactionCommand,
    ) -> Result<(), WorkbenchError> {
        self.inner.transaction_control(command).await?;
        if matches!(
            (self.discard_control_ack, command),
            (
                Some(ControlAck::Begin),
                TransactionCommand::ManualBegin { .. }
            ) | (Some(ControlAck::Rollback), TransactionCommand::Rollback)
        ) {
            return Err(WorkbenchError::CancelUnsettled);
        }
        Ok(())
    }
    async fn execute(
        &mut self,
        sql: &str,
        limits: QueryLimits,
        sink: &mut dyn RowSink,
    ) -> Result<DriverSummary, WorkbenchError> {
        if sql == CACHE_SQL {
            self.calls.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.execute(sql, limits, sink).await
    }
    async fn execute_bound(
        &mut self,
        sql: &str,
        parameters: &[TypedCell],
        limits: QueryLimits,
        sink: &mut dyn RowSink,
    ) -> Result<DriverSummary, WorkbenchError> {
        self.inner
            .execute_bound(sql, parameters, limits, sink)
            .await
    }
    async fn close(&mut self) -> Result<(), WorkbenchError> {
        self.inner.close().await
    }
}
async fn prepare(fixture: &support::QueryFixture, sql: &str) -> (QueryRunInput, String) {
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
    let input = QueryRunInput {
        session: fixture.database.session,
        review_id: review.review_id,
        confirmed_warnings: review.warnings,
        limits: limits(),
    };
    let token = fixture
        .coordinator
        .prepare_query(&fixture.database.context, input.clone())
        .await
        .unwrap();
    (input, token.attempt_token)
}
async fn worker_settled(
    fixture: &support::QueryFixture,
    admission: &QueryAdmission,
) -> Vec<QueryResult> {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let page = fixture
                .coordinator
                .results(
                    &fixture.database.context,
                    QueryResultsInput {
                        session: fixture.database.session,
                        query_run_id: admission.query_run_id,
                        page: PageInput {
                            cursor: None,
                            limit: 100,
                        },
                    },
                )
                .await
                .unwrap();
            if matches!(
                page.count,
                restoreplane_domain::workbench_metadata::Count::Exact { .. }
            ) {
                break page.items;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}
fn acknowledge_result(
    fixture: &support::QueryFixture,
    identity: &restoreplane_domain::workbench_stream::StreamIdentity,
) {
    let mut after = None;
    loop {
        let pull = fixture
            .streams
            .frames(&fixture.database.context, identity, after)
            .unwrap();
        let value = serde_json::to_value(&pull).unwrap();
        let Some(frame) = value["frames"].as_array().unwrap().first() else {
            break;
        };
        let sequence = frame["sequence"].as_str().unwrap().parse::<u64>().unwrap();
        let more = value["hasMore"].as_bool().unwrap();
        pull.mark_sent().unwrap();
        drop(pull);
        let ack = fixture
            .streams
            .ack(
                &fixture.database.context,
                &restoreplane_domain::workbench_stream::Ack {
                    identity: identity.clone(),
                    highest_contiguous_sequence: sequence.into(),
                },
            )
            .unwrap();
        assert_eq!(ack.accepted_through.value(), sequence);
        after = Some(sequence);
        if !more {
            break;
        }
    }
}
#[tokio::test]
async fn one_hundred_twenty_completed_runs_evict_only_acked_data_and_old_token_is_status_only() {
    let source = support::Fixture::sqlite("CREATE TABLE source(id INTEGER PRIMARY KEY)").await;
    let descriptor = source
        .runtime
        .binding_receipt(&source.context, source.session, AuthorizePurpose::Read)
        .await
        .unwrap()
        .descriptor;
    let calls = Arc::new(AtomicUsize::new(0));
    let fixture = support::QueryFixture::new(
        support::Fixture::connect_with_factory(
            descriptor,
            String::new(),
            zeroize::Zeroizing::new(String::new()),
            Mode::ReadOnly,
            Arc::new(CountFactory {
                calls: calls.clone(),
                discard_control_ack: None,
            }),
        )
        .await,
    );
    let mut first = None;
    for index in 0..120 {
        let (input, token) = prepare(&fixture, CACHE_SQL).await;
        let admission = fixture
            .coordinator
            .run(&fixture.database.context, token.clone(), input.clone())
            .await
            .unwrap();
        assert_eq!(admission.query_run_id, admission.admission.operation_id);
        assert!(admission.execution_epoch.value() > 0);
        let results = worker_settled(&fixture, &admission).await;
        assert_eq!(results.len(), 1);
        for result in &results {
            acknowledge_result(&fixture, &result.identity);
        }
        if index == 0 {
            first = Some((input, token, admission, results[0].identity.clone()));
        }
    }
    assert_eq!(calls.load(Ordering::SeqCst), 120);
    let (input, token, first, identity) = first.unwrap();
    assert!(matches!(
        fixture
            .coordinator
            .prepare_query(&fixture.database.context, input.clone())
            .await,
        Err(WorkbenchError::AttemptExpired)
    ));
    assert!(matches!(
        fixture
            .streams
            .page(&fixture.database.context, &identity, 0, 100),
        Err(WorkbenchError::ResultExpired)
    ));
    assert!(matches!(
        fixture
            .coordinator
            .results(
                &fixture.database.context,
                QueryResultsInput {
                    session: fixture.database.session,
                    query_run_id: first.query_run_id,
                    page: PageInput {
                        cursor: None,
                        limit: 100
                    }
                }
            )
            .await,
        Err(WorkbenchError::ResultExpired)
    ));
    let duplicate = fixture
        .coordinator
        .run(&fixture.database.context, token.clone(), input.clone())
        .await
        .unwrap();
    assert!(duplicate.admission.deduplicated);
    assert_eq!(duplicate.query_run_id, first.query_run_id);
    assert_eq!(duplicate.execution_epoch, first.execution_epoch);
    assert_eq!(
        duplicate.admission.operation_id,
        first.admission.operation_id
    );
    assert_eq!(calls.load(Ordering::SeqCst), 120);
    let mut changed = input;
    changed.limits.max_rows -= 1;
    assert!(matches!(
        fixture
            .coordinator
            .run(&fixture.database.context, token, changed)
            .await,
        Err(WorkbenchError::IdempotencyConflict)
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 120);
    let operations = fixture
        .journal
        .list(
            &fixture.database.context,
            &fixture.database.scope,
            None,
            None,
            200,
        )
        .unwrap();
    assert_eq!(operations.len(), 120);
    assert!(
        fixture.streams.budget().retained_bytes()
            <= restoreplane_workbench::result_stream::NATIVE_RESULT_BYTES
    );
}
#[tokio::test]
async fn active_run_capacity_refuses_before_admission_and_reserved_epoch_is_durable() {
    let mut fixture = support::QueryFixture::new(
        support::Fixture::sqlite("CREATE TABLE source(id INTEGER PRIMARY KEY)").await,
    );
    fixture.coordinator = Arc::new(
        restoreplane_workbench::query::QueryCoordinator::new(
            fixture.database.runtime.clone(),
            fixture.journal.clone(),
            fixture.streams.clone(),
        )
        .with_run_cache_capacity(1)
        .unwrap(),
    );
    let(first_input,first_token)=prepare(&fixture,"WITH RECURSIVE x(n) AS (VALUES(0) UNION ALL SELECT n+1 FROM x WHERE n<100000000) SELECT SUM(n) FROM x").await;
    let (second_input, second_token) = prepare(&fixture, "SELECT 7").await;
    let first = fixture
        .coordinator
        .run(&fixture.database.context, first_token, first_input)
        .await
        .unwrap();
    assert_eq!(
        fixture
            .journal
            .execution_epoch(
                &fixture.database.context,
                &fixture.database.scope,
                first.admission.operation_id
            )
            .unwrap(),
        first.execution_epoch.value()
    );
    assert!(matches!(
        fixture
            .coordinator
            .run(&fixture.database.context, second_token, second_input)
            .await,
        Err(WorkbenchError::Limit)
    ));
    assert_eq!(
        fixture
            .journal
            .list(
                &fixture.database.context,
                &fixture.database.scope,
                None,
                None,
                200
            )
            .unwrap()
            .len(),
        1
    );
    fixture
        .coordinator
        .cancel(
            &fixture.database.context,
            QueryCancelInput {
                session: fixture.database.session,
                query_run_id: first.query_run_id,
                execution_epoch: first.execution_epoch,
            },
        )
        .await
        .unwrap();
    worker_settled(&fixture, &first).await;
}
fn limits() -> Limits {
    Limits {
        max_rows: 200,
        max_bytes: 512 * 1024,
        statement_timeout_ms: 30_000,
        lock_wait_ms: 2000,
    }
}
async fn run(fixture: &support::QueryFixture, sql: &str) -> QueryAdmission {
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
    let input = QueryRunInput {
        session: fixture.database.session,
        review_id: review.review_id,
        confirmed_warnings: review.warnings,
        limits: limits(),
    };
    let token = fixture
        .coordinator
        .prepare_query(&fixture.database.context, input.clone())
        .await
        .unwrap();
    fixture
        .coordinator
        .run(&fixture.database.context, token.attempt_token, input)
        .await
        .unwrap()
}
async fn completed(fixture: &support::QueryFixture, id: uuid::Uuid) -> Operation {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let operation = fixture
                .journal
                .get(&fixture.database.context, &fixture.database.scope, id)
                .unwrap();
            if operation.state.is_terminal() {
                break operation;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap()
}
#[tokio::test]
async fn observed_constraint_error_keeps_manual_socket_for_positive_rollback() {
    let fixture = support::QueryFixture::new(
        support::Fixture::sqlite_mode(
            "CREATE TABLE source(id INTEGER PRIMARY KEY)",
            Mode::ReadWrite,
        )
        .await,
    );
    let tx = fixture
        .coordinator
        .transaction_begin(
            &fixture.database.context,
            TransactionBeginInput {
                session: fixture.database.session,
                isolation: "serializable".into(),
                read_only: false,
            },
        )
        .await
        .unwrap();
    let first_run = run(&fixture, "INSERT INTO source VALUES(1)").await;
    let first = completed(&fixture, first_run.admission.operation_id).await;
    assert_eq!(
        first.statements[0].effect_evidence,
        EffectEvidence::PendingTransaction
    );
    // A durable terminal reports DB evidence before the worker necessarily
    // releases its physical FIFO ticket. QueryResults completeness is the
    // existing native settlement observation, rather than a timing delay.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let page = fixture
                .coordinator
                .results(
                    &fixture.database.context,
                    QueryResultsInput {
                        session: fixture.database.session,
                        query_run_id: first_run.query_run_id,
                        page: PageInput {
                            cursor: None,
                            limit: 100,
                        },
                    },
                )
                .await
                .unwrap();
            if page.completeness == restoreplane_domain::workbench_metadata::Completeness::Complete
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let rejected = run(&fixture, "INSERT INTO source VALUES(1)").await;
    let failed = completed(&fixture, rejected.admission.operation_id).await;
    assert_eq!(failed.state, OperationState::Failed);
    assert_eq!(failed.statements[0].outcome, StatementStatus::Failed);
    assert_eq!(
        failed.statements[0].effect_evidence,
        EffectEvidence::PendingTransaction
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if fixture
                .coordinator
                .transaction_get(&fixture.database.context, fixture.database.session)
                .await
                .unwrap()
                .unwrap()
                .state
                == TransactionState::FailedNeedsRollback
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        fixture
            .coordinator
            .review(
                &fixture.database.context,
                QueryReviewInput {
                    session: fixture.database.session,
                    sql: "SELECT 1".into(),
                    purpose: QueryPurpose::Run
                }
            )
            .await
            .err()
            .unwrap(),
        WorkbenchError::TargetBusy
    );
    let rolled = fixture
        .coordinator
        .transaction_rollback(
            &fixture.database.context,
            TransactionInput {
                session: fixture.database.session,
                transaction_id: tx.transaction_id,
            },
        )
        .await
        .unwrap();
    assert_eq!(rolled.state, TransactionState::RolledBack);
    let observer = rusqlite::Connection::open(&fixture.database.path).unwrap();
    assert_eq!(
        observer
            .query_row("SELECT COUNT(*) FROM source", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    let preserved = fixture
        .journal
        .get(
            &fixture.database.context,
            &fixture.database.scope,
            first.operation_id,
        )
        .unwrap();
    assert_eq!(
        preserved.statements[0].effect_evidence,
        EffectEvidence::RolledBack
    );
}

#[tokio::test]
async fn restored_sqlite_identity_is_checked_before_publish_and_again_on_reconnect() {
    use restoreplane_domain::workbench_protection::WorkbenchRestoreProof;
    use restoreplane_workbench::session::RestoredSessionCheck;
    let fixture = support::Fixture::sqlite("CREATE TABLE source(id INTEGER PRIMARY KEY)").await;
    let object_id = restoreplane_source_sqlite::probe_sqlite_file(&fixture.path)
        .unwrap()
        .object_id()
        .to_owned();
    let proof = WorkbenchRestoreProof {
        restore_work_item_id: restoreplane_domain::WorkItemId::new(),
        restore_run_id: restoreplane_domain::RestoreRunId::new(),
        connection_profile_id: restoreplane_domain::ConnectionProfileId::new(),
        database_name: "original restore destination alias".into(),
        engine: Engine::Sqlite,
        server_identity: None,
        actual_database_identity: format!("sqlite-file-object-id:{object_id}"),
        completed_at_ms: restoreplane_workbench::types::now_ms().into(),
    };
    let credential = fixture
        .runtime
        .credential_reference(&fixture.context, fixture.session)
        .unwrap();
    let mut wrong = proof.clone();
    wrong.actual_database_identity = "sqlite-file-object-id:wrong-file".into();
    let check = RestoredSessionCheck::from_authenticated_work(
        fixture.context.principal().clone(),
        fixture.scope.workspace_id.clone(),
        wrong,
    )
    .unwrap();
    assert!(matches!(
        fixture
            .runtime
            .open_checked(
                &fixture.context,
                fixture.scope.clone(),
                credential,
                None,
                check
            )
            .await,
        Err(WorkbenchError::BindingUnproven)
    ));
    assert_eq!(
        fixture
            .runtime
            .list(&fixture.context, &fixture.scope)
            .unwrap()
            .len(),
        1
    );
    let check = RestoredSessionCheck::from_authenticated_work(
        fixture.context.principal().clone(),
        fixture.scope.workspace_id.clone(),
        proof,
    )
    .unwrap();
    let restored = fixture
        .runtime
        .open_checked(
            &fixture.context,
            fixture.scope.clone(),
            credential,
            None,
            check,
        )
        .await
        .unwrap();
    assert_eq!(restored.actual_context.purpose, TargetPurpose::Restored);
    assert!(matches!(
        fixture
            .runtime
            .set_mode(&fixture.context, restored.session, Mode::ReadWrite)
            .await,
        Err(WorkbenchError::Forbidden)
    ));
    fixture
        .runtime
        .close(&fixture.context, fixture.session)
        .await
        .unwrap();
    let coordinator = support::QueryFixture::new(fixture);
    let new = coordinator
        .coordinator
        .reconnect(
            &coordinator.database.context,
            restored.session,
            &"b".repeat(64),
        )
        .await
        .unwrap();
    assert_eq!(new.actual_context.purpose, TargetPurpose::Restored);
    assert_eq!(new.mode, Mode::ReadOnly);
}
#[tokio::test]
async fn exact_original_readonly_runs_result_sets_and_empty_schema_are_real_sqlite() {
    let fixture=support::QueryFixture::new(support::Fixture::sqlite("CREATE TABLE source(id INTEGER PRIMARY KEY,label TEXT);INSERT INTO source VALUES(1,'한국어; literal'),(2,'')").await);
    let sql="-- SQL_CANARY_query_exact\r\nSELECT id,label FROM source WHERE label='한국어; literal'; /* source bytes retained */\n SELECT id,label FROM source WHERE 1=0;";
    let admission = run(&fixture, sql).await;
    assert!(admission.execution_epoch.value() > 0);
    let operation = completed(&fixture, admission.admission.operation_id).await;
    assert_eq!(operation.state, OperationState::Completed);
    assert_eq!(operation.statements.len(), 2);
    assert!(operation
        .statements
        .iter()
        .all(|statement| statement.effect_evidence == EffectEvidence::ReadOnly));
    let results = fixture
        .coordinator
        .results(
            &fixture.database.context,
            QueryResultsInput {
                session: fixture.database.session,
                query_run_id: admission.query_run_id,
                page: PageInput {
                    cursor: None,
                    limit: 100,
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(results.items.len(), 2);
    let page = fixture
        .streams
        .page(
            &fixture.database.context,
            &results.items[0].identity,
            0,
            200,
        )
        .unwrap();
    assert_eq!(page.page().page.items.len(), 1);
    assert_eq!(
        page.page().page.items[0].cells[1],
        TypedCell::Text {
            value: "한국어; literal".into()
        }
    );
    let empty = fixture
        .streams
        .page(
            &fixture.database.context,
            &results.items[1].identity,
            0,
            200,
        )
        .unwrap();
    assert_eq!(empty.page().columns.len(), 2);
    assert!(empty.page().page.items.is_empty());
    for filename in ["client-state.sqlite3", "client-state.sqlite3-wal"] {
        let path = fixture.journal_root.path().join(filename);
        if path.exists() {
            let bytes = std::fs::read(path).unwrap();
            assert!(!bytes
                .windows(b"SQL_CANARY_query_exact".len())
                .any(|part| part == b"SQL_CANARY_query_exact"));
        }
    }
}
#[tokio::test]
async fn readonly_modes_and_immutable_resolved_attempts_fail_before_dispatch() {
    let fixture = support::QueryFixture::new(
        support::Fixture::sqlite("CREATE TABLE source(id INTEGER PRIMARY KEY)").await,
    );
    assert!(fixture
        .coordinator
        .review(
            &fixture.database.context,
            QueryReviewInput {
                session: fixture.database.session,
                sql: "DELETE FROM source".into(),
                purpose: QueryPurpose::Run
            }
        )
        .await
        .is_err());
    let review = fixture
        .coordinator
        .review(
            &fixture.database.context,
            QueryReviewInput {
                session: fixture.database.session,
                sql: "SELECT 1".into(),
                purpose: QueryPurpose::Run,
            },
        )
        .await
        .unwrap();
    let input = QueryRunInput {
        session: fixture.database.session,
        review_id: review.review_id,
        confirmed_warnings: review.warnings,
        limits: limits(),
    };
    let prepared = fixture
        .coordinator
        .prepare_query(&fixture.database.context, input.clone())
        .await
        .unwrap();
    let mut changed = input.clone();
    changed.limits.max_rows = 1;
    assert!(matches!(
        fixture
            .coordinator
            .run(
                &fixture.database.context,
                prepared.attempt_token.clone(),
                changed
            )
            .await,
        Err(WorkbenchError::IdempotencyConflict)
    ));
    let admitted = fixture
        .coordinator
        .run(
            &fixture.database.context,
            prepared.attempt_token.clone(),
            input.clone(),
        )
        .await
        .unwrap();
    let duplicate = fixture
        .coordinator
        .run(&fixture.database.context, prepared.attempt_token, input)
        .await
        .unwrap();
    assert!(duplicate.admission.deduplicated);
    assert_eq!(duplicate.query_run_id, admitted.query_run_id);
    assert_eq!(duplicate.execution_epoch, admitted.execution_epoch);
    completed(&fixture, admitted.admission.operation_id).await;
    let dispatches: i64 = fixture
        .store
        .read(|connection| {
            connection
                .query_row("SELECT SUM(dispatch_count) FROM operations", [], |row| {
                    row.get(0)
                })
                .map_err(|_| WorkbenchError::StoreUnavailable)
        })
        .unwrap();
    assert_eq!(dispatches, 1);
}
#[tokio::test]
async fn manual_transaction_same_socket_rollback_and_commit_do_not_replay() {
    let fixture = support::QueryFixture::new(
        support::Fixture::sqlite_mode(
            "CREATE TABLE source(id INTEGER PRIMARY KEY,label TEXT)",
            Mode::ReadWrite,
        )
        .await,
    );
    let tx = fixture
        .coordinator
        .transaction_begin(
            &fixture.database.context,
            TransactionBeginInput {
                session: fixture.database.session,
                isolation: "serializable".into(),
                read_only: false,
            },
        )
        .await
        .unwrap();
    let insert = run(&fixture, "INSERT INTO source VALUES(1,'draft')").await;
    let operation = completed(&fixture, insert.admission.operation_id).await;
    assert_eq!(
        operation.statements[0].effect_evidence,
        EffectEvidence::PendingTransaction
    );
    let rolled = fixture
        .coordinator
        .transaction_rollback(
            &fixture.database.context,
            TransactionInput {
                session: fixture.database.session,
                transaction_id: tx.transaction_id,
            },
        )
        .await
        .unwrap();
    assert_eq!(rolled.state, TransactionState::RolledBack);
    let count: i64 = rusqlite::Connection::open(&fixture.database.path)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM source", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);
    let tx = fixture
        .coordinator
        .transaction_begin(
            &fixture.database.context,
            TransactionBeginInput {
                session: fixture.database.session,
                isolation: "serializable".into(),
                read_only: false,
            },
        )
        .await
        .unwrap();
    let insert = run(&fixture, "INSERT INTO source VALUES(1,'committed')").await;
    completed(&fixture, insert.admission.operation_id).await;
    let input = TransactionInput {
        session: fixture.database.session,
        transaction_id: tx.transaction_id,
    };
    let token = fixture
        .coordinator
        .prepare_commit(&fixture.database.context, input.clone())
        .await
        .unwrap();
    fixture
        .coordinator
        .transaction_commit(
            &fixture.database.context,
            token.attempt_token.clone(),
            input.clone(),
        )
        .await
        .unwrap();
    let duplicate = fixture
        .coordinator
        .transaction_commit(&fixture.database.context, token.attempt_token, input)
        .await
        .unwrap();
    assert!(duplicate.deduplicated);
    let count: i64 = rusqlite::Connection::open(&fixture.database.path)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM source", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
}
#[tokio::test]
async fn queued_cancel_is_proven_unsent_and_running_cancel_cannot_hit_a_new_generation() {
    let fixture = support::QueryFixture::new(
        support::Fixture::sqlite("CREATE TABLE source(id INTEGER)").await,
    );
    let mut inputs = Vec::new();
    for sql in ["WITH RECURSIVE data(n) AS (VALUES(0) UNION ALL SELECT n+1 FROM data WHERE n<1000000000) SELECT sum(n) FROM data","SELECT 2"] {
        let review=fixture.coordinator.review(&fixture.database.context,QueryReviewInput{session:fixture.database.session,sql:sql.into(),purpose:QueryPurpose::Run}).await.unwrap();
        let input=QueryRunInput{session:fixture.database.session,review_id:review.review_id,confirmed_warnings:review.warnings,limits:limits()};let token=fixture.coordinator.prepare_query(&fixture.database.context,input.clone()).await.unwrap();inputs.push((input,token));
    }
    let (input, token) = inputs.remove(0);
    let running = fixture
        .coordinator
        .run(&fixture.database.context, token.attempt_token, input)
        .await
        .unwrap();
    let (input, token) = inputs.remove(0);
    let queued = fixture
        .coordinator
        .run(&fixture.database.context, token.attempt_token, input)
        .await
        .unwrap();
    assert!(matches!(
        fixture
            .coordinator
            .cancel(
                &fixture.database.context,
                QueryCancelInput {
                    session: fixture.database.session,
                    query_run_id: queued.query_run_id,
                    execution_epoch: running.execution_epoch
                }
            )
            .await,
        Err(WorkbenchError::SessionStale)
    ));
    let cancel = fixture
        .coordinator
        .cancel(
            &fixture.database.context,
            QueryCancelInput {
                session: fixture.database.session,
                query_run_id: queued.query_run_id,
                execution_epoch: queued.execution_epoch,
            },
        )
        .await
        .unwrap();
    assert!(cancel.cancel_requested && cancel.settled);
    let cancelled = completed(&fixture, queued.admission.operation_id).await;
    assert_eq!(cancelled.state, OperationState::Cancelled);
    let dispatch: i64 = fixture
        .store
        .read(|connection| {
            connection
                .query_row(
                    "SELECT dispatch_count FROM operations WHERE operation_id=?1",
                    [queued.admission.operation_id.to_string()],
                    |row| row.get(0),
                )
                .map_err(|_| WorkbenchError::StoreUnavailable)
        })
        .unwrap();
    assert_eq!(dispatch, 0);
    let cancel = fixture
        .coordinator
        .cancel(
            &fixture.database.context,
            QueryCancelInput {
                session: fixture.database.session,
                query_run_id: running.query_run_id,
                execution_epoch: running.execution_epoch,
            },
        )
        .await
        .unwrap();
    assert!(cancel.cancel_requested);
    assert!(!cancel.settled);
    let interrupted = completed(&fixture, running.admission.operation_id).await;
    assert_eq!(interrupted.state, OperationState::Interrupted);
    assert_ne!(
        interrupted.transaction_outcome,
        TransactionOutcome::RolledBack
    );
    let session = fixture
        .coordinator
        .reconnect(
            &fixture.database.context,
            fixture.database.session,
            &"b".repeat(64),
        )
        .await
        .unwrap();
    assert_ne!(session.session, fixture.database.session);
    assert!(matches!(
        fixture
            .coordinator
            .cancel(
                &fixture.database.context,
                QueryCancelInput {
                    session: session.session,
                    query_run_id: running.query_run_id,
                    execution_epoch: running.execution_epoch
                }
            )
            .await,
        Err(WorkbenchError::Forbidden)
    ));
    fixture
        .database
        .runtime
        .with_connection(
            &fixture.database.context,
            session.session,
            AuthorizePurpose::Read,
            |_| Box::pin(async { Ok(()) }),
        )
        .await
        .unwrap();
}
#[tokio::test]
async fn native_idle_deadline_rolls_back_and_mode_change_fences_old_handles() {
    let mut fixture = support::QueryFixture::new(
        support::Fixture::sqlite_mode("CREATE TABLE source(id INTEGER)", Mode::ReadWrite).await,
    );
    fixture.coordinator = std::sync::Arc::new(
        restoreplane_workbench::query::QueryCoordinator::new(
            fixture.database.runtime.clone(),
            fixture.journal.clone(),
            fixture.streams.clone(),
        )
        .with_idle_timeout(std::time::Duration::from_millis(500))
        .unwrap(),
    );
    let tx = fixture
        .coordinator
        .transaction_begin(
            &fixture.database.context,
            TransactionBeginInput {
                session: fixture.database.session,
                isolation: "serializable".into(),
                read_only: false,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        fixture
            .database
            .runtime
            .set_mode(
                &fixture.database.context,
                fixture.database.session,
                Mode::ReadOnly
            )
            .await,
        Err(WorkbenchError::TargetBusy)
    ));
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    assert_eq!(fixture.coordinator.expire_transactions().await.unwrap(), 1);
    let operation = fixture
        .journal
        .get(
            &fixture.database.context,
            &fixture.database.scope,
            tx.transaction_id,
        )
        .unwrap();
    assert_eq!(
        operation.transaction_outcome,
        TransactionOutcome::RolledBack
    );
    let session = fixture
        .database
        .runtime
        .set_mode(
            &fixture.database.context,
            fixture.database.session,
            Mode::ReadOnly,
        )
        .await
        .unwrap();
    assert_ne!(session.session, fixture.database.session);
    let upgraded = fixture
        .database
        .runtime
        .set_mode(&fixture.database.context, session.session, Mode::ReadWrite)
        .await
        .unwrap();
    assert_ne!(upgraded.session, session.session);
}
#[tokio::test]
async fn credential_revocation_marks_manual_transaction_unknown_without_rollback_inference() {
    let fixture = support::QueryFixture::new(
        support::Fixture::sqlite_mode("CREATE TABLE source(id INTEGER)", Mode::ReadWrite).await,
    );
    let tx = fixture
        .coordinator
        .transaction_begin(
            &fixture.database.context,
            TransactionBeginInput {
                session: fixture.database.session,
                isolation: "serializable".into(),
                read_only: false,
            },
        )
        .await
        .unwrap();
    let credential = fixture
        .database
        .runtime
        .credential_reference(&fixture.database.context, fixture.database.session)
        .unwrap();
    fixture
        .database
        .runtime
        .invalidate_credential(&fixture.database.context, credential)
        .await
        .unwrap();
    let status = fixture
        .coordinator
        .transaction_get(&fixture.database.context, fixture.database.session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.state, TransactionState::OutcomeUnknown);
    let operation = fixture
        .journal
        .get(
            &fixture.database.context,
            &fixture.database.scope,
            tx.transaction_id,
        )
        .unwrap();
    assert_eq!(operation.state, OperationState::OutcomeUnknown);
}

#[tokio::test]
async fn commit_requires_fresh_authority_and_owner_can_still_request_observed_rollback() {
    let fixture = support::QueryFixture::new(
        support::Fixture::sqlite_mode("CREATE TABLE source(id INTEGER)", Mode::ReadWrite).await,
    );
    let tx = fixture
        .coordinator
        .transaction_begin(
            &fixture.database.context,
            TransactionBeginInput {
                session: fixture.database.session,
                isolation: "serializable".into(),
                read_only: false,
            },
        )
        .await
        .unwrap();
    let inserted = run(&fixture, "INSERT INTO source VALUES(1)").await;
    completed(&fixture, inserted.admission.operation_id).await;
    let input = TransactionInput {
        session: fixture.database.session,
        transaction_id: tx.transaction_id,
    };
    let token = fixture
        .coordinator
        .prepare_commit(&fixture.database.context, input.clone())
        .await
        .unwrap();
    fixture
        .database
        .authority_available
        .store(false, std::sync::atomic::Ordering::SeqCst);
    assert!(matches!(
        fixture
            .coordinator
            .transaction_commit(
                &fixture.database.context,
                token.attempt_token,
                input.clone()
            )
            .await,
        Err(WorkbenchError::ServiceUnavailable)
    ));
    let still_open = fixture
        .journal
        .get(
            &fixture.database.context,
            &fixture.database.scope,
            tx.transaction_id,
        )
        .unwrap();
    assert_eq!(still_open.state, OperationState::Running);
    let rolled_back = fixture
        .coordinator
        .transaction_rollback(&fixture.database.context, input)
        .await
        .unwrap();
    assert_eq!(rolled_back.state, TransactionState::RolledBack);
    let rows: i64 = rusqlite::Connection::open(&fixture.database.path)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM source", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 0);
}

#[tokio::test]
async fn metadata_only_retired_session_reconnects_from_native_basis_after_authority_recovers() {
    let fixture = support::QueryFixture::new(
        support::Fixture::sqlite("CREATE TABLE source(id INTEGER)").await,
    );
    let context = &fixture.database.context;
    let session = fixture.database.session;
    fixture
        .database
        .authority_available
        .store(false, Ordering::SeqCst);
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    assert!(matches!(
        fixture
            .database
            .runtime
            .with_connection(context, session, AuthorizePurpose::Read, move |_| Box::pin(
                async move {
                    observed.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            ))
            .await,
        Err(WorkbenchError::ServiceUnavailable)
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        fixture
            .database
            .runtime
            .get(context, session)
            .unwrap()
            .state,
        SessionState::Retiring
    );
    fixture
        .database
        .authority_available
        .store(true, Ordering::SeqCst);
    let other_window = restoreplane_workbench::InvocationContext::new(
        context.principal().clone(),
        uuid::Uuid::new_v4(),
    );
    assert!(matches!(
        fixture
            .coordinator
            .reconnect(&other_window, session, &"b".repeat(64))
            .await,
        Err(WorkbenchError::SessionStale | WorkbenchError::Forbidden)
    ));
    let other_owner = restoreplane_workbench::InvocationContext::new(
        restoreplane_workbench::NativePrincipal::new(
            "c".repeat(64),
            7,
            restoreplane_workbench::IntegrityLevel::Standard,
        )
        .unwrap(),
        context.window_instance_id(),
    );
    assert!(matches!(
        fixture
            .coordinator
            .reconnect(&other_owner, session, &"b".repeat(64))
            .await,
        Err(WorkbenchError::Forbidden)
    ));
    let mut other_generation = session;
    other_generation.connection_generation = (session.connection_generation.value() + 1).into();
    assert!(matches!(
        fixture
            .coordinator
            .reconnect(context, other_generation, &"b".repeat(64))
            .await,
        Err(WorkbenchError::SessionStale)
    ));
    let reopened = fixture
        .coordinator
        .reconnect(context, session, &"b".repeat(64))
        .await
        .unwrap();
    assert_ne!(reopened.session, session);
    assert_eq!(reopened.scope, fixture.database.scope);
    assert_eq!(reopened.mode, Mode::ReadOnly);
    fixture
        .database
        .runtime
        .with_connection(
            context,
            reopened.session,
            AuthorizePurpose::Read,
            move |_| {
                Box::pin(async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn metadata_only_authority_epoch_rotation_discards_old_driver_and_reconnects_with_fresh_receipt(
) {
    for after_first_read in [false, true] {
        let fixture = support::QueryFixture::new(
            support::Fixture::sqlite("CREATE TABLE source(id INTEGER)").await,
        );
        let context = &fixture.database.context;
        let session = fixture.database.session;
        if after_first_read {
            fixture
                .database
                .rotate_authority_after_read
                .store(true, Ordering::SeqCst);
        } else {
            *fixture.database.authority_epoch.lock().unwrap() = uuid::Uuid::new_v4().to_string();
        }
        #[cfg(windows)]
        let parked = fixture.database.path.with_file_name("epoch-parked.sqlite3");
        #[cfg(windows)]
        assert!(std::fs::rename(&fixture.database.path, &parked).is_err());
        let calls = Arc::new(AtomicUsize::new(0));
        let callback = calls.clone();
        let stale = fixture
            .database
            .runtime
            .with_connection(context, session, AuthorizePurpose::Read, move |_| {
                Box::pin(async move {
                    callback.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            })
            .await;
        assert!(matches!(stale, Err(WorkbenchError::BindingStale)));
        #[cfg(windows)]
        {
            std::fs::rename(&fixture.database.path, &parked).unwrap();
            std::fs::rename(&parked, &fixture.database.path).unwrap();
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            fixture
                .database
                .runtime
                .get(context, session)
                .unwrap()
                .state,
            SessionState::Retiring
        );
        let reopened = fixture
            .coordinator
            .reconnect(context, session, &"b".repeat(64))
            .await
            .unwrap();
        assert_ne!(reopened.session, session);
        let receipt = fixture
            .database
            .runtime
            .binding_receipt(context, reopened.session, AuthorizePurpose::Read)
            .await
            .unwrap();
        assert_eq!(
            receipt.authority_epoch,
            *fixture.database.authority_epoch.lock().unwrap()
        );
        assert_eq!(
            fixture
                .database
                .runtime
                .list(context, &fixture.database.scope)
                .unwrap()
                .len(),
            1
        );
        let callback = calls.clone();
        assert!(matches!(
            fixture
                .database
                .runtime
                .with_connection(context, session, AuthorizePurpose::Read, move |_| Box::pin(
                    async move {
                        callback.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    }
                ))
                .await,
            Err(WorkbenchError::SessionStale)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        fixture
            .database
            .runtime
            .with_connection(
                context,
                reopened.session,
                AuthorizePurpose::Read,
                move |_| {
                    Box::pin(async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    })
                },
            )
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn positive_begin_or_rollback_with_discarded_ack_is_unknown_and_never_reuses_the_socket() {
    for discarded in [ControlAck::Begin, ControlAck::Rollback] {
        let source = support::Fixture::sqlite_mode(
            "CREATE TABLE source(id INTEGER PRIMARY KEY)",
            Mode::ReadWrite,
        )
        .await;
        let descriptor = source
            .runtime
            .binding_receipt(&source.context, source.session, AuthorizePurpose::Read)
            .await
            .unwrap()
            .descriptor;
        let fixture = support::QueryFixture::new(
            support::Fixture::connect_with_factory(
                descriptor,
                String::new(),
                zeroize::Zeroizing::new(String::new()),
                Mode::ReadWrite,
                Arc::new(CountFactory {
                    calls: Arc::new(AtomicUsize::new(0)),
                    discard_control_ack: Some(discarded),
                }),
            )
            .await,
        );
        let begin = fixture
            .coordinator
            .transaction_begin(
                &fixture.database.context,
                TransactionBeginInput {
                    session: fixture.database.session,
                    isolation: "serializable".into(),
                    read_only: false,
                },
            )
            .await;
        match discarded {
            ControlAck::Begin => assert!(matches!(begin, Err(WorkbenchError::CancelUnsettled))),
            ControlAck::Rollback => {
                let transaction = begin.unwrap();
                let admitted = run(&fixture, "INSERT INTO source VALUES(1)").await;
                completed(&fixture, admitted.admission.operation_id).await;
                worker_settled(&fixture, &admitted).await;
                let result = fixture
                    .coordinator
                    .transaction_rollback(
                        &fixture.database.context,
                        TransactionInput {
                            session: fixture.database.session,
                            transaction_id: transaction.transaction_id,
                        },
                    )
                    .await
                    .unwrap();
                assert_eq!(result.state, TransactionState::OutcomeUnknown);
            }
        }
        let status = fixture
            .coordinator
            .transaction_get(&fixture.database.context, fixture.database.session)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.state, TransactionState::OutcomeUnknown);
        let journal = fixture
            .journal
            .get(
                &fixture.database.context,
                &fixture.database.scope,
                status.transaction_id,
            )
            .unwrap();
        assert_eq!(journal.state, OperationState::OutcomeUnknown);
        assert_eq!(
            journal.transaction_outcome,
            TransactionOutcome::OutcomeUnknown
        );
        let reused = Arc::new(AtomicUsize::new(0));
        let callback = reused.clone();
        assert!(matches!(
            fixture
                .database
                .runtime
                .with_connection(
                    &fixture.database.context,
                    fixture.database.session,
                    AuthorizePurpose::Read,
                    move |_| Box::pin(async move {
                        callback.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    })
                )
                .await,
            Err(WorkbenchError::SessionStale)
        ));
        assert_eq!(reused.load(Ordering::SeqCst), 0);
        // The independent observer sees actual data, while the discarded ACK
        // deliberately prevents this coordinator from claiming rollback proof.
        let rows: i64 = rusqlite::Connection::open(&source.path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM source", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 0);
    }
}

#[cfg(windows)]
#[tokio::test]
async fn retiring_lease_drops_actual_sqlite_file_pin_and_reconnect_reclaims_the_same_bounded_slot()
{
    use restoreplane_workbench::session::ConnectionLeaseKind;
    let source = support::Fixture::sqlite_mode(
        "CREATE TABLE source(id INTEGER PRIMARY KEY);INSERT INTO source VALUES(1)",
        Mode::ReadWrite,
    )
    .await;
    let descriptor = source
        .runtime
        .binding_receipt(&source.context, source.session, AuthorizePurpose::Read)
        .await
        .unwrap()
        .descriptor;
    source
        .runtime
        .close(&source.context, source.session)
        .await
        .unwrap();
    let database = support::Fixture::connect(
        descriptor,
        String::new(),
        zeroize::Zeroizing::new(String::new()),
        Mode::ReadWrite,
    )
    .await;
    let fixture = support::QueryFixture::new(database);
    let context = &fixture.database.context;
    let session = fixture.database.session;
    let parked = source.path.with_file_name("parked.sqlite3");
    assert!(
        std::fs::rename(&source.path, &parked).is_err(),
        "the live native driver must really hold the no-delete-share file pin"
    );
    let lease = fixture
        .database
        .runtime
        .pin_connection(
            context,
            session,
            ConnectionLeaseKind::StandaloneWrite,
            std::time::Duration::from_secs(30),
        )
        .await
        .unwrap();
    fixture
        .database
        .runtime
        .finish_lease(context, &lease, false)
        .await
        .unwrap();
    let views = fixture
        .database
        .runtime
        .list(context, &fixture.database.scope)
        .unwrap();
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].state, SessionState::Retiring);
    // This would still fail if only interrupt()/logical close had run while
    // retaining SqliteConnection, its guard File, or its connection handle.
    std::fs::rename(&source.path, &parked).unwrap();
    std::fs::rename(&parked, &source.path).unwrap();
    let invoked = Arc::new(AtomicUsize::new(0));
    let callback = invoked.clone();
    assert!(matches!(
        fixture
            .database
            .runtime
            .with_lease_cleanup(context, &lease, move |_| Box::pin(async move {
                callback.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }))
            .await,
        Err(WorkbenchError::SessionStale | WorkbenchError::Forbidden)
    ));
    assert_eq!(invoked.load(Ordering::SeqCst), 0);
    assert!(matches!(
        fixture
            .database
            .runtime
            .request_cancel(context, session, 0)
            .await,
        Err(WorkbenchError::SessionStale)
    ));
    let other_window = restoreplane_workbench::InvocationContext::new(
        context.principal().clone(),
        uuid::Uuid::new_v4(),
    );
    assert!(matches!(
        fixture
            .coordinator
            .reconnect(&other_window, session, &"b".repeat(64))
            .await,
        Err(WorkbenchError::SessionStale | WorkbenchError::Forbidden)
    ));
    let mut other_generation = session;
    other_generation.connection_generation = (session.connection_generation.value() + 1).into();
    assert!(matches!(
        fixture
            .coordinator
            .reconnect(context, other_generation, &"b".repeat(64))
            .await,
        Err(WorkbenchError::SessionStale)
    ));
    let restored = fixture
        .coordinator
        .reconnect(context, session, &"b".repeat(64))
        .await
        .unwrap();
    assert_ne!(restored.session, session);
    let views = fixture
        .database
        .runtime
        .list(context, &fixture.database.scope)
        .unwrap();
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].state, SessionState::Ready);
    fixture
        .database
        .runtime
        .with_connection(
            context,
            restored.session,
            AuthorizePurpose::Read,
            move |_| {
                Box::pin(async move {
                    invoked.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        fixture
            .database
            .runtime
            .with_connection(context, session, AuthorizePurpose::Read, |_| Box::pin(
                async { Ok(()) }
            ))
            .await,
        Err(WorkbenchError::SessionStale)
    ));
    fixture
        .database
        .runtime
        .close_window(context)
        .await
        .unwrap();
}

#[tokio::test]
async fn query_row_wrapper_bytecap_keeps_a_contiguous_prefix_and_finishes_before_the_next_review() {
    let fixture = support::QueryFixture::new(
        support::Fixture::sqlite("CREATE TABLE source(id INTEGER)").await,
    );
    let value = "x".repeat(100);
    let sql=format!("SELECT '{value}' AS value UNION ALL SELECT '{value}' UNION ALL SELECT '{value}' UNION ALL SELECT ''");
    let review = fixture
        .coordinator
        .review(
            &fixture.database.context,
            QueryReviewInput {
                session: fixture.database.session,
                sql,
                purpose: QueryPurpose::Run,
            },
        )
        .await
        .unwrap();
    let input = QueryRunInput {
        session: fixture.database.session,
        review_id: review.review_id,
        confirmed_warnings: review.warnings,
        limits: Limits {
            max_rows: 100,
            max_bytes: 512,
            statement_timeout_ms: 30000,
            lock_wait_ms: 2000,
        },
    };
    let token = fixture
        .coordinator
        .prepare_query(&fixture.database.context, input.clone())
        .await
        .unwrap();
    let admitted = fixture
        .coordinator
        .run(&fixture.database.context, token.attempt_token, input)
        .await
        .unwrap();
    let operation = completed(&fixture, admitted.admission.operation_id).await;
    let results = worker_settled(&fixture, &admitted).await;
    assert_eq!(operation.result_completeness, ResultCompleteness::Capped);
    assert_eq!(
        results[0].completeness,
        Some(restoreplane_domain::workbench_stream::Completion::Capped)
    );
    let page = fixture
        .streams
        .page(&fixture.database.context, &results[0].identity, 0, 100)
        .unwrap();
    assert_eq!(page.page().page.items.len(),2,"the row handle consumes bytes too; a later empty value must not fill the skipped third row");
    assert!(page.page().page.items.iter().all(|row| row.cells
        == vec![TypedCell::Text {
            value: value.clone()
        }]));
    // Count::Exact above is native worker settlement, not just a terminal row.
    fixture
        .coordinator
        .review(
            &fixture.database.context,
            QueryReviewInput {
                session: fixture.database.session,
                sql: "SELECT 1".into(),
                purpose: QueryPurpose::Run,
            },
        )
        .await
        .unwrap();
}

// Native Explain regression tests; execution is subject to the owned test lease.
#[path = "support/explain_wire_probe.rs"]
mod explain_wire_probe;

#[tokio::test]
async fn sqlite_explain_plan_purpose_roundtrip() {
    use restoreplane_domain::workbench_stream::Completion;

    let fixture = support::QueryFixture::new(
        support::Fixture::sqlite("CREATE TABLE source(id INTEGER PRIMARY KEY)").await,
    );
    let source = "SELECT 1 AS exact_source /* 한글 원문 */\r\n";
    let source_before = source.as_bytes().to_vec();
    let wire = format!("EXPLAIN QUERY PLAN {source}");
    let review = fixture
        .coordinator
        .review(
            &fixture.database.context,
            QueryReviewInput {
                session: fixture.database.session,
                sql: wire.clone(),
                purpose: QueryPurpose::ExplainPlan,
            },
        )
        .await
        .unwrap_or_else(|error| panic!("native ExplainPlan review {}", error.code()));
    assert_eq!(
        review.digest,
        blake3::hash(wire.as_bytes()).to_hex().to_string()
    );
    assert!(review
        .warnings
        .iter()
        .any(|warning| warning == "plan_is_not_effect_proof"));
    let input = QueryRunInput {
        session: fixture.database.session,
        review_id: review.review_id,
        confirmed_warnings: review.warnings,
        limits: limits(),
    };
    let prepared = fixture
        .coordinator
        .prepare_query(&fixture.database.context, input.clone())
        .await
        .unwrap();
    let admission = fixture
        .coordinator
        .run(&fixture.database.context, prepared.attempt_token, input)
        .await
        .unwrap();
    assert!(admission.execution_epoch.value() > 0);
    let results = worker_settled(&fixture, &admission).await;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].completeness, Some(Completion::Complete));
    let page = fixture
        .streams
        .page(&fixture.database.context, &results[0].identity, 0, 200)
        .unwrap();
    assert!(!page.page().columns.is_empty());
    assert!(!page.page().page.items.is_empty());
    drop(page);
    acknowledge_result(&fixture, &results[0].identity);
    let operation = fixture
        .journal
        .get(
            &fixture.database.context,
            &fixture.database.scope,
            admission.admission.operation_id,
        )
        .unwrap();
    assert_eq!(operation.state, OperationState::Completed);
    assert_eq!(operation.statements.len(), 1);
    assert_eq!(operation.statements[0].outcome, StatementStatus::Succeeded);
    assert_eq!(
        operation.statements[0].effect_evidence,
        EffectEvidence::ReadOnly
    );
    assert_eq!(
        fixture
            .database
            .runtime
            .get(&fixture.database.context, fixture.database.session)
            .unwrap()
            .state,
        SessionState::Ready
    );
    assert_eq!(source.as_bytes(), source_before.as_slice());
    println!("sqlite ExplainPlan: native purpose/digest accepted, actual plan rows/Complete/pull ACK observed; effect ReadOnly; source bytes unchanged");
}

#[tokio::test]
async fn native_explain_modes_and_purpose_mismatch_reject_before_dispatch() {
    for mode in [Mode::ReadOnly, Mode::ReadWrite, Mode::Full] {
        let mut cases = vec![
            (QueryPurpose::ExplainPlan, "SELECT 1"),
            (QueryPurpose::ExplainPlan, "EXPLAIN ANALYZE SELECT 1"),
            (QueryPurpose::ExplainAnalyze, "EXPLAIN SELECT 1"),
        ];
        if mode != Mode::Full {
            cases.push((QueryPurpose::ExplainAnalyze, "EXPLAIN ANALYZE SELECT 1"));
        }
        let watched: Vec<_> = cases.iter().map(|(_, wire)| *wire).collect();
        let probe = explain_wire_probe::Probe::new(&watched);
        let source =
            support::Fixture::sqlite_mode("CREATE TABLE source(id INTEGER PRIMARY KEY)", mode)
                .await;
        let receipt = source
            .runtime
            .binding_receipt(&source.context, source.session, AuthorizePurpose::Read)
            .await
            .unwrap();
        let fixture = support::QueryFixture::new(
            support::Fixture::connect_with_factory(
                receipt.descriptor,
                String::new(),
                zeroize::Zeroizing::new(String::new()),
                mode,
                probe.factory(),
            )
            .await,
        );
        for (purpose, wire) in cases {
            let review = fixture
                .coordinator
                .review(
                    &fixture.database.context,
                    QueryReviewInput {
                        session: fixture.database.session,
                        sql: wire.into(),
                        purpose,
                    },
                )
                .await;
            assert!(
                review.is_err(),
                "native mode/purpose gate must refuse before admission"
            );
            assert_eq!(
                probe.count(wire),
                0,
                "rejected exact user SQL must never enter the actual native adapter"
            );
        }
        // Review's closed settings probes are permitted; no user statement is admitted or dispatched.
        let counts: (i64, i64) = fixture
            .store
            .read(|connection| {
                connection
                    .query_row(
                        "SELECT COUNT(*),COALESCE(SUM(dispatch_count),0) FROM operations",
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .map_err(|_| WorkbenchError::StoreUnavailable)
            })
            .unwrap();
        assert_eq!(counts, (0, 0));
        assert_eq!(
            fixture
                .database
                .runtime
                .get(&fixture.database.context, fixture.database.session)
                .unwrap()
                .state,
            SessionState::Ready
        );
    }
    println!("native Explain negative modes/purpose: exact user adapter sends0/admitted0/dispatched0; review probes permitted");
}
