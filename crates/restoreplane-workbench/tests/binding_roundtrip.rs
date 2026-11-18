//! Real local v4 metadata -> native file connection boundary; not OS-user IPC E2E.
use async_trait::async_trait;
use restoreplane_catalog::{ConnectionProfileSpec, DatabaseTargetSpec, OperationalStore};
use restoreplane_domain::workbench::{Column, Mode, Route, Scope, TypedCell};
use restoreplane_domain::{ConnectionProfileId, DatabaseTargetId, WorkspaceId};
use restoreplane_workbench::driver::{
    DriverFactory, DriverLogin, NativeDriverFactory, QueryLimits, RowSink,
};
use restoreplane_workbench::WorkbenchError;
use std::str::FromStr;
use zeroize::Zeroizing;

#[derive(Default)]
struct Sink {
    rows: Vec<Vec<TypedCell>>,
}
#[async_trait]
impl RowSink for Sink {
    async fn columns(&mut self, _: Vec<Column>) -> Result<(), WorkbenchError> {
        Ok(())
    }
    async fn row(&mut self, row: Vec<TypedCell>) -> Result<(), WorkbenchError> {
        self.rows.push(row);
        Ok(())
    }
}
#[tokio::test]
async fn v4_registered_ordinary_absolute_sqlite_path_connects_to_the_bound_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("registered.sqlite3");
    let original = rusqlite::Connection::open(&path).unwrap();
    original
        .execute_batch("CREATE TABLE sample(value INTEGER); INSERT INTO sample VALUES(42);")
        .unwrap();
    drop(original);
    let canonical = std::fs::canonicalize(&path).unwrap();
    let native = canonical.to_string_lossy();
    #[cfg(windows)]
    let ordinary = native.strip_prefix(r"\\?\").unwrap_or(&native).to_owned();
    #[cfg(not(windows))]
    let ordinary = native.into_owned();
    assert!(std::path::Path::new(&ordinary).is_absolute());
    #[cfg(windows)]
    assert!(
        !ordinary.starts_with(r"\\?\"),
        "fixture must exercise a normal registered Windows path"
    );
    let probe =
        restoreplane_source_sqlite::probe_sqlite_file(std::path::Path::new(&ordinary)).unwrap();
    let store = OperationalStore::open(directory.path().join("service-v4.sqlite3")).unwrap();
    let scope = Scope {
        workspace_id: WorkspaceId::from_str(restoreplane_catalog::DEFAULT_WORKSPACE_ID).unwrap(),
        connection_profile_id: ConnectionProfileId::new(),
        database_target_id: DatabaseTargetId::new(),
    };
    store
        .configuration()
        .insert_connection(ConnectionProfileSpec::file(
            scope.connection_profile_id.clone(),
            "Ordinary SQLite",
            "sqlite",
            ordinary.clone(),
            100,
        ))
        .unwrap();
    store
        .configuration()
        .insert_target(DatabaseTargetSpec::new(
            scope.database_target_id.clone(),
            scope.connection_profile_id.clone(),
            format!("file:{}", probe.object_id()),
            "main",
            "main",
            "configured-identity-fingerprint",
            110,
        ))
        .unwrap();
    let snapshot = store
        .workbench_binding()
        .snapshot(&scope, &"a".repeat(64))
        .unwrap();
    assert!(
        matches!(&snapshot.descriptor.route,Route::File{canonical_path} if canonical_path==&ordinary)
    );
    let mut connection = NativeDriverFactory::default()
        .connect(
            &snapshot.descriptor,
            DriverLogin {
                username: String::new(),
                secret: Zeroizing::new(String::new()),
            },
            Mode::ReadOnly,
        )
        .await
        .unwrap();
    let mut sink = Sink::default();
    connection
        .execute(
            "SELECT value FROM sample",
            QueryLimits::default(),
            &mut sink,
        )
        .await
        .unwrap();
    assert_eq!(sink.rows, vec![vec![TypedCell::I64 { value: "42".into() }]]);
    assert_eq!(
        connection.facts().context.endpoint,
        probe.canonical_path().to_string_lossy()
    );
    assert_eq!(
        connection.facts().server_identity,
        Some(format!("sqlite-file-id:{}", probe.object_id()))
    );
    connection.close().await.unwrap();
    drop(connection);
    assert_eq!(store.writer_diagnostics().unwrap().user_version, 4);
    let other = directory.path().join("other.sqlite3");
    let other_db = rusqlite::Connection::open(&other).unwrap();
    other_db
        .execute_batch("CREATE TABLE different(value INTEGER);")
        .unwrap();
    drop(other_db);
    let mut changed = snapshot.descriptor;
    changed.route = Route::File {
        canonical_path: other.to_string_lossy().into_owned(),
    };
    assert!(matches!(
        NativeDriverFactory::default()
            .connect(
                &changed,
                DriverLogin {
                    username: String::new(),
                    secret: Zeroizing::new(String::new())
                },
                Mode::ReadOnly
            )
            .await,
        Err(WorkbenchError::BindingStale)
    ));
}
