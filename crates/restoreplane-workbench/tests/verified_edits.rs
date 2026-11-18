mod support;
use async_trait::async_trait;
use restoreplane_domain::workbench::*;
use restoreplane_domain::workbench_edit::*;
use restoreplane_domain::workbench_metadata::{CatalogKind, Filter, Limits, ObjectRef, PageInput};
use restoreplane_workbench::browse::{BrowseInput, BrowseService};
use restoreplane_workbench::driver::{
    DriverConnection, DriverFactory, DriverLogin, DriverSummary, NativeDriverFactory, QueryLimits,
    RowSink, TransactionCommand,
};
use restoreplane_workbench::edits::EditService;
use restoreplane_workbench::generated_sql::{self, NativeInsertLimits, WriteCertification};
use restoreplane_workbench::metadata::{MetadataCatalog, RelationMetadata};
use restoreplane_workbench::{InvocationContext, WorkbenchError};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use uuid::Uuid;
use zeroize::Zeroizing;

struct Harness {
    database: support::QueryFixture,
    metadata: Arc<MetadataCatalog>,
    browse: Arc<BrowseService>,
    edits: Arc<EditService>,
}
impl Harness {
    async fn new(database: support::Fixture) -> Self {
        let database = support::QueryFixture::new(database);
        let metadata = Arc::new(MetadataCatalog::new(
            database.database.runtime.clone(),
            database.streams.budget(),
        ));
        let browse = Arc::new(BrowseService::new(
            metadata.clone(),
            database.streams.clone(),
        ));
        let facts = database
            .database
            .runtime
            .with_connection(
                &database.database.context,
                database.database.session,
                AuthorizePurpose::Read,
                |connection| Box::pin(async move { Ok(connection.facts().clone()) }),
            )
            .await
            .unwrap();
        // Explicit native fixture seam; not an OS-token/service E2E or a
        // production certification promotion before this scenario has passed.
        let cert = WriteCertification::from_verified_build(
            "section06-native-fixture-candidate".into(),
            vec![(facts.engine, facts.server_version)],
        )
        .unwrap();
        let edits = Arc::new(EditService::new(
            database.database.runtime.clone(),
            metadata.clone(),
            browse.clone(),
            database.journal.clone(),
            Some(cert),
        ));
        Self {
            database,
            metadata,
            browse,
            edits,
        }
    }
    fn ctx(&self) -> &InvocationContext {
        &self.database.database.context
    }
    fn session(&self) -> SessionRef {
        self.database.database.session
    }
    async fn object(&self, name: &str) -> ObjectRef {
        self.metadata
            .page(
                self.ctx(),
                self.session(),
                None,
                CatalogKind::Relations,
                name.into(),
                PageInput {
                    cursor: None,
                    limit: 100,
                },
            )
            .await
            .unwrap()
            .items
            .into_iter()
            .find(|item| item.name == name)
            .unwrap()
            .object
    }
    async fn relation(&self, name: &str) -> Arc<RelationMetadata> {
        let object = self.object(name).await;
        self.metadata
            .resolve_relation(self.ctx(), self.session(), &object)
            .await
            .unwrap()
    }
    async fn rows(&self, object: ObjectRef) -> restoreplane_domain::workbench_metadata::ResultPage {
        self.browse
            .open(
                self.ctx(),
                self.session(),
                BrowseInput {
                    object,
                    filter: Filter::default(),
                    order: Vec::new(),
                    snapshot_id: None,
                    limits: Limits {
                        max_rows: 100,
                        max_bytes: 512 * 1024,
                        statement_timeout_ms: 30000,
                        lock_wait_ms: 2000,
                    },
                },
                None,
            )
            .await
            .unwrap()
            .result
    }
    async fn review(&self, object: ObjectRef, changes: Vec<EditChange>) -> EditReview {
        self.edits
            .prepare(
                self.ctx(),
                EditPrepareInput {
                    session: self.session(),
                    object,
                    changes,
                },
            )
            .await
            .unwrap()
    }
    fn input(&self, review: &EditReview) -> EditApplyInput {
        EditApplyInput {
            session: self.session(),
            draft_id: review.draft_id,
            review_id: review.review_id,
            confirmed_warnings: review.warnings.clone(),
        }
    }
    async fn apply(&self, review: &EditReview) -> Result<Admission, WorkbenchError> {
        let input = self.input(review);
        let token = self.edits.prepare_apply(self.ctx(), input.clone()).await?;
        self.edits
            .apply(self.ctx(), token.attempt_token, input)
            .await
    }
    fn operation(&self, id: Uuid) -> Operation {
        self.database
            .journal
            .get(self.ctx(), &self.database.database.scope, id)
            .unwrap()
    }
}
fn value(relation: &RelationMetadata, name: &str, value: TypedCell) -> EditValue {
    EditValue {
        column_id: relation
            .columns
            .iter()
            .find(|c| c.column.name == name)
            .unwrap()
            .column
            .column_id,
        value,
    }
}
fn integer(value: i64) -> TypedCell {
    TypedCell::I64 {
        value: value.to_string(),
    }
}
fn text(value: &str) -> TypedCell {
    TypedCell::Text {
        value: value.into(),
    }
}

#[tokio::test]
async fn insert_review_distinguishes_omission_null_and_generated_or_default_expressions() {
    let harness = Harness::new(support::Fixture::sqlite_mode("CREATE TABLE ordinary(id INTEGER PRIMARY KEY,label TEXT);CREATE TABLE generated(id INTEGER PRIMARY KEY,computed INTEGER GENERATED ALWAYS AS(id+1) STORED);CREATE TABLE defaults(id INTEGER PRIMARY KEY,label TEXT DEFAULT('server-value'));",Mode::ReadWrite).await).await;
    let relation = harness.relation("ordinary").await;
    let label = value(&relation, "label", TypedCell::Null).column_id;
    let omitted = harness
        .review(
            relation.object.clone(),
            vec![EditChange::Insert {
                values: vec![value(&relation, "id", integer(1))],
            }],
        )
        .await;
    assert_eq!(omitted.changes[0].omitted_default_column_ids, vec![label]);
    assert_eq!(omitted.parameter_preview, vec![integer(1)]);
    let explicit = harness
        .review(
            relation.object.clone(),
            vec![EditChange::Insert {
                values: vec![
                    value(&relation, "id", integer(2)),
                    value(&relation, "label", TypedCell::Null),
                ],
            }],
        )
        .await;
    assert!(explicit.changes[0].omitted_default_column_ids.is_empty());
    assert_eq!(
        explicit.parameter_preview,
        vec![integer(2), TypedCell::Null]
    );
    harness.apply(&omitted).await.unwrap();
    harness.apply(&explicit).await.unwrap();
    let generated_key = harness
        .edits
        .prepare(
            harness.ctx(),
            EditPrepareInput {
                session: harness.session(),
                object: relation.object.clone(),
                changes: vec![EditChange::Insert {
                    values: vec![value(
                        &relation,
                        "label",
                        text("server key would be required"),
                    )],
                }],
            },
        )
        .await;
    assert!(
        matches!(generated_key, Err(WorkbenchError::Unsupported)),
        "an omitted required primary key must not silently claim a generated-key retrieval path"
    );
    for (name, reason) in [
        ("generated", "generated_column"),
        ("defaults", "default_effects_unproven"),
    ] {
        let relation = harness.relation(name).await;
        let result = harness
            .edits
            .prepare(
                harness.ctx(),
                EditPrepareInput {
                    session: harness.session(),
                    object: relation.object.clone(),
                    changes: vec![EditChange::Insert {
                        values: vec![value(&relation, "id", integer(3))],
                    }],
                },
            )
            .await;
        assert!(matches!(result,Err(WorkbenchError::EditRefused(actual)) if actual==reason));
    }
    let default = serde_json::json!({"session":harness.session(),"object":relation.object,"changes":[{"kind":"update","rowHandle":Uuid::new_v4(),"values":[{"columnId":label,"value":{"kind":"default"}}]}]});
    assert!(
        serde_json::from_value::<EditPrepareInput>(default).is_err(),
        "DEFAULT is not a wire value or executable SQL escape"
    );
    let bound_default = harness
        .review(
            relation.object.clone(),
            vec![EditChange::Insert {
                values: vec![
                    value(&relation, "id", integer(3)),
                    value(&relation, "label", text("DEFAULT")),
                ],
            }],
        )
        .await;
    assert_eq!(
        bound_default.parameter_preview,
        vec![integer(3), text("DEFAULT")]
    );
    assert!(!bound_default.statement_previews[0].contains("DEFAULT"));
    harness.apply(&bound_default).await.unwrap();
    let rows = harness.rows(relation.object.clone()).await;
    assert_eq!(rows.page.items[0].cells[1], TypedCell::Null);
    assert_eq!(rows.page.items[1].cells[1], TypedCell::Null);
    assert_eq!(rows.page.items[2].cells[1], text("DEFAULT"));
}

#[tokio::test]
async fn capability_and_injection_gates_use_native_catalog_and_independent_grammar() {
    let table = "records\";--한글";
    let harness=Harness::new(support::Fixture::sqlite_mode("CREATE TABLE \"records\"\";--한글\"(id INTEGER PRIMARY KEY,label TEXT,payload BLOB);CREATE TABLE keyless(label TEXT);CREATE VIEW view_records AS SELECT * FROM \"records\"\";--한글\";",Mode::ReadWrite).await).await;
    let relation = harness.relation(table).await;
    let columns = relation
        .columns
        .iter()
        .map(|c| c.column.column_id)
        .collect::<Vec<_>>();
    let hostile = "'); DELETE FROM records; -- \" 한글";
    let parameters = vec![
        integer(i64::MAX),
        text(hostile),
        TypedCell::Bytes {
            base64: "AP8=".into(),
        },
    ];
    let generated = generated_sql::prepare_insert(
        relation.clone(),
        &columns,
        std::slice::from_ref(&parameters),
        NativeInsertLimits::editing(),
    )
    .unwrap();
    generated_sql::validate_generated_insert(generated.preview(), &relation, &columns, 1).unwrap();
    assert!(generated_sql::validate_generated_insert(
        &format!("{}; DELETE FROM records", generated.preview()),
        &relation,
        &columns,
        1
    )
    .is_err());
    assert!(generated_sql::validate_generated_insert(
        &generated.preview().replace("?1", "?9"),
        &relation,
        &columns,
        1
    )
    .is_err());
    assert!(generated_sql::validate_generated_insert(
        &format!("{} /* extra */", generated.preview()),
        &relation,
        &columns,
        1
    )
    .is_err());
    let review = harness
        .review(
            relation.object.clone(),
            vec![EditChange::Insert {
                values: columns
                    .iter()
                    .copied()
                    .zip(parameters)
                    .map(|(column_id, value)| EditValue { column_id, value })
                    .collect(),
            }],
        )
        .await;
    assert!(!review.statement_previews[0].contains(hostile));
    let applied = harness.apply(&review).await.unwrap();
    assert_eq!(
        harness.operation(applied.operation_id).state,
        OperationState::Committed
    );
    let rows = harness.rows(relation.object.clone()).await;
    assert_eq!(rows.page.items.len(), 1);
    assert_eq!(
        rows.page.items[0].cells,
        vec![
            integer(i64::MAX),
            text(hostile),
            TypedCell::Bytes {
                base64: "AP8=".into()
            }
        ]
    );
    for (name, reason) in [("keyless", "keyless"), ("view_records", "view")] {
        let relation = harness.relation(name).await;
        let input = EditPrepareInput {
            session: harness.session(),
            object: relation.object.clone(),
            changes: vec![EditChange::Insert {
                values: vec![value(&relation, "label", text("refused"))],
            }],
        };
        assert_eq!(
            harness
                .edits
                .prepare(harness.ctx(), input)
                .await
                .err()
                .unwrap(),
            WorkbenchError::EditRefused(reason.into())
        );
    }
}

#[tokio::test]
async fn indirect_constraints_are_refused_before_an_edit_is_admitted() {
    let harness = Harness::new(support::Fixture::sqlite_mode(
        "CREATE TABLE checked(id INTEGER PRIMARY KEY CHECK(id>0));CREATE TABLE parent(id INTEGER PRIMARY KEY);CREATE TABLE child(id INTEGER PRIMARY KEY,parent_id INTEGER REFERENCES parent(id))",
        Mode::ReadWrite,
    ).await).await;
    for name in ["checked", "parent", "child"] {
        let relation = harness.relation(name).await;
        let refused = harness
            .edits
            .prepare(
                harness.ctx(),
                EditPrepareInput {
                    session: harness.session(),
                    object: relation.object.clone(),
                    changes: vec![EditChange::Insert {
                        values: vec![value(&relation, "id", integer(7))],
                    }],
                },
            )
            .await
            .err()
            .unwrap();
        assert_eq!(
            refused,
            WorkbenchError::EditRefused("transactional_effects_unproven".into())
        );
    }
}

#[tokio::test]
async fn locked_original_conflict_preserves_rows_and_bounded_observation_mapping() {
    let harness=Harness::new(support::Fixture::sqlite_mode("CREATE TABLE records(id INTEGER PRIMARY KEY,label TEXT,payload BLOB);INSERT INTO records VALUES(1,'one',NULL),(2,'two',NULL)",Mode::ReadWrite).await).await;
    let relation = harness.relation("records").await;
    let rows = harness.rows(relation.object.clone()).await;
    let review = harness
        .review(
            relation.object.clone(),
            vec![
                EditChange::Update {
                    row_handle: rows.page.items[0].row_handle,
                    values: vec![value(&relation, "label", text("draft one"))],
                },
                EditChange::Update {
                    row_handle: rows.page.items[1].row_handle,
                    values: vec![value(&relation, "label", text("draft two"))],
                },
            ],
        )
        .await;
    let observer = rusqlite::Connection::open(&harness.database.database.path).unwrap();
    observer
        .execute("UPDATE records SET label='external' WHERE id=2", [])
        .unwrap();
    let id = match harness.apply(&review).await.err().unwrap() {
        WorkbenchError::EditConflict(id) => id,
        error => panic!("unexpected {}", error.code()),
    };
    let conflict = harness
        .edits
        .conflict_get(harness.ctx(), harness.session(), id)
        .unwrap();
    assert_eq!(conflict.review_id, review.review_id);
    assert_eq!(conflict.rows.len(), 2);
    let row = conflict
        .rows
        .iter()
        .find(|row| row.change_index == 1)
        .unwrap();
    assert_eq!(row.row_handle, Some(rows.page.items[1].row_handle));
    assert!(
        matches!(&row.current,ObservedCurrent::Present{values} if values.iter().any(|v|v.value==text("external")))
    );
    assert_eq!(
        harness.operation(conflict.operation_id).state,
        OperationState::RolledBack
    );
    assert_eq!(
        observer
            .query_row("SELECT label FROM records WHERE id=1", [], |row| row
                .get::<_, String>(0))
            .unwrap(),
        "one"
    );
    assert_eq!(
        observer
            .query_row("SELECT label FROM records WHERE id=2", [], |row| row
                .get::<_, String>(0))
            .unwrap(),
        "external"
    );
    let other = InvocationContext::new(harness.ctx().principal().clone(), Uuid::new_v4());
    assert!(harness
        .edits
        .conflict_get(&other, harness.session(), id)
        .is_err());
    assert!(harness
        .edits
        .discard(harness.ctx(), review.draft_id)
        .unwrap());
    assert!(harness
        .edits
        .conflict_get(harness.ctx(), harness.session(), id)
        .is_err());
}

#[tokio::test]
async fn verified_noop_is_unsent_and_mid_batch_constraint_rolls_back_prior_changes() {
    let harness=Harness::new(support::Fixture::sqlite_mode("CREATE TABLE records(id INTEGER PRIMARY KEY,label TEXT,payload BLOB);INSERT INTO records VALUES(1,NULL,NULL)",Mode::ReadWrite).await).await;
    let relation = harness.relation("records").await;
    let rows = harness.rows(relation.object.clone()).await;
    let review = harness
        .review(
            relation.object.clone(),
            vec![EditChange::Update {
                row_handle: rows.page.items[0].row_handle,
                values: vec![value(&relation, "label", TypedCell::Null)],
            }],
        )
        .await;
    let applied = harness.apply(&review).await.unwrap();
    let operation = harness.operation(applied.operation_id);
    assert_eq!(operation.state, OperationState::Committed);
    assert_eq!(operation.statements[0].rows_matched.unwrap().value(), 1);
    assert_eq!(operation.statements[0].rows_changed.unwrap().value(), 0);
    let changes = [2, 1]
        .into_iter()
        .map(|id| EditChange::Insert {
            values: vec![
                value(&relation, "id", integer(id)),
                value(&relation, "label", text("batch")),
                value(&relation, "payload", TypedCell::Null),
            ],
        })
        .collect();
    let review = harness.review(relation.object.clone(), changes).await;
    assert!(matches!(
        harness.apply(&review).await,
        Err(WorkbenchError::StatementRejected(_))
    ));
    let observer = rusqlite::Connection::open(&harness.database.database.path).unwrap();
    assert_eq!(
        observer
            .query_row("SELECT COUNT(*) FROM records", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    let operations = harness
        .database
        .journal
        .list(
            harness.ctx(),
            &harness.database.database.scope,
            None,
            None,
            100,
        )
        .unwrap();
    let rolled_back = operations
        .iter()
        .find(|operation| operation.state == OperationState::RolledBack)
        .expect("the batch must have positive rollback evidence");
    // The bounded list deliberately omits statement bodies; fetch this one
    // operation's bounded detail before asserting its settled row evidence.
    let rolled_back = harness.operation(rolled_back.operation_id);
    assert_eq!(rolled_back.statements.len(), 2);
    assert_eq!(
        rolled_back.statements[0].effect_evidence,
        EffectEvidence::RolledBack
    );
    assert_eq!(rolled_back.statements[1].outcome, StatementStatus::Failed);
    assert_eq!(
        rolled_back.statements[1].effect_evidence,
        EffectEvidence::RolledBack
    );
}

struct CommitFaultFactory {
    count: Arc<AtomicUsize>,
}
#[tokio::test]
async fn schema_change_before_dispatch_preserves_draft_and_never_claims_a_rollback() {
    let harness=Harness::new(support::Fixture::sqlite_mode("CREATE TABLE records(id INTEGER PRIMARY KEY,label TEXT,payload BLOB);INSERT INTO records VALUES(1,'original',NULL)",Mode::ReadWrite).await).await;
    let relation = harness.relation("records").await;
    let rows = harness.rows(relation.object.clone()).await;
    let review = harness
        .review(
            relation.object.clone(),
            vec![EditChange::Update {
                row_handle: rows.page.items[0].row_handle,
                values: vec![value(&relation, "label", text("draft"))],
            }],
        )
        .await;
    let observer = rusqlite::Connection::open(&harness.database.database.path).unwrap();
    observer
        .execute("ALTER TABLE records ADD COLUMN extra TEXT", [])
        .unwrap();
    let conflict = match harness.apply(&review).await.err().unwrap() {
        WorkbenchError::EditConflict(id) => harness
            .edits
            .conflict_get(harness.ctx(), harness.session(), id)
            .unwrap(),
        error => panic!("unexpected {}", error.code()),
    };
    assert_eq!(conflict.review_id, review.review_id);
    assert!(conflict.current_schema_version.is_none());
    assert!(
        matches!(&conflict.rows[0].current,ObservedCurrent::Unavailable{reason} if reason=="schema_changed")
    );
    let operation = harness.operation(conflict.operation_id);
    assert_eq!(operation.state, OperationState::Failed);
    assert_eq!(operation.transaction_outcome, TransactionOutcome::None);
    assert!(operation
        .statements
        .iter()
        .all(|statement| statement.outcome == StatementStatus::NotSent));
    assert_eq!(
        observer
            .query_row("SELECT label FROM records WHERE id=1", [], |row| row
                .get::<_, String>(0))
            .unwrap(),
        "original"
    );
    assert_eq!(review.changes[0].draft[0].value, text("draft"));
}
struct CommitFault {
    inner: Box<dyn DriverConnection>,
    count: Arc<AtomicUsize>,
    pause: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
}
#[async_trait]
impl DriverFactory for CommitFaultFactory {
    async fn connect(
        &self,
        descriptor: &ConnectDescriptor,
        login: DriverLogin,
        mode: Mode,
    ) -> Result<Box<dyn DriverConnection>, WorkbenchError> {
        Ok(Box::new(CommitFault {
            inner: NativeDriverFactory::default()
                .connect(descriptor, login, mode)
                .await?,
            count: self.count.clone(),
            pause: None,
        }))
    }
}
#[async_trait]
impl DriverConnection for CommitFault {
    fn facts(&self) -> &restoreplane_workbench::driver::DriverFacts {
        self.inner.facts()
    }
    fn cancel_handle(&self) -> Arc<dyn restoreplane_workbench::driver::DriverCancel> {
        self.inner.cancel_handle()
    }
    async fn set_read_only(&mut self, read_only: bool) -> Result<(), WorkbenchError> {
        self.inner.set_read_only(read_only).await
    }
    async fn configure_limits(
        &mut self,
        duration: std::time::Duration,
    ) -> Result<(), WorkbenchError> {
        self.inner.configure_limits(duration).await
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
        self.inner
            .execute_bound(sql, parameters, limits, sink)
            .await
    }
    async fn transaction_control(
        &mut self,
        command: TransactionCommand,
    ) -> Result<(), WorkbenchError> {
        if matches!(command, TransactionCommand::ManualBegin { .. }) {
            if let Some((started, release)) = &self.pause {
                started.notify_one();
                release.notified().await;
            }
        }
        if matches!(command, TransactionCommand::Commit) {
            self.count.fetch_add(1, Ordering::SeqCst);
            self.inner.transaction_control(command).await?;
            Err(WorkbenchError::CancelUnsettled)
        } else {
            self.inner.transaction_control(command).await
        }
    }
    async fn close(&mut self) -> Result<(), WorkbenchError> {
        self.inner.close().await
    }
}
struct PauseBeginFactory {
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}
#[async_trait]
impl DriverFactory for PauseBeginFactory {
    async fn connect(
        &self,
        descriptor: &ConnectDescriptor,
        login: DriverLogin,
        mode: Mode,
    ) -> Result<Box<dyn DriverConnection>, WorkbenchError> {
        Ok(Box::new(CommitFault {
            inner: NativeDriverFactory::default()
                .connect(descriptor, login, mode)
                .await?,
            count: Arc::new(AtomicUsize::new(0)),
            pause: Some((self.started.clone(), self.release.clone())),
        }))
    }
}
#[tokio::test]
async fn manual_opening_is_observable_and_independently_excludes_generated_editing() {
    use restoreplane_domain::workbench_query::{TransactionBeginInput, TransactionInput};
    let source = support::Fixture::sqlite_mode(
        "CREATE TABLE records(id INTEGER PRIMARY KEY,label TEXT,payload BLOB)",
        Mode::ReadWrite,
    )
    .await;
    let descriptor = source
        .runtime
        .binding_receipt(&source.context, source.session, AuthorizePurpose::Read)
        .await
        .unwrap()
        .descriptor;
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let harness = Harness::new(
        support::Fixture::connect_with_factory(
            descriptor,
            String::new(),
            Zeroizing::new(String::new()),
            Mode::ReadWrite,
            Arc::new(PauseBeginFactory {
                started: started.clone(),
                release: release.clone(),
            }),
        )
        .await,
    )
    .await;
    let relation = harness.relation("records").await;
    let coordinator = harness.database.coordinator.clone();
    let ctx = harness.ctx().clone();
    let session = harness.session();
    let opening = tokio::spawn(async move {
        coordinator
            .transaction_begin(
                &ctx,
                TransactionBeginInput {
                    session,
                    isolation: "serializable".into(),
                    read_only: false,
                },
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    let transaction = harness
        .database
        .coordinator
        .transaction_get(harness.ctx(), session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(transaction.state, TransactionState::Opening);
    let error = harness
        .edits
        .prepare(
            harness.ctx(),
            EditPrepareInput {
                session,
                object: relation.object.clone(),
                changes: vec![EditChange::Insert {
                    values: vec![
                        value(&relation, "id", integer(1)),
                        value(&relation, "label", TypedCell::Null),
                        value(&relation, "payload", TypedCell::Null),
                    ],
                }],
            },
        )
        .await
        .err()
        .unwrap();
    assert_eq!(
        error,
        WorkbenchError::EditRefused("manual_transaction_active".into())
    );
    release.notify_one();
    let transaction = opening.await.unwrap().unwrap();
    assert_eq!(transaction.state, TransactionState::Active);
    let rollback = harness
        .database
        .coordinator
        .transaction_rollback(
            harness.ctx(),
            TransactionInput {
                session,
                transaction_id: transaction.transaction_id,
            },
        )
        .await
        .unwrap();
    assert_eq!(rollback.state, TransactionState::RolledBack);
}
#[tokio::test]
async fn positive_commit_discarded_ack_preserves_unknown_and_never_reapplies_after_acknowledgment()
{
    let source = support::Fixture::sqlite_mode(
        "CREATE TABLE records(id INTEGER PRIMARY KEY,label TEXT,payload BLOB)",
        Mode::ReadWrite,
    )
    .await;
    let descriptor = source
        .runtime
        .binding_receipt(&source.context, source.session, AuthorizePurpose::Read)
        .await
        .unwrap()
        .descriptor;
    let count = Arc::new(AtomicUsize::new(0));
    let harness = Harness::new(
        support::Fixture::connect_with_factory(
            descriptor,
            String::new(),
            Zeroizing::new(String::new()),
            Mode::ReadWrite,
            Arc::new(CommitFaultFactory {
                count: count.clone(),
            }),
        )
        .await,
    )
    .await;
    let relation = harness.relation("records").await;
    let review = harness
        .review(
            relation.object.clone(),
            vec![EditChange::Insert {
                values: vec![
                    value(&relation, "id", integer(1)),
                    value(&relation, "label", text("once")),
                    value(&relation, "payload", TypedCell::Null),
                ],
            }],
        )
        .await;
    let input = harness.input(&review);
    let token = harness
        .edits
        .prepare_apply(harness.ctx(), input.clone())
        .await
        .unwrap();
    assert!(matches!(
        harness
            .edits
            .apply(harness.ctx(), token.attempt_token.clone(), input.clone())
            .await,
        Err(WorkbenchError::CancelUnsettled)
    ));
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let observer = rusqlite::Connection::open(&source.path).unwrap();
    assert_eq!(
        observer
            .query_row("SELECT COUNT(*) FROM records", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    let operations = harness
        .database
        .journal
        .list(
            harness.ctx(),
            &harness.database.database.scope,
            None,
            None,
            100,
        )
        .unwrap();
    let operation = operations
        .iter()
        .find(|operation| operation.kind == OperationKind::VerifiedApply)
        .unwrap();
    assert_eq!(operation.state, OperationState::OutcomeUnknown);
    harness
        .database
        .journal
        .acknowledge_unknown(
            harness.ctx(),
            &harness.database.database.scope,
            operation.operation_id,
            operation.version.value(),
        )
        .unwrap();
    let duplicate = harness
        .edits
        .apply(harness.ctx(), token.attempt_token.clone(), input.clone())
        .await
        .unwrap();
    assert!(duplicate.deduplicated);
    assert_eq!(duplicate.operation_id, operation.operation_id);
    // Removing every volatile review has the same cold-cache boundary as TTL
    // expiry. The durable status path must remain available without SQL.
    harness
        .edits
        .retire_session(harness.ctx(), harness.session())
        .unwrap();
    let cold = harness
        .edits
        .apply(harness.ctx(), token.attempt_token.clone(), input.clone())
        .await
        .unwrap();
    assert!(cold.deduplicated);
    assert_eq!(cold.operation_id, operation.operation_id);
    let mut changed = input.clone();
    changed
        .confirmed_warnings
        .push("changed_public_payload".into());
    assert!(matches!(
        harness
            .edits
            .apply(harness.ctx(), token.attempt_token.clone(), changed)
            .await,
        Err(WorkbenchError::IdempotencyConflict)
    ));
    let foreign = InvocationContext::new(harness.ctx().principal().clone(), Uuid::new_v4());
    assert!(matches!(
        harness
            .edits
            .apply(&foreign, token.attempt_token.clone(), input.clone())
            .await,
        Err(WorkbenchError::Forbidden)
    ));
    let mut changed_generation = input.clone();
    changed_generation.session.connection_generation =
        (input.session.connection_generation.value() + 1).into();
    assert!(matches!(
        harness
            .edits
            .apply(harness.ctx(), token.attempt_token, changed_generation)
            .await,
        Err(WorkbenchError::IdempotencyConflict)
    ));
    assert!(matches!(
        harness.edits.prepare_apply(harness.ctx(), input).await,
        Err(WorkbenchError::ResultExpired)
    ));
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(
        harness.operation(operation.operation_id).state,
        OperationState::OutcomeUnknown
    );
    let dispatches: i64 = harness
        .database
        .store
        .read(|connection| {
            connection
                .query_row(
                    "SELECT dispatch_count FROM operations WHERE operation_id=?1",
                    [operation.operation_id.to_string()],
                    |row| row.get(0),
                )
                .map_err(|_| WorkbenchError::StoreUnavailable)
        })
        .unwrap();
    assert_eq!(dispatches, 1);
}
#[derive(Default)]
struct Collect {
    rows: Vec<Vec<TypedCell>>,
}
#[async_trait]
impl RowSink for Collect {
    async fn columns(&mut self, _: Vec<Column>) -> Result<(), WorkbenchError> {
        Ok(())
    }
    async fn row(&mut self, row: Vec<TypedCell>) -> Result<(), WorkbenchError> {
        if self.rows.len() >= 200 {
            return Err(WorkbenchError::Limit);
        }
        self.rows.push(row);
        Ok(())
    }
}
async fn direct(connection: &mut dyn DriverConnection, sql: &str) -> Vec<Vec<TypedCell>> {
    let mut sink = Collect::default();
    connection
        .execute(
            sql,
            QueryLimits {
                max_rows: 200,
                max_bytes: 512 * 1024,
                ..QueryLimits::default()
            },
            &mut sink,
        )
        .await
        .unwrap_or_else(|error| panic!("owned fixture SQL {}", error.code()));
    sink.rows
}
#[tokio::test]
#[ignore = "owned verified TLS PostgreSQL/MySQL/MariaDB fixture matrix; no external database"]
async fn tls_three_engines_verified_insert_update_delete_noop_and_decimal_bytes_are_observed() {
    let bytes = Zeroizing::new(
        std::fs::read(std::env::var_os("WORKBENCH_LIVE_FIXTURES").expect("owned fixture required"))
            .unwrap(),
    );
    let config: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    for (name, engine) in [
        ("postgresql", Engine::Postgresql),
        ("mysql", Engine::Mysql),
        ("mariadb", Engine::Mariadb),
    ] {
        let data = &config[name];
        let ca = data["ca_file"].as_str().unwrap();
        let descriptor = ConnectDescriptor {
            engine,
            route: Route::Tcp {
                host: data["host"].as_str().unwrap().into(),
                port: u16::try_from(data["port"].as_u64().unwrap()).unwrap(),
            },
            database: DatabaseIdentity {
                engine_object_id: "fixture".into(),
                name: data["database"].as_str().unwrap().into(),
                source_fingerprint: "fixture".into(),
            },
            tls: Tls {
                mode: "verify-full".into(),
                server_name: None,
                root_trust: Some(RootTrust::Custom {
                    canonical_path: ca.into(),
                    content_digest: blake3::hash(&std::fs::read(ca).unwrap())
                        .to_hex()
                        .to_string(),
                }),
            },
            transport_options: std::collections::BTreeMap::new(),
            expected_server_identity: None,
        };
        let username = data["username"].as_str().unwrap().to_owned();
        let secret = Zeroizing::new(data["password"].as_str().unwrap().to_owned());
        let mut observer = NativeDriverFactory::default()
            .connect(
                &descriptor,
                DriverLogin {
                    username: username.clone(),
                    secret: secret.clone(),
                },
                Mode::Full,
            )
            .await
            .unwrap();
        if name == "mariadb" {
            if let Ok(owned) = std::env::var("WORKBENCH_OWNED_WB06_CLEANUP") {
                let suffix = owned
                    .strip_prefix("wb06_")
                    .expect("cleanup is restricted to a recorded owned fixture");
                assert_eq!(suffix.len(), 32);
                assert!(suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
                direct(
                    observer.as_mut(),
                    &format!(
                        "DROP TABLE IF EXISTS {}",
                        restoreplane_workbench::types::quote_identifier(engine, &owned).unwrap()
                    ),
                )
                .await;
            }
        }
        if name == "postgresql" {
            if let Ok(owned) = std::env::var("WORKBENCH_OWNED_WB06_PG_DOMAIN_CLEANUP") {
                let suffix = owned
                    .strip_prefix("wb06_")
                    .expect("recorded owned fixture only");
                assert_eq!(suffix.len(), 32);
                assert!(suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
                direct(
                    observer.as_mut(),
                    &format!("DROP TABLE IF EXISTS \"{owned}_shadow\""),
                )
                .await;
                direct(
                    observer.as_mut(),
                    &format!("DROP DOMAIN IF EXISTS \"{owned}_types\".int8"),
                )
                .await;
                direct(
                    observer.as_mut(),
                    &format!("DROP SCHEMA IF EXISTS \"{owned}_types\""),
                )
                .await;
                direct(
                    observer.as_mut(),
                    &format!("DROP TABLE IF EXISTS \"{owned}\""),
                )
                .await;
            }
        }
        let table = format!("wb06_{}", Uuid::new_v4().simple());
        println!("06 owned temporary fixture engine={name} table={table}");
        let binary = if engine == Engine::Postgresql {
            "BYTEA"
        } else {
            "BLOB"
        };
        direct(observer.as_mut(),&format!("CREATE TABLE {table}(id BIGINT PRIMARY KEY,label VARCHAR(80),payload {binary},amount DECIMAL(30,12))")).await;
        let harness = Harness::new(
            support::Fixture::connect(descriptor, username, secret, Mode::ReadWrite).await,
        )
        .await;
        let relation = harness.relation(&table).await;
        let decimal = TypedCell::Decimal {
            value: "123456789012345678.123456789012".into(),
            scale: Some(12),
        };
        let review = harness
            .review(
                relation.object.clone(),
                vec![EditChange::Insert {
                    values: vec![
                        value(&relation, "id", integer(i64::MAX)),
                        value(&relation, "label", TypedCell::Null),
                        value(
                            &relation,
                            "payload",
                            TypedCell::Bytes {
                                base64: "AP8=".into(),
                            },
                        ),
                        value(&relation, "amount", decimal.clone()),
                    ],
                }],
            )
            .await;
        let inserted = harness
            .apply(&review)
            .await
            .unwrap_or_else(|error| panic!("{name} native insert {}", error.code()));
        assert_eq!(
            harness.operation(inserted.operation_id).state,
            OperationState::Committed
        );
        let rows = harness.rows(relation.object.clone()).await;
        assert_eq!(rows.page.items.len(), 1);
        assert_eq!(rows.page.items[0].cells[0], integer(i64::MAX));
        assert_eq!(rows.page.items[0].cells[1], TypedCell::Null);
        assert_eq!(
            rows.page.items[0].cells[2],
            TypedCell::Bytes {
                base64: "AP8=".into()
            }
        );
        assert_eq!(rows.page.items[0].cells[3], decimal);
        let noop = harness
            .review(
                relation.object.clone(),
                vec![EditChange::Update {
                    row_handle: rows.page.items[0].row_handle,
                    values: vec![value(&relation, "label", TypedCell::Null)],
                }],
            )
            .await;
        let noop = harness.apply(&noop).await.unwrap();
        let op = harness.operation(noop.operation_id);
        assert_eq!(op.statements[0].rows_matched.unwrap().value(), 1);
        assert_eq!(op.statements[0].rows_changed.unwrap().value(), 0);
        let update = harness
            .review(
                relation.object.clone(),
                vec![EditChange::Update {
                    row_handle: rows.page.items[0].row_handle,
                    values: vec![value(&relation, "label", text("한글 '; -- literal"))],
                }],
            )
            .await;
        harness.apply(&update).await.unwrap();
        let actual = direct(observer.as_mut(), &format!("SELECT label FROM {table}")).await;
        assert_eq!(actual, vec![vec![text("한글 '; -- literal")]]);
        let fresh = harness.rows(relation.object.clone()).await;
        let delete = harness
            .review(
                relation.object.clone(),
                vec![EditChange::Delete {
                    row_handle: fresh.page.items[0].row_handle,
                }],
            )
            .await;
        harness.apply(&delete).await.unwrap();
        assert!(
            direct(observer.as_mut(), &format!("SELECT id FROM {table}"))
                .await
                .is_empty()
        );
        if engine == Engine::Postgresql {
            let schema = format!("{table}_types");
            let shadow = format!("{table}_shadow");
            direct(observer.as_mut(), &format!("CREATE SCHEMA {schema}")).await;
            direct(
                observer.as_mut(),
                &format!("CREATE TYPE {schema}.int8 AS ENUM ('00000007')"),
            )
            .await;
            direct(
                observer.as_mut(),
                &format!("CREATE TABLE {shadow}(id BIGINT PRIMARY KEY,typed {schema}.int8)"),
            )
            .await;
            direct(
                observer.as_mut(),
                &format!("INSERT INTO {shadow} VALUES(1,'00000007')"),
            )
            .await;
            let metadata = harness.relation(&shadow).await;
            let column = metadata
                .columns
                .iter()
                .find(|c| c.column.name == "typed")
                .unwrap();
            assert_eq!(column.declared_type, "int8");
            assert_eq!(column.native_type_schema.as_deref(), Some(schema.as_str()));
            assert!(generated_sql::validate_column_cell(engine, column, &TypedCell::Null).is_err());
            assert!(matches!(
                &direct(observer.as_mut(), &format!("SELECT typed FROM {shadow}")).await[0][0],
                TypedCell::Unsupported { .. }
            ));
            let input = EditPrepareInput {
                session: harness.session(),
                object: metadata.object.clone(),
                changes: vec![EditChange::Insert {
                    values: vec![
                        value(&metadata, "id", integer(2)),
                        value(&metadata, "typed", integer(8)),
                    ],
                }],
            };
            assert_eq!(
                harness
                    .edits
                    .prepare(harness.ctx(), input)
                    .await
                    .err()
                    .unwrap(),
                WorkbenchError::EditRefused("unsupported_type".into())
            );
            direct(observer.as_mut(), &format!("DROP TABLE {shadow}")).await;
            direct(observer.as_mut(), &format!("DROP TYPE {schema}.int8")).await;
            direct(observer.as_mut(), &format!("DROP SCHEMA {schema}")).await;
        } else {
            let unsigned = format!("{table}_unsigned");
            direct(observer.as_mut(),&format!("CREATE TABLE {unsigned}(id BIGINT UNSIGNED PRIMARY KEY,label VARCHAR(80)) ENGINE=InnoDB")).await;
            let metadata = harness.relation(&unsigned).await;
            let review = harness
                .review(
                    metadata.object.clone(),
                    vec![EditChange::Insert {
                        values: vec![
                            value(
                                &metadata,
                                "id",
                                TypedCell::U64 {
                                    value: u64::MAX.to_string(),
                                },
                            ),
                            value(&metadata, "label", TypedCell::Null),
                        ],
                    }],
                )
                .await;
            harness.apply(&review).await.unwrap();
            let rows = harness.rows(metadata.object.clone()).await;
            assert_eq!(
                rows.page.items[0].cells[0],
                TypedCell::U64 {
                    value: u64::MAX.to_string()
                }
            );
            let update = harness
                .review(
                    metadata.object.clone(),
                    vec![EditChange::Update {
                        row_handle: rows.page.items[0].row_handle,
                        values: vec![value(&metadata, "label", text("unsigned exact key"))],
                    }],
                )
                .await;
            harness.apply(&update).await.unwrap();
            assert_eq!(
                direct(observer.as_mut(), &format!("SELECT label FROM {unsigned}")).await,
                vec![vec![text("unsigned exact key")]]
            );
            direct(observer.as_mut(), &format!("DROP TABLE {unsigned}")).await;
            let nontransactional = format!("{table}_myisam");
            direct(observer.as_mut(),&format!("CREATE TABLE {nontransactional}(id BIGINT PRIMARY KEY,label VARCHAR(80)) ENGINE=MyISAM")).await;
            let metadata = harness.relation(&nontransactional).await;
            let input = EditPrepareInput {
                session: harness.session(),
                object: metadata.object.clone(),
                changes: vec![EditChange::Insert {
                    values: vec![
                        value(&metadata, "id", integer(1)),
                        value(&metadata, "label", TypedCell::Null),
                    ],
                }],
            };
            assert_eq!(
                harness
                    .edits
                    .prepare(harness.ctx(), input)
                    .await
                    .err()
                    .unwrap(),
                WorkbenchError::EditRefused("non_transactional_storage".into())
            );
            assert!(direct(
                observer.as_mut(),
                &format!("SELECT id FROM {nontransactional}")
            )
            .await
            .is_empty());
            direct(observer.as_mut(), &format!("DROP TABLE {nontransactional}")).await;
        }
        harness
            .database
            .database
            .runtime
            .close(harness.ctx(), harness.session())
            .await
            .unwrap();
        direct(observer.as_mut(), &format!("DROP TABLE {table}")).await;
        observer.close().await.unwrap();
        println!("06 owned {name}: verifiedTLS standalone insert/update/delete, NULL/empty separation, i64max/exactdecimal/00ffbytes, skipped no-op matched1 changed0; no production certification promotion in test");
    }
}
