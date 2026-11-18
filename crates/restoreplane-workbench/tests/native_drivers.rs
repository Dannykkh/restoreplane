use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use restoreplane_domain::workbench::{
    Column, ConnectDescriptor, DatabaseIdentity, Engine, Mode, RootTrust, Route, Tls, TypedCell,
};
use restoreplane_workbench::driver::{
    DriverFactory, DriverLogin, NativeDriverFactory, QueryLimits, RowSink,
};
use restoreplane_workbench::WorkbenchError;
use zeroize::Zeroizing;

#[derive(Default)]
struct Sink {
    columns: Vec<Column>,
    rows: Vec<Vec<TypedCell>>,
    started: Option<std::sync::Arc<tokio::sync::Notify>>,
}
#[async_trait]
impl RowSink for Sink {
    async fn columns(&mut self, columns: Vec<Column>) -> Result<(), WorkbenchError> {
        self.columns = columns;
        Ok(())
    }
    async fn row(&mut self, cells: Vec<TypedCell>) -> Result<(), WorkbenchError> {
        self.rows.push(cells);
        if let Some(started) = &self.started {
            started.notify_one();
        }
        Ok(())
    }
}

fn login() -> DriverLogin {
    DriverLogin {
        username: String::new(),
        secret: Zeroizing::new(String::new()),
    }
}

#[tokio::test]
#[ignore = "explicit owned-fixture transport diagnostics; never prints credentials or driver text"]
async fn live_mysql_transport_diagnostic() {
    let path = std::env::var_os("WORKBENCH_LIVE_FIXTURES").expect("owned fixture required");
    let bytes = Zeroizing::new(std::fs::read(path).unwrap());
    let config: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    for engine in ["mysql", "mariadb"] {
        let row = &config[engine];
        let ca = std::fs::read(row["ca_file"].as_str().unwrap()).unwrap();
        let ssl = mysql_async::SslOpts::default()
            .with_disable_built_in_roots(true)
            .with_root_certs(vec![ca.into()]);
        let options = mysql_async::OptsBuilder::default()
            .ip_or_hostname(row["host"].as_str().unwrap())
            .tcp_port(row["port"].as_u64().unwrap() as u16)
            .prefer_socket(false)
            .user(Some(row["username"].as_str().unwrap()))
            .pass(Some(row["password"].as_str().unwrap()))
            .db_name(Some(row["database"].as_str().unwrap()))
            .ssl_opts(ssl)
            .max_allowed_packet(Some(1024 * 1024));
        match mysql_async::Conn::new(options).await {
            Ok(connection) => {
                println!("{engine}: transport connected");
                connection.disconnect().await.unwrap();
            }
            Err(error) => {
                let classification = match error {
                    mysql_async::Error::Server(error) => format!("server_code={}", error.code),
                    mysql_async::Error::Io(mysql_async::IoError::Io(error)) => {
                        format!("io_kind={:?}", error.kind())
                    }
                    mysql_async::Error::Io(mysql_async::IoError::Tls(error)) => {
                        let text = error.to_string();
                        let code = text
                            .split(|ch: char| !ch.is_ascii_alphanumeric())
                            .find(|part| {
                                part.starts_with("0x")
                                    && part[2..].chars().all(|ch| ch.is_ascii_hexdigit())
                            })
                            .unwrap_or("unavailable");
                        format!("tls_error_code={code}")
                    }
                    mysql_async::Error::Driver(error) => {
                        format!("driver_variant={:?}", std::mem::discriminant(&error))
                    }
                    _ => "other_redacted".into(),
                };
                panic!("{engine}: {classification}");
            }
        }
    }
}

fn sqlite_descriptor(path: &std::path::Path) -> ConnectDescriptor {
    ConnectDescriptor {
        engine: Engine::Sqlite,
        route: Route::File {
            canonical_path: std::fs::canonicalize(path)
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        },
        database: DatabaseIdentity {
            engine_object_id: format!(
                "file:{}",
                restoreplane_source_sqlite::probe_sqlite_file(path)
                    .unwrap()
                    .object_id()
            ),
            name: "fixture".into(),
            source_fingerprint: "fixture".into(),
        },
        tls: Tls {
            mode: "not_applicable".into(),
            server_name: None,
            root_trust: None,
        },
        transport_options: BTreeMap::new(),
        expected_server_identity: None,
    }
}

#[derive(Default)]
struct CountingRows {
    rows: usize,
    columns: usize,
}
#[async_trait]
impl RowSink for CountingRows {
    async fn columns(&mut self, columns: Vec<Column>) -> Result<(), WorkbenchError> {
        self.columns = columns.len();
        Ok(())
    }
    async fn row(&mut self, _: Vec<TypedCell>) -> Result<(), WorkbenchError> {
        self.rows += 1;
        Ok(())
    }
}

#[tokio::test]
async fn sqlite_wide_query_bytecap_drains_all_rows_and_preserves_readwrite_returning_effects() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("wide-cap.sqlite3");
    let definitions = (0..100)
        .map(|index| {
            if index == 7 {
                "c7 INTEGER PRIMARY KEY".into()
            } else if index % 8 == 5 {
                format!("c{index} BLOB")
            } else if index % 8 == 7 {
                format!("c{index} INTEGER")
            } else {
                format!("c{index} TEXT")
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    let values = (0..100)
        .map(|index| match index % 8 {
            0 => "NULL".into(),
            1 => "''".into(),
            2 => "'18446744073709551615'".into(),
            3 => "n||'.12345678901234567890'".into(),
            4 => format!("'한국어 값'||n||' 열{index} 시드 71237'"),
            5 => "x'00010203fffe'".into(),
            6 => "'2026-11-17T00:00:00.123456+09:00'".into(),
            _ => format!("-(n*100+{index})"),
        })
        .collect::<Vec<_>>()
        .join(",");
    let source = rusqlite::Connection::open(&path).unwrap();
    source.execute_batch(&format!("CREATE TABLE bench({definitions});WITH RECURSIVE r(n) AS(VALUES(0) UNION ALL SELECT n+1 FROM r WHERE n<99999) INSERT INTO bench SELECT {values} FROM r;")).unwrap();
    drop(source);
    let mut connection = NativeDriverFactory::default()
        .connect(&sqlite_descriptor(&path), login(), Mode::ReadOnly)
        .await
        .unwrap();
    let mut rows = CountingRows::default();
    let started = std::time::Instant::now();
    let summary = connection
        .execute(
            "SELECT * FROM bench ORDER BY c7 DESC",
            QueryLimits {
                max_rows: 100_000,
                max_bytes: 64 * 1024 * 1024,
                timeout: Duration::from_secs(30),
            },
            &mut rows,
        )
        .await
        .unwrap();
    assert_eq!(rows.columns, 100);
    assert_eq!(summary.rows_seen, 100_000);
    assert!(summary.capped);
    assert_eq!(summary.rows_kept as usize, rows.rows);
    assert!(summary.encoded_bytes <= 64 * 1024 * 1024);
    assert_eq!(
        summary.transaction_state,
        restoreplane_workbench::driver::DriverTransactionState::Idle
    );
    println!(
        "sqlite_wide_cap elapsed_ms={} rows_seen={} rows_kept={} encoded_bytes={} capped=true",
        started.elapsed().as_millis(),
        summary.rows_seen,
        summary.rows_kept,
        summary.encoded_bytes
    );
    let mut check = Sink::default();
    connection
        .execute(
            "SELECT COUNT(*) FROM bench",
            QueryLimits::default(),
            &mut check,
        )
        .await
        .unwrap();
    assert_eq!(
        check.rows[0][0],
        TypedCell::I64 {
            value: "100000".into()
        }
    );
    connection.close().await.unwrap();
    let mut writer = NativeDriverFactory::default()
        .connect(&sqlite_descriptor(&path), login(), Mode::ReadWrite)
        .await
        .unwrap();
    let mut returned = CountingRows::default();
    let changed = writer
        .execute(
            "UPDATE bench SET c1='after-cap' RETURNING c7,c1",
            QueryLimits {
                max_rows: 100_000,
                max_bytes: 128,
                timeout: Duration::from_secs(30),
            },
            &mut returned,
        )
        .await
        .unwrap();
    assert!(changed.capped);
    assert_eq!(changed.rows_seen, 100_000);
    assert_eq!(changed.rows_affected, Some(100_000));
    let observer = rusqlite::Connection::open(&path).unwrap();
    let count: i64 = observer
        .query_row(
            "SELECT COUNT(*) FROM bench WHERE c1='after-cap'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 100_000,
        "RETURNING cap must never abbreviate actual mutation execution"
    );
}

#[tokio::test]
async fn sqlite_actual_vm_deadline_is_interrupted_without_claiming_cleanup_or_rollback() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("deadline.sqlite3");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE TABLE source(id INTEGER)")
        .unwrap();
    let mut connection = NativeDriverFactory::default()
        .connect(&sqlite_descriptor(&path), login(), Mode::ReadOnly)
        .await
        .unwrap();
    let started = std::time::Instant::now();
    let error=connection.execute("WITH RECURSIVE n(x) AS(VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000000000) SELECT SUM(x) FROM n",QueryLimits{timeout:Duration::from_millis(1),..QueryLimits::default()},&mut CountingRows::default()).await.err().unwrap();
    assert_eq!(error, WorkbenchError::CancelUnsettled);
    println!(
        "sqlite_native_vm_deadline elapsed_ms={} interrupted=true rollback_proof=false",
        started.elapsed().as_millis()
    );
}

#[tokio::test]
async fn sqlite_readonly_exact_values_caps_and_unsafe_pragmas() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("source.sqlite3");
    let source = rusqlite::Connection::open(&path).unwrap();
    source.execute_batch("CREATE TABLE example(id INTEGER PRIMARY KEY, label TEXT, payload BLOB); INSERT INTO example VALUES(9223372036854775807,NULL,X'00FF'); INSERT INTO example VALUES(1,'',X''); INSERT INTO example VALUES(2,'한국어',X'01')").unwrap();
    drop(source);
    let mut connection = NativeDriverFactory::default()
        .connect(&sqlite_descriptor(&path), login(), Mode::ReadOnly)
        .await
        .unwrap();
    assert!(connection.facts().read_only_held);
    let mut sink = Sink::default();
    let summary = connection
        .execute(
            "SELECT id,label,payload FROM example ORDER BY id DESC",
            QueryLimits::default(),
            &mut sink,
        )
        .await
        .unwrap();
    assert_eq!(summary.rows_seen, 3);
    assert_eq!(
        sink.rows[0][0],
        TypedCell::I64 {
            value: i64::MAX.to_string()
        }
    );
    assert_eq!(sink.rows[0][1], TypedCell::Null);
    assert_eq!(
        sink.rows[0][2],
        TypedCell::Bytes {
            base64: "AP8=".into()
        }
    );
    assert!(sink.rows.iter().any(|row| row[1]
        == TypedCell::Text {
            value: String::new()
        }));
    let mut capped = Sink::default();
    let summary = connection
        .execute(
            "SELECT id FROM example",
            QueryLimits {
                max_rows: 1,
                ..QueryLimits::default()
            },
            &mut capped,
        )
        .await
        .unwrap();
    assert!(summary.capped);
    assert_eq!((summary.rows_seen, summary.rows_kept), (3, 1));
    for unsafe_sql in [
        "PRAGMA query_only=OFF",
        "ATTACH DATABASE ':memory:' AS escape",
        "DELETE FROM example",
    ] {
        assert!(connection
            .execute(unsafe_sql, QueryLimits::default(), &mut Sink::default())
            .await
            .is_err());
    }
    assert!(connection.set_read_only(false).await.is_err());
    let values = vec![
        TypedCell::I64 {
            value: i64::MAX.to_string(),
        },
        TypedCell::Text {
            value: "한국어 ' ; DROP TABLE example; --".into(),
        },
        TypedCell::Bytes {
            base64: "AP8=".into(),
        },
        TypedCell::Null,
    ];
    let mut bound = Sink::default();
    connection
        .execute_bound(
            "SELECT ?1,?2,?3,?4",
            &values,
            QueryLimits::default(),
            &mut bound,
        )
        .await
        .unwrap();
    assert_eq!(bound.rows, vec![values]);
    connection.close().await.unwrap();
}

#[tokio::test]
async fn sqlite_refuses_oversized_value_before_sending_a_row() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("source.sqlite3");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("PRAGMA user_version=1")
        .unwrap();
    let mut connection = NativeDriverFactory::default()
        .connect(&sqlite_descriptor(&path), login(), Mode::ReadOnly)
        .await
        .unwrap();
    let mut sink = Sink::default();
    assert!(connection
        .execute(
            "SELECT zeroblob(67108865)",
            QueryLimits::default(),
            &mut sink
        )
        .await
        .is_err());
    assert!(sink.rows.is_empty());
}

#[tokio::test]
async fn sqlite_ordinary_absolute_path_checks_file_identity_and_holds_it() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("ordinary.sqlite3");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE TABLE sample(id INTEGER PRIMARY KEY)")
        .unwrap();
    let mut descriptor = sqlite_descriptor(&path);
    descriptor.route = Route::File {
        canonical_path: path.to_string_lossy().into_owned(),
    };
    let factory = NativeDriverFactory::default();
    let mut connection = factory
        .connect(&descriptor, login(), Mode::ReadOnly)
        .await
        .unwrap();
    assert!(connection
        .facts()
        .server_identity
        .as_deref()
        .is_some_and(|value| value.starts_with("sqlite-file-id:")));
    let other_path = directory.path().join("other.sqlite3");
    rusqlite::Connection::open(&other_path)
        .unwrap()
        .execute_batch("CREATE TABLE different(id INTEGER)")
        .unwrap();
    let mut stale = descriptor.clone();
    stale.database.engine_object_id = sqlite_descriptor(&other_path).database.engine_object_id;
    assert!(matches!(
        factory.connect(&stale, login(), Mode::ReadOnly).await,
        Err(WorkbenchError::BindingStale)
    ));
    #[cfg(windows)]
    assert!(
        std::fs::rename(&path, directory.path().join("replaced.sqlite3")).is_err(),
        "held file identity must prevent replace/delete"
    );
    connection.close().await.unwrap();
    drop(connection);
    std::fs::rename(&path, directory.path().join("released.sqlite3")).unwrap();
}

#[tokio::test]
async fn sqlite_interrupt_stops_a_running_vm_without_claiming_rollback() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("cancel.sqlite3");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("PRAGMA user_version=1")
        .unwrap();
    let mut connection = NativeDriverFactory::default()
        .connect(&sqlite_descriptor(&path), login(), Mode::ReadOnly)
        .await
        .unwrap();
    let cancel = connection.cancel_handle();
    let started = std::sync::Arc::new(tokio::sync::Notify::new());
    let sink_started = started.clone();
    let work = tokio::spawn(async move {
        connection.execute("WITH RECURSIVE counter(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM counter WHERE n<1000000000) SELECT n FROM counter", QueryLimits::default(), &mut Sink { started: Some(sink_started), ..Sink::default() }).await
    });
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    cancel.request().await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), work)
            .await
            .unwrap()
            .unwrap()
            .err()
            .unwrap(),
        WorkbenchError::CancelUnsettled
    );
}

#[tokio::test]
async fn production_factory_never_silently_weakens_tls() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("unused.sqlite3");
    rusqlite::Connection::open(&file)
        .unwrap()
        .execute_batch("PRAGMA user_version=1")
        .unwrap();
    let mut descriptor = sqlite_descriptor(&file);
    descriptor.engine = Engine::Postgresql;
    descriptor.route = Route::Tcp {
        host: "127.0.0.1".into(),
        port: 1,
    };
    for mode in ["disable", "prefer", "require", "verify-ca", "unknown"] {
        descriptor.tls.mode = mode.into();
        assert!(matches!(
            NativeDriverFactory::default()
                .connect(&descriptor, login(), Mode::ReadOnly)
                .await,
            Err(WorkbenchError::Unsupported)
        ));
    }
}

/// Explicit synthetic loopback fixtures only; credentials never appear in output.
#[tokio::test]
#[ignore = "requires WORKBENCH_LIVE_FIXTURES pointing at an owned private fixture config"]
async fn live_tls_pg_mysql_mariadb_exact_values_and_oversized_frames() {
    let path = std::env::var_os("WORKBENCH_LIVE_FIXTURES")
        .expect("explicit owned fixture config required");
    let contents = Zeroizing::new(std::fs::read(path).expect("private fixture config unavailable"));
    let fixtures: serde_json::Value =
        serde_json::from_slice(&contents).expect("fixture config format");
    for (name, engine) in [
        ("postgresql", Engine::Postgresql),
        ("mysql", Engine::Mysql),
        ("mariadb", Engine::Mariadb),
    ] {
        println!("owned synthetic TLS adapter: {name}");
        let fixture = &fixtures[name];
        let ca_path = fixture["ca_file"]
            .as_str()
            .expect("explicit synthetic CA required");
        let ca_bytes = std::fs::read(ca_path).expect("synthetic CA unavailable");
        let mut descriptor = ConnectDescriptor {
            engine,
            route: Route::Tcp {
                host: fixture["host"].as_str().unwrap().into(),
                port: fixture["port"].as_u64().unwrap() as u16,
            },
            database: DatabaseIdentity {
                engine_object_id: "fixture".into(),
                name: fixture["database"].as_str().unwrap().into(),
                source_fingerprint: "fixture".into(),
            },
            tls: Tls {
                mode: "verify-full".into(),
                server_name: None,
                root_trust: Some(RootTrust::Custom {
                    canonical_path: ca_path.into(),
                    content_digest: blake3::hash(&ca_bytes).to_hex().to_string(),
                }),
            },
            transport_options: BTreeMap::new(),
            expected_server_identity: None,
        };
        let make_login = || DriverLogin {
            username: fixture["username"].as_str().unwrap().into(),
            secret: Zeroizing::new(fixture["password"].as_str().unwrap().into()),
        };
        let factory = NativeDriverFactory::default();
        let mut connection = tokio::time::timeout(
            Duration::from_secs(20),
            factory.connect(&descriptor, make_login(), Mode::ReadOnly),
        )
        .await
        .expect("fixture TLS login timed out")
        .unwrap_or_else(|error| {
            panic!(
                "fixture verified TLS login failed for {name}; redacted code={}",
                error.code()
            )
        });
        assert!(connection.facts().verified_tls);
        assert!(connection.facts().read_only_held);
        assert_eq!(connection.facts().engine, engine);
        let mut sink = Sink::default();
        let summary = connection.execute("SELECT id,nullable_text,exact_decimal,payload,unicode_text FROM wb_rows ORDER BY id", QueryLimits::default(), &mut sink).await.expect("fixture read failed; details redacted");
        assert_eq!(summary.rows_seen, 3, "{name}");
        assert!(
            sink.rows.iter().any(|row| row[1] == TypedCell::Null),
            "{name}"
        );
        assert!(
            sink.rows.iter().any(|row| row[1]
                == TypedCell::Text {
                    value: String::new()
                }),
            "{name}"
        );
        assert!(
            sink.rows.iter().any(
                |row| matches!(&row[2], TypedCell::Decimal { value, .. } if value.contains('.'))
            ),
            "{name}"
        );
        assert!(
            sink.rows
                .iter()
                .any(|row| matches!(&row[3], TypedCell::Bytes { .. })),
            "{name}: column types {:?}; payload kinds {:?}",
            sink.columns
                .iter()
                .map(|column| &column.type_name)
                .collect::<Vec<_>>(),
            sink.rows
                .iter()
                .map(|row| std::mem::discriminant(&row[3]))
                .collect::<Vec<_>>()
        );
        assert!(sink.rows.iter().any(|row| matches!(&row[4], TypedCell::Text { value } if value.chars().any(|ch| ('가'..='힣').contains(&ch)))), "{name}");
        let parameters = vec![
            TypedCell::I64 {
                value: i64::MAX.to_string(),
            },
            TypedCell::Text {
                value: "한국어 ' ; DROP TABLE wb_rows; --".into(),
            },
            TypedCell::Bytes {
                base64: "AP8=".into(),
            },
            TypedCell::Decimal {
                value: "123456789012345678.123456789012".into(),
                scale: Some(12),
            },
            TypedCell::Null,
        ];
        let bound_sql = if engine == Engine::Postgresql {
            "SELECT $1::bigint,$2::text,$3::bytea,$4::numeric,$5::text"
        } else {
            "SELECT CAST(? AS SIGNED),CAST(? AS CHAR CHARACTER SET utf8mb4),CAST(? AS BINARY),CAST(? AS DECIMAL(30,12)),?"
        };
        let mut bound = Sink::default();
        connection
            .execute_bound(bound_sql, &parameters, QueryLimits::default(), &mut bound)
            .await
            .expect("bound primitive roundtrip failed; details redacted");
        assert_eq!(
            bound.rows.first(),
            Some(&parameters),
            "bound values must stay exact: {name}"
        );
        let mut prefix = Sink::default();
        let prefix_summary = connection.execute("SELECT 1 AS seq,'small' AS payload UNION ALL SELECT 2,REPEAT('x',4096) UNION ALL SELECT 3,'tail' ORDER BY seq",QueryLimits{max_rows:100,max_bytes:512,timeout:Duration::from_secs(30)},&mut prefix).await.expect("capped prefix must still reach protocol completion");
        assert!(prefix_summary.capped, "{name}");
        assert_eq!(
            (prefix_summary.rows_seen, prefix_summary.rows_kept),
            (3, 1),
            "{name}: later small row must not cross a missing key"
        );
        assert_eq!(prefix.rows.len(), 1, "{name}");
        assert_eq!(
            prefix.rows[0][0],
            TypedCell::I64 { value: "1".into() },
            "{name}"
        );
        assert_eq!(
            prefix_summary.transaction_state,
            restoreplane_workbench::driver::DriverTransactionState::Idle,
            "{name}: Ready/OK must still be observed"
        );
        connection
            .execute("SELECT 1", QueryLimits::default(), &mut Sink::default())
            .await
            .expect("a capped read must leave a fully drained reusable connection");
        let mut empty = Sink::default();
        let empty_summary = connection
            .execute(
                "SELECT id FROM wb_rows WHERE 1=0",
                QueryLimits::default(),
                &mut empty,
            )
            .await
            .expect("zero-row metadata failed; details redacted");
        assert_eq!(empty.columns.len(), 1, "empty result metadata: {name}");
        assert_eq!(empty_summary.rows_seen, 0, "{name}");
        assert!(empty.rows.is_empty(), "{name}");
        assert!(
            connection
                .execute(
                    "DELETE FROM wb_rows WHERE id=-9223372036854775808",
                    QueryLimits::default(),
                    &mut Sink::default()
                )
                .await
                .is_err(),
            "readonly backstop: {name}"
        );
        connection.close().await.expect("fixture close failed");
        let mut cancellable = factory
            .connect(&descriptor, make_login(), Mode::ReadOnly)
            .await
            .expect("fixture cancellation login failed");
        let cancel = cancellable.cancel_handle();
        let work = tokio::spawn(async move {
            let result = cancellable
                .execute(
                    if engine == Engine::Postgresql {
                        "SELECT pg_sleep(3)"
                    } else {
                        "SELECT 1 FROM wb_rows WHERE SLEEP(3)"
                    },
                    QueryLimits::default(),
                    &mut Sink::default(),
                )
                .await;
            let _ = cancellable.close().await;
            result
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        tokio::time::timeout(Duration::from_secs(2), cancel.request())
            .await
            .expect("cancel request timed out")
            .expect("cancel request failed; details redacted");
        assert!(
            tokio::time::timeout(Duration::from_secs(2), work)
                .await
                .expect("cancel did not settle read")
                .unwrap()
                .is_err(),
            "cancelled read must fail: {name}"
        );
        let mut giant = factory
            .connect(&descriptor, make_login(), Mode::ReadOnly)
            .await
            .expect("fixture reopen failed");
        let mut giant_sink = Sink::default();
        let giant_query = if engine == Engine::Postgresql {
            "SELECT repeat('x',67108865)"
        } else {
            "SELECT repeat('x',2097152)"
        };
        assert!(
            tokio::time::timeout(
                Duration::from_secs(15),
                giant.execute(giant_query, QueryLimits::default(), &mut giant_sink)
            )
            .await
            .expect("giant frame test timed out")
            .is_err(),
            "oversized frame should refuse: {name}"
        );
        assert!(giant_sink.rows.is_empty(), "{name}");
        let _ = giant.close().await;
        let mut slow = factory
            .connect(&descriptor, make_login(), Mode::ReadOnly)
            .await
            .expect("fixture timeout login failed");
        let started = std::time::Instant::now();
        let timeout_error = slow
            .execute(
                if engine == Engine::Postgresql {
                    "SELECT pg_sleep(3)"
                } else {
                    "SELECT SLEEP(3)"
                },
                QueryLimits {
                    timeout: Duration::from_millis(100),
                    ..QueryLimits::default()
                },
                &mut Sink::default(),
            )
            .await
            .expect_err("client timeout must retire the transport");
        assert!(
            matches!(timeout_error, WorkbenchError::CancelUnsettled),
            "{name}"
        );
        assert!(started.elapsed() < Duration::from_secs(2), "{name}");
        assert!(
            slow.execute("SELECT 1", QueryLimits::default(), &mut Sink::default())
                .await
                .is_err(),
            "timed out transport cannot be reused: {name}"
        );
        let _ = slow.close().await;
        descriptor.tls.root_trust = Some(RootTrust::System);
        assert!(
            factory
                .connect(&descriptor, make_login(), Mode::ReadOnly)
                .await
                .is_err(),
            "untrusted CA must refuse: {name}"
        );
        descriptor.tls.root_trust = Some(RootTrust::Custom {
            canonical_path: ca_path.into(),
            content_digest: blake3::hash(&ca_bytes).to_hex().to_string(),
        });
        descriptor.tls.server_name = Some("wrong-fixture-host.invalid".into());
        assert!(
            factory
                .connect(&descriptor, make_login(), Mode::ReadOnly)
                .await
                .is_err(),
            "hostname mismatch must refuse: {name}"
        );
    }
}
