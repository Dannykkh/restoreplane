mod support;
use async_trait::async_trait;
use restoreplane_domain::workbench::{
    AuthorizePurpose, Column, ConnectDescriptor, DatabaseIdentity, Engine, Mode, RootTrust, Route,
    Tls, TypedCell,
};
use restoreplane_domain::workbench_metadata::{
    CatalogKind, Consistency, Filter, Limits, ObjectRef, PageInput,
};
use restoreplane_workbench::browse::{BrowseInput, BrowseService, SnapshotCertification};
use restoreplane_workbench::driver::{
    DriverConnection, DriverFactory, DriverLogin, NativeDriverFactory, QueryLimits, RowSink,
};
use restoreplane_workbench::metadata::MetadataCatalog;
use restoreplane_workbench::result_stream::ResultStreams;
use restoreplane_workbench::WorkbenchError;
use std::collections::BTreeMap;
use std::sync::Arc;
use zeroize::Zeroizing;

#[derive(Default)]
struct Sink {
    rows: Vec<Vec<TypedCell>>,
}
struct RestoreSettingFaultFactory {
    enabled: Arc<std::sync::atomic::AtomicBool>,
    closed: Arc<std::sync::atomic::AtomicUsize>,
}
struct RestoreSettingFault {
    inner: Box<dyn DriverConnection>,
    enabled: Arc<std::sync::atomic::AtomicBool>,
    closed: Arc<std::sync::atomic::AtomicUsize>,
}
#[async_trait]
impl DriverFactory for RestoreSettingFaultFactory {
    async fn connect(
        &self,
        descriptor: &ConnectDescriptor,
        login: DriverLogin,
        mode: Mode,
    ) -> Result<Box<dyn DriverConnection>, WorkbenchError> {
        Ok(Box::new(RestoreSettingFault {
            inner: NativeDriverFactory::default()
                .connect(descriptor, login, mode)
                .await?,
            enabled: self.enabled.clone(),
            closed: self.closed.clone(),
        }))
    }
}
#[async_trait]
impl DriverConnection for RestoreSettingFault {
    fn facts(&self) -> &restoreplane_workbench::driver::DriverFacts {
        self.inner.facts()
    }
    fn cancel_handle(&self) -> Arc<dyn restoreplane_workbench::driver::DriverCancel> {
        self.inner.cancel_handle()
    }
    async fn set_read_only(&mut self, value: bool) -> Result<(), WorkbenchError> {
        self.inner.set_read_only(value).await
    }
    async fn execute(
        &mut self,
        sql: &str,
        limits: QueryLimits,
        sink: &mut dyn RowSink,
    ) -> Result<restoreplane_workbench::driver::DriverSummary, WorkbenchError> {
        self.inner.execute(sql, limits, sink).await
    }
    async fn execute_bound(
        &mut self,
        sql: &str,
        params: &[TypedCell],
        limits: QueryLimits,
        sink: &mut dyn RowSink,
    ) -> Result<restoreplane_workbench::driver::DriverSummary, WorkbenchError> {
        if self.enabled.load(std::sync::atomic::Ordering::SeqCst)
            && sql == "SET SESSION group_concat_max_len=777"
        {
            return Err(WorkbenchError::Io);
        }
        self.inner.execute_bound(sql, params, limits, sink).await
    }
    async fn close(&mut self) -> Result<(), WorkbenchError> {
        self.closed
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.close().await
    }
}

#[tokio::test]
#[ignore = "owned TLS MySQL/MariaDB catalog settings and injected bridge restore failure"]
async fn catalog_group_concat_setting_is_restored_and_failed_restore_retires_the_socket() {
    let bytes = Zeroizing::new(
        std::fs::read(
            std::env::var_os("WORKBENCH_LIVE_FIXTURES").expect("owned fixture config required"),
        )
        .unwrap(),
    );
    let config: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    for (name, engine) in [("mysql", Engine::Mysql), ("mariadb", Engine::Mariadb)] {
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
            transport_options: BTreeMap::new(),
            expected_server_identity: None,
        };
        let enabled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let closed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fixture = support::Fixture::connect_with_factory(
            descriptor,
            data["username"].as_str().unwrap().into(),
            Zeroizing::new(data["password"].as_str().unwrap().into()),
            Mode::ReadOnly,
            Arc::new(RestoreSettingFaultFactory {
                enabled: enabled.clone(),
                closed: closed.clone(),
            }),
        )
        .await;
        fixture
            .runtime
            .with_connection(
                &fixture.context,
                fixture.session,
                AuthorizePurpose::Read,
                |connection| {
                    Box::pin(async move {
                        run(connection, "SET SESSION group_concat_max_len=777").await;
                        Ok(())
                    })
                },
            )
            .await
            .unwrap();
        let streams = Arc::new(ResultStreams::new_pull(fixture.runtime.clone()).unwrap());
        let metadata = MetadataCatalog::new(fixture.runtime.clone(), streams.budget());
        let relation = object(&metadata, &fixture, "wb_rows").await;
        metadata
            .resolve_relation(&fixture.context, fixture.session, &relation)
            .await
            .unwrap();
        let value = fixture
            .runtime
            .with_connection(
                &fixture.context,
                fixture.session,
                AuthorizePurpose::Read,
                |connection| {
                    Box::pin(async move {
                        Ok(run(connection, "SELECT @@session.group_concat_max_len").await)
                    })
                },
            )
            .await
            .unwrap();
        assert!(
            matches!(&value[0][0],TypedCell::I64{value}|TypedCell::U64{value} if value=="777"),
            "{name}"
        );
        enabled.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(metadata
            .page(
                &fixture.context,
                fixture.session,
                None,
                CatalogKind::Relations,
                "wb_rows".into(),
                PageInput {
                    cursor: None,
                    limit: 10
                }
            )
            .await
            .is_err());
        assert_eq!(
            fixture
                .runtime
                .get(&fixture.context, fixture.session)
                .unwrap()
                .state,
            restoreplane_domain::workbench::SessionState::Retiring
        );
        assert!(closed.load(std::sync::atomic::Ordering::SeqCst) > 0);
        println!("04 {name} caller group_concat_max_len=777 preserved after list/describe; injected restoration failure retired physical connection");
    }
}
#[async_trait]
impl RowSink for Sink {
    async fn columns(&mut self, _: Vec<Column>) -> Result<(), WorkbenchError> {
        Ok(())
    }
    async fn row(&mut self, row: Vec<TypedCell>) -> Result<(), WorkbenchError> {
        assert!(self.rows.len() < 200);
        self.rows.push(row);
        Ok(())
    }
}
async fn run(connection: &mut dyn DriverConnection, sql: &str) -> Vec<Vec<TypedCell>> {
    let mut sink = Sink::default();
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
        .unwrap_or_else(|error| panic!("owned fixture SQL failed: {}", error.code()));
    sink.rows
}
async fn object(metadata: &MetadataCatalog, fixture: &support::Fixture, name: &str) -> ObjectRef {
    let page = metadata
        .page(
            &fixture.context,
            fixture.session,
            None,
            CatalogKind::Relations,
            name.into(),
            PageInput {
                cursor: None,
                limit: 100,
            },
        )
        .await
        .unwrap_or_else(|error| panic!("catalog page: {}", error.code()));
    page.items
        .into_iter()
        .find(|item| item.name == name)
        .expect("owned relation catalog item")
        .object
}
fn input(object: ObjectRef, snapshot_id: Option<uuid::Uuid>) -> BrowseInput {
    BrowseInput {
        object,
        filter: Filter::default(),
        order: Vec::new(),
        snapshot_id,
        limits: Limits {
            max_rows: 2,
            max_bytes: 512 * 1024,
            statement_timeout_ms: 30_000,
            lock_wait_ms: 2000,
        },
    }
}

#[tokio::test]
async fn sqlite_explicit_snapshot_isolation_and_ttl_are_observed_not_inferred() {
    let fixture=support::Fixture::sqlite("PRAGMA journal_mode=WAL;CREATE TABLE wb04_snapshot(id INTEGER PRIMARY KEY,label TEXT);INSERT INTO wb04_snapshot VALUES(1,'before'),(2,'second'),(3,'third')").await;
    let writer = rusqlite::Connection::open(&fixture.path).unwrap();
    let streams = Arc::new(ResultStreams::new_pull(fixture.runtime.clone()).unwrap());
    let metadata = Arc::new(MetadataCatalog::new(
        fixture.runtime.clone(),
        streams.budget(),
    ));
    let version = fixture
        .runtime
        .with_connection(
            &fixture.context,
            fixture.session,
            AuthorizePurpose::Read,
            |connection| Box::pin(async move { Ok(connection.facts().server_version.clone()) }),
        )
        .await
        .unwrap();
    let uncertified = BrowseService::new(metadata.clone(), streams.clone());
    assert!(matches!(
        uncertified
            .snapshot_begin(&fixture.context, fixture.session, "repeatable_read", 60_000)
            .await,
        Err(WorkbenchError::Unsupported)
    ));
    let browse = BrowseService::with_snapshot_certification(
        metadata.clone(),
        streams,
        SnapshotCertification::from_verified_build(
            "section04-real-sqlite-isolation".into(),
            vec![(Engine::Sqlite, version.clone())],
        )
        .unwrap(),
    );
    let relation = object(&metadata, &fixture, "wb04_snapshot").await;
    let (snapshot, _) = browse
        .snapshot_begin(&fixture.context, fixture.session, "repeatable_read", 60_000)
        .await
        .unwrap();
    let first = browse
        .open(
            &fixture.context,
            fixture.session,
            input(relation.clone(), Some(snapshot)),
            None,
        )
        .await
        .unwrap();
    assert!(matches!(
        first.result.consistency,
        Consistency::TransactionSnapshot { .. }
    ));
    writer
        .execute("UPDATE wb04_snapshot SET label='after' WHERE id=1", [])
        .unwrap();
    let again = browse
        .open(
            &fixture.context,
            fixture.session,
            input(relation.clone(), Some(snapshot)),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        again.result.page.items[0].cells[1],
        TypedCell::Text {
            value: "before".into()
        }
    );
    assert!(browse
        .snapshot_end(&fixture.context, fixture.session, snapshot)
        .await
        .unwrap());
    let live = browse
        .open(
            &fixture.context,
            fixture.session,
            input(relation.clone(), None),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        live.result.page.items[0].cells[1],
        TypedCell::Text {
            value: "after".into()
        }
    );
    assert!(matches!(live.result.consistency, Consistency::LivePerPage));
    assert!(browse
        .page(
            &fixture.context,
            fixture.session,
            first.result.page.next_cursor.clone().unwrap(),
            2
        )
        .await
        .is_err());
    let (_expired, _) = browse
        .snapshot_begin(&fixture.context, fixture.session, "repeatable_read", 20)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    assert_eq!(browse.expire_snapshots().await.unwrap(), 1);
    println!(
        "04 real snapshot sqlite version={version}; held-old/live-new/ttl/default-refusal=true"
    );
    drop(writer);
}

#[tokio::test]
#[ignore = "requires explicit owned WORKBENCH_LIVE_FIXTURES; no credentials printed"]
async fn live_tls_catalog_keyset_temporal_decimal_and_snapshot_for_three_server_engines() {
    let path = std::env::var_os("WORKBENCH_LIVE_FIXTURES").expect("owned fixture config required");
    let bytes = Zeroizing::new(std::fs::read(path).unwrap());
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
            transport_options: BTreeMap::new(),
            expected_server_identity: None,
        };
        let username = data["username"].as_str().unwrap().to_owned();
        let secret = Zeroizing::new(data["password"].as_str().unwrap().to_owned());
        let mut writer = NativeDriverFactory::default()
            .connect(
                &descriptor,
                DriverLogin {
                    username: username.clone(),
                    secret: secret.clone(),
                },
                Mode::Full,
            )
            .await
            .expect("owned TLS writer login");
        let table = format!("wb04_{}", uuid::Uuid::new_v4().simple());
        run(writer.as_mut(),&format!("CREATE TABLE {table}(id BIGINT PRIMARY KEY,label VARCHAR(80),exact_value DECIMAL(30,12))")).await;
        run(writer.as_mut(),&format!("INSERT INTO {table} VALUES(1,'before',123456789012345678.123456789012),(2,'same',0.000000000001),(3,'same',0.000000000002)")).await;
        let fixture = support::Fixture::connect(descriptor, username, secret, Mode::ReadOnly).await;
        let shadow = if engine == Engine::Postgresql {
            let schema = format!("wb04_shadow_{}", uuid::Uuid::new_v4().simple());
            run(writer.as_mut(), &format!("CREATE SCHEMA {schema}")).await;
            run(writer.as_mut(),&format!("CREATE FUNCTION {schema}.eq(pg_catalog.text,pg_catalog.text) RETURNS pg_catalog.bool LANGUAGE sql IMMUTABLE AS 'SELECT true'")).await;
            run(writer.as_mut(),&format!("CREATE OPERATOR {schema}.= (LEFTARG=pg_catalog.text,RIGHTARG=pg_catalog.text,FUNCTION={schema}.eq)")).await;
            let setting = format!("SET search_path TO {schema},pg_catalog");
            fixture
                .runtime
                .with_connection(
                    &fixture.context,
                    fixture.session,
                    AuthorizePurpose::Read,
                    move |connection| {
                        Box::pin(async move {
                            run(connection, &setting).await;
                            Ok(())
                        })
                    },
                )
                .await
                .unwrap();
            Some(schema)
        } else {
            None
        };
        let streams = Arc::new(ResultStreams::new_pull(fixture.runtime.clone()).unwrap());
        let metadata = Arc::new(MetadataCatalog::new(
            fixture.runtime.clone(),
            streams.budget(),
        ));
        let version = fixture
            .runtime
            .with_connection(
                &fixture.context,
                fixture.session,
                AuthorizePurpose::Read,
                |connection| {
                    Box::pin(async move {
                        assert!(
                            connection.facts().verified_tls && connection.facts().read_only_held
                        );
                        Ok(connection.facts().server_version.clone())
                    })
                },
            )
            .await
            .unwrap();
        println!("04 live engine={name} version={version}; catalog begin");
        let schemas = metadata
            .page(
                &fixture.context,
                fixture.session,
                None,
                CatalogKind::Schemas,
                String::new(),
                PageInput {
                    cursor: None,
                    limit: 100,
                },
            )
            .await
            .unwrap_or_else(|e| panic!("{name} schemas {}", e.code()));
        assert!(!schemas.items.is_empty());
        let schema = schemas
            .items
            .iter()
            .find(|item| {
                item.name
                    == if engine == Engine::Postgresql {
                        "public"
                    } else {
                        "workbench"
                    }
            })
            .unwrap()
            .object
            .clone();
        let graph = metadata
            .graph(
                &fixture.context,
                fixture.session,
                schema.clone(),
                PageInput {
                    cursor: None,
                    limit: 100,
                },
            )
            .await
            .unwrap_or_else(|e| panic!("{name} graph {}", e.code()));
        let parent = graph
            .items
            .iter()
            .find(|node| node.name == "wb_parent")
            .unwrap();
        let child = graph
            .items
            .iter()
            .find(|node| node.name == "wb_rows")
            .unwrap();
        assert!(child.edges.iter().any(|edge| edge.to == parent.node_id));
        let foreign = metadata
            .describe(
                &fixture.context,
                fixture.session,
                child.relation.clone(),
                restoreplane_domain::workbench_metadata::DescribePart::ForeignKeys,
                PageInput {
                    cursor: None,
                    limit: 100,
                },
            )
            .await
            .unwrap();
        assert!(foreign.items.iter().any(|detail|matches!(detail,restoreplane_domain::workbench_metadata::CatalogDetail::ForeignKey {referenced_relation,..} if referenced_relation.catalog_id==parent.node_id)));
        let routines = metadata
            .page(
                &fixture.context,
                fixture.session,
                Some(schema),
                CatalogKind::Routines,
                String::new(),
                PageInput {
                    cursor: None,
                    limit: 100,
                },
            )
            .await
            .unwrap();
        assert!(routines.items.len() <= 100);
        let relation = object(&metadata, &fixture, &table).await;
        let described = metadata
            .resolve_relation(&fixture.context, fixture.session, &relation)
            .await
            .unwrap_or_else(|e| panic!("{name} describe {}", e.code()));
        assert!(described.stable_key().is_some());
        if shadow.is_some() {
            let rows = fixture
                .runtime
                .with_connection(
                    &fixture.context,
                    fixture.session,
                    AuthorizePurpose::Read,
                    |connection| {
                        Box::pin(async move {
                            Ok(run(
                                connection,
                                "SELECT 'a'::pg_catalog.text='b'::pg_catalog.text",
                            )
                            .await)
                        })
                    },
                )
                .await
                .unwrap();
            assert_eq!(
                rows,
                vec![vec![TypedCell::Bool { value: true }]],
                "generated metadata must restore the exact user operator lookup path"
            );
        }
        let browse = BrowseService::with_snapshot_certification(
            metadata.clone(),
            streams,
            SnapshotCertification::from_verified_build(
                "section04-owned-verify-full-three-engine-isolation".into(),
                vec![(engine, version.clone())],
            )
            .unwrap(),
        );
        let page = browse
            .open(
                &fixture.context,
                fixture.session,
                input(relation.clone(), None),
                None,
            )
            .await
            .unwrap_or_else(|e| panic!("{name} browse {}", e.code()));
        assert_eq!(
            page.result.page.items[0].cells[2],
            TypedCell::Decimal {
                value: "123456789012345678.123456789012".into(),
                scale: Some(12)
            }
        );
        let next = browse
            .page(
                &fixture.context,
                fixture.session,
                page.result.page.next_cursor.clone().unwrap(),
                2,
            )
            .await
            .unwrap();
        assert_eq!(next.result().page.items.len(), 1);
        let (snapshot, _) = browse
            .snapshot_begin(&fixture.context, fixture.session, "repeatable_read", 60_000)
            .await
            .unwrap_or_else(|e| panic!("{name} snapshot begin {}", e.code()));
        let first = browse
            .open(
                &fixture.context,
                fixture.session,
                input(relation.clone(), Some(snapshot)),
                None,
            )
            .await
            .unwrap();
        run(
            writer.as_mut(),
            &format!("UPDATE {table} SET label='after' WHERE id=1"),
        )
        .await;
        let retained = browse
            .open(
                &fixture.context,
                fixture.session,
                input(relation.clone(), Some(snapshot)),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            retained.result.page.items[0].cells[1],
            TypedCell::Text {
                value: "before".into()
            }
        );
        assert!(matches!(
            first.result.consistency,
            Consistency::TransactionSnapshot { .. }
        ));
        browse
            .snapshot_end(&fixture.context, fixture.session, snapshot)
            .await
            .unwrap();
        let live = browse
            .open(
                &fixture.context,
                fixture.session,
                input(relation, None),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            live.result.page.items[0].cells[1],
            TypedCell::Text {
                value: "after".into()
            }
        );
        let temporal = if engine == Engine::Postgresql {
            "SELECT DATE '2024-02-29',TIME '23:59:59.123456',TIMESTAMP '2024-02-29 23:59:59.123456',TIMETZ '12:34:56.123456+05:30',1.25::double precision"
        } else {
            "SELECT CAST('2024-02-29' AS DATE),CAST('23:59:59.123456' AS TIME(6)),CAST('2024-02-29 23:59:59.123456' AS DATETIME(6)),CAST(18446744073709551615 AS UNSIGNED),CAST(1.25 AS DOUBLE)"
        };
        let values = run(writer.as_mut(), temporal).await;
        assert!(matches!(
            values[0][0],
            TypedCell::Temporal { precision: 0, .. }
        ));
        assert!(
            matches!(&values[0][1],TypedCell::Temporal {value,precision:6,..} if value=="23:59:59.123456")
        );
        assert!(
            matches!(&values[0][2],TypedCell::Temporal {value,precision:6,..} if value.contains("23:59:59.123456"))
        );
        if engine != Engine::Postgresql {
            assert_eq!(
                values[0][3],
                TypedCell::U64 {
                    value: u64::MAX.to_string()
                }
            );
        }
        run(writer.as_mut(), &format!("DROP TABLE {table}")).await;
        if let Some(schema) = shadow {
            fixture
                .runtime
                .with_connection(
                    &fixture.context,
                    fixture.session,
                    AuthorizePurpose::Read,
                    |connection| {
                        Box::pin(async move {
                            run(connection, "SET search_path TO public,pg_catalog").await;
                            Ok(())
                        })
                    },
                )
                .await
                .unwrap();
            run(writer.as_mut(), &format!("DROP SCHEMA {schema} CASCADE")).await;
        }
        writer.close().await.unwrap();
        fixture
            .runtime
            .close(&fixture.context, fixture.session)
            .await
            .unwrap();
        println!("04 live engine={name}; verified-TLS/catalog/keyset/lossless-decimal/temporal/snapshot-held/live-new=true");
    }
}
