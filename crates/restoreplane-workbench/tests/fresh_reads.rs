mod support;
use async_trait::async_trait;
use restoreplane_domain::workbench::{Column, TypedCell};
use restoreplane_domain::workbench_metadata::Consistency;
use restoreplane_domain::workbench_query::{QueryPurpose, QueryReviewInput};
use restoreplane_domain::workbench_stream::Completion;
use restoreplane_workbench::browse::{BrowseService, SnapshotCertification};
use restoreplane_workbench::driver::{FileReadLimits, RowSink};
use restoreplane_workbench::metadata::MetadataCatalog;
use restoreplane_workbench::query::ApprovedExportRead;
use restoreplane_workbench::WorkbenchError;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<Vec<TypedCell>>>>);
#[async_trait]
impl RowSink for Sink {
    async fn columns(&mut self, _: Vec<Column>) -> Result<(), WorkbenchError> {
        Ok(())
    }
    async fn row(&mut self, row: Vec<TypedCell>) -> Result<(), WorkbenchError> {
        self.0.lock().unwrap().push(row);
        Ok(())
    }
}
fn services(fixture: &support::QueryFixture) -> (Arc<MetadataCatalog>, Arc<BrowseService>) {
    let metadata = Arc::new(MetadataCatalog::new(
        fixture.database.runtime.clone(),
        fixture.streams.budget(),
    ));
    let certification = SnapshotCertification::from_verified_build(
        "local-native-test".into(),
        vec![(
            restoreplane_domain::workbench::Engine::Sqlite,
            rusqlite::version().into(),
        )],
    )
    .unwrap();
    let browse = Arc::new(BrowseService::with_snapshot_certification(
        metadata.clone(),
        fixture.streams.clone(),
        certification,
    ));
    (metadata, browse)
}
async fn approval(
    fixture: &support::QueryFixture,
    sql: &str,
    snapshot: Option<uuid::Uuid>,
    metadata: Arc<MetadataCatalog>,
    browse: Arc<BrowseService>,
) -> Result<Arc<ApprovedExportRead>, WorkbenchError> {
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
        .await?;
    fixture
        .coordinator
        .approve_export_read(
            &fixture.database.context,
            fixture.database.session,
            review.review_id,
            snapshot,
            metadata,
            browse,
        )
        .await
}
#[tokio::test]
async fn original_read_is_single_use_streamed_capped_and_releases_physical_pin() {
    let fixture=support::QueryFixture::new(support::Fixture::sqlite("CREATE TABLE records(id INTEGER PRIMARY KEY,label TEXT);INSERT INTO records VALUES(1,NULL),(2,''),(3,'한글')").await);
    let (metadata, browse) = services(&fixture);
    let approved = approval(
        &fixture,
        "SELECT id,label FROM main.records;",
        None,
        metadata,
        browse,
    )
    .await
    .unwrap();
    let sink = Sink::default();
    let result = approved
        .execute(
            &fixture.database.context,
            Box::new(sink.clone()),
            FileReadLimits::bounded(2, 64 * 1024 * 1024, std::time::Duration::from_secs(30))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(result.completeness, Completion::Capped);
    assert!(matches!(result.consistency, Consistency::LivePerPage));
    assert_eq!(result.summary.rows_kept, 2);
    assert_eq!(sink.0.lock().unwrap().len(), 2);
    assert!(matches!(sink.0.lock().unwrap()[0][1], TypedCell::Null));
    assert!(matches!(
        approved
            .execute(
                &fixture.database.context,
                Box::new(Sink::default()),
                FileReadLimits::default()
            )
            .await,
        Err(WorkbenchError::AttemptExpired)
    ));
    fixture
        .database
        .runtime
        .with_connection(
            &fixture.database.context,
            fixture.database.session,
            restoreplane_domain::workbench::AuthorizePurpose::Read,
            |connection| {
                Box::pin(async move {
                    // The completed standalone scope released its transaction;
                    // a subsequent native request can acquire the same socket.
                    let mut sink = Sink::default();
                    let result = connection
                        .execute(
                            "SELECT 1",
                            restoreplane_workbench::driver::QueryLimits::default(),
                            &mut sink,
                        )
                        .await?;
                    assert_eq!(
                        result.transaction_state,
                        restoreplane_workbench::driver::DriverTransactionState::Idle
                    );
                    Ok(())
                })
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn timeout_and_cancel_never_complete_and_interactive_hardmax_is_unchanged() {
    assert!(restoreplane_workbench::driver::QueryLimits {
        max_bytes: 65 * 1024 * 1024,
        ..Default::default()
    }
    .validate()
    .is_err());
    for cancel in [true, false] {
        let fixture = support::QueryFixture::new(
            support::Fixture::sqlite(
                "CREATE TABLE records(id INTEGER PRIMARY KEY);INSERT INTO records VALUES(1),(2)",
            )
            .await,
        );
        let (metadata, browse) = services(&fixture);
        let approved = approval(
            &fixture,
            "SELECT * FROM main.records",
            None,
            metadata,
            browse,
        )
        .await
        .unwrap();
        struct SlowSink;
        #[async_trait]
        impl RowSink for SlowSink {
            async fn columns(&mut self, _: Vec<Column>) -> Result<(), WorkbenchError> {
                Ok(())
            }
            async fn row(&mut self, _: Vec<TypedCell>) -> Result<(), WorkbenchError> {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                Ok(())
            }
        }
        if cancel {
            assert!(
                approved
                    .cancel(&fixture.database.context)
                    .await
                    .unwrap()
                    .cancel_requested
            );
        }
        let result = approved
            .execute(
                &fixture.database.context,
                Box::new(SlowSink),
                FileReadLimits::bounded(
                    100,
                    64 * 1024 * 1024,
                    std::time::Duration::from_millis(30),
                )
                .unwrap(),
            )
            .await;
        assert!(result.is_err());
        assert!(approved
            .execute(
                &fixture.database.context,
                Box::new(Sink::default()),
                FileReadLimits::default()
            )
            .await
            .is_err());
    }
}
#[tokio::test]
async fn view_virtual_complex_and_function_reads_are_refused_before_any_source_row() {
    let fixture=support::QueryFixture::new(support::Fixture::sqlite("CREATE TABLE records(id INTEGER PRIMARY KEY);CREATE VIEW derived AS SELECT id FROM records;CREATE VIRTUAL TABLE virtual_rows USING fts5(label)").await);
    let (metadata, browse) = services(&fixture);
    for sql in [
        "SELECT * FROM main.derived",
        "SELECT * FROM main.virtual_rows",
        "SELECT abs(id) FROM main.records",
        "SELECT * FROM main.records WHERE id=1",
        "SELECT * FROM records",
    ] {
        assert!(
            approval(&fixture, sql, None, metadata.clone(), browse.clone())
                .await
                .is_err(),
            "must refuse {sql}"
        );
    }
}
#[tokio::test]
async fn schema_changed_after_approval_never_streams_rows() {
    let fixture = support::QueryFixture::new(
        support::Fixture::sqlite("CREATE TABLE records(id INTEGER PRIMARY KEY)").await,
    );
    let (metadata, browse) = services(&fixture);
    let approved = approval(
        &fixture,
        "SELECT * FROM main.records",
        None,
        metadata,
        browse,
    )
    .await
    .unwrap();
    rusqlite::Connection::open(&fixture.database.path)
        .unwrap()
        .execute_batch("ALTER TABLE records ADD COLUMN changed TEXT")
        .unwrap();
    let sink = Sink::default();
    assert!(approved
        .execute(
            &fixture.database.context,
            Box::new(sink.clone()),
            FileReadLimits::default()
        )
        .await
        .is_err());
    assert!(sink.0.lock().unwrap().is_empty());
}
#[tokio::test]
async fn certified_snapshot_is_explicit_and_ended_snapshot_never_falls_back_to_live() {
    let fixture = support::QueryFixture::new(
        support::Fixture::sqlite(
            "CREATE TABLE records(id INTEGER PRIMARY KEY);INSERT INTO records VALUES(1)",
        )
        .await,
    );
    let (metadata, browse) = services(&fixture);
    let review = fixture
        .coordinator
        .review(
            &fixture.database.context,
            QueryReviewInput {
                session: fixture.database.session,
                sql: "SELECT * FROM main.records".into(),
                purpose: QueryPurpose::Run,
            },
        )
        .await
        .unwrap();
    let (id, _) = browse
        .snapshot_begin(
            &fixture.database.context,
            fixture.database.session,
            "repeatable_read",
            60_000,
        )
        .await
        .unwrap();
    let approved = fixture
        .coordinator
        .approve_export_read(
            &fixture.database.context,
            fixture.database.session,
            review.review_id,
            Some(id),
            metadata.clone(),
            browse.clone(),
        )
        .await
        .unwrap();
    let result = approved
        .execute(
            &fixture.database.context,
            Box::new(Sink::default()),
            FileReadLimits::default(),
        )
        .await
        .unwrap();
    assert_eq!(result.completeness, Completion::Complete);
    assert!(
        matches!(result.consistency,Consistency::TransactionSnapshot{snapshot_id,..} if snapshot_id==id)
    );
    browse
        .snapshot_end(&fixture.database.context, fixture.database.session, id)
        .await
        .unwrap();
    assert!(matches!(
        fixture
            .coordinator
            .approve_export_read(
                &fixture.database.context,
                fixture.database.session,
                review.review_id,
                Some(id),
                metadata,
                browse
            )
            .await,
        Err(WorkbenchError::ResultExpired)
    ));
}

#[tokio::test]
async fn file_read_totals_exceed_ui_retention_while_native_sink_stays_bounded() {
    let fixture = support::QueryFixture::new(
        support::Fixture::sqlite("CREATE TABLE large_rows(id INTEGER PRIMARY KEY,payload TEXT)")
            .await,
    );
    let mut source = rusqlite::Connection::open(&fixture.database.path).unwrap();
    let transaction = source.transaction().unwrap();
    {
        let mut insert = transaction
            .prepare("INSERT INTO large_rows VALUES(?1,?2)")
            .unwrap();
        let value = "a".repeat(12 * 1024);
        for id in 0..7000i64 {
            insert.execute(rusqlite::params![id, value]).unwrap();
        }
    }
    transaction.commit().unwrap();
    drop(source);
    let (metadata, browse) = services(&fixture);
    let approved = approval(
        &fixture,
        "SELECT * FROM main.large_rows",
        None,
        metadata,
        browse,
    )
    .await
    .unwrap();
    struct CountingSink {
        budget: Arc<restoreplane_workbench::result_stream::NativeBudget>,
        rows: Arc<std::sync::atomic::AtomicUsize>,
        ceiling: usize,
    }
    #[async_trait]
    impl RowSink for CountingSink {
        async fn columns(&mut self, _: Vec<Column>) -> Result<(), WorkbenchError> {
            Ok(())
        }
        async fn row(&mut self, row: Vec<TypedCell>) -> Result<(), WorkbenchError> {
            let bytes = restoreplane_workbench::result_stream::encoded_size(&row, 256 * 1024)?;
            let _reservation = self.budget.reserve(bytes * 2)?;
            assert!(self.budget.retained_bytes() <= self.ceiling);
            self.rows.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }
    let rows = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    // ResultStreams already reserves its fixed 8MiB worker headroom. The
    // streaming source/sink must plateau above that baseline rather than grow
    // with the >64MiB cumulative result.
    let baseline = fixture.streams.budget().retained_bytes();
    let ceiling = baseline + 512 * 1024;
    let result = approved
        .execute(
            &fixture.database.context,
            Box::new(CountingSink {
                budget: fixture.streams.budget(),
                rows: rows.clone(),
                ceiling,
            }),
            FileReadLimits::default(),
        )
        .await
        .unwrap();
    assert_eq!(result.completeness, Completion::Complete);
    assert!(result.summary.encoded_bytes > 64 * 1024 * 1024);
    assert_eq!(rows.load(std::sync::atomic::Ordering::SeqCst), 7000);
    assert!(fixture.streams.budget().retained_bytes() <= ceiling);
}

#[tokio::test]
#[ignore = "owned verified TLS PostgreSQL/MySQL/MariaDB exact native fresh read"]
async fn tls_three_engines_native_fresh_read_completes_and_rls_origin_is_refused() {
    use restoreplane_domain::workbench::{
        ConnectDescriptor, DatabaseIdentity, Engine, Mode, RootTrust, Route, Tls,
    };
    use restoreplane_workbench::driver::{
        DriverFactory, DriverLogin, NativeDriverFactory, QueryLimits,
    };
    use zeroize::Zeroizing;
    let config_bytes = Zeroizing::new(
        std::fs::read(
            std::env::var_os("WORKBENCH_LIVE_FIXTURES").expect("owned fixture config required"),
        )
        .unwrap(),
    );
    let config: serde_json::Value = serde_json::from_slice(&config_bytes).unwrap();
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
        let table = format!("wb07_read_{}", uuid::Uuid::new_v4().simple());
        let schema = if engine == Engine::Postgresql {
            "public"
        } else {
            data["database"].as_str().unwrap()
        };
        let qualified = format!("{schema}.{table}");
        for sql in [
            format!("CREATE TABLE {qualified}(id BIGINT PRIMARY KEY,label VARCHAR(80))"),
            format!("INSERT INTO {qualified} VALUES(1,NULL),(2,''),(3,'한글')"),
        ] {
            observer
                .execute(&sql, QueryLimits::default(), &mut Sink::default())
                .await
                .unwrap();
        }
        let fixture = support::QueryFixture::new(
            support::Fixture::connect(descriptor, username, secret, Mode::ReadOnly).await,
        );
        let (metadata, browse) = services(&fixture);
        let approved = approval(
            &fixture,
            &format!("SELECT id,label FROM {qualified}"),
            None,
            metadata.clone(),
            browse.clone(),
        )
        .await
        .unwrap();
        let sink = Sink::default();
        let outcome = approved
            .execute(
                &fixture.database.context,
                Box::new(sink.clone()),
                FileReadLimits::default(),
            )
            .await
            .unwrap();
        assert_eq!(outcome.completeness, Completion::Complete);
        assert_eq!(outcome.summary.rows_kept, 3);
        assert_eq!(
            sink.0.lock().unwrap().as_slice(),
            &[
                vec![TypedCell::I64 { value: "1".into() }, TypedCell::Null],
                vec![
                    TypedCell::I64 { value: "2".into() },
                    TypedCell::Text {
                        value: String::new()
                    }
                ],
                vec![
                    TypedCell::I64 { value: "3".into() },
                    TypedCell::Text {
                        value: "한글".into()
                    }
                ],
            ]
        );
        if engine == Engine::Postgresql {
            observer
                .execute(
                    &format!("ALTER TABLE {qualified} ENABLE ROW LEVEL SECURITY"),
                    QueryLimits::default(),
                    &mut Sink::default(),
                )
                .await
                .unwrap();
            assert!(matches!(
                approval(
                    &fixture,
                    &format!("SELECT * FROM {qualified}"),
                    None,
                    metadata,
                    browse
                )
                .await,
                Err(WorkbenchError::Unsupported)
            ));
        }
        observer
            .execute(
                &format!("DROP TABLE {qualified}"),
                QueryLimits::default(),
                &mut Sink::default(),
            )
            .await
            .unwrap();
        println!("07 native fresh verifiedTLS engine={name} sourceRows=3 complete=true exactTypedRows=true readonlyCleanup=true");
    }
}
