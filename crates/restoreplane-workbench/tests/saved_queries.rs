mod support;
use restoreplane_domain::workbench_metadata::PageInput;
use restoreplane_domain::workbench_query::SavedQueryPut;
use restoreplane_workbench::saved_queries::SavedQueries;
use restoreplane_workbench::WorkbenchError;
use std::sync::Arc;

#[tokio::test]
async fn explicit_named_query_is_current_user_encrypted_before_bind_and_version_guarded() {
    let fixture = support::QueryFixture::new(
        support::Fixture::sqlite("CREATE TABLE source(id INTEGER)").await,
    );
    let saved = Arc::new(SavedQueries::new(
        fixture.database.runtime.clone(),
        fixture.store.clone(),
    ));
    let sql = "SELECT 'SQL_CANARY_saved_7b1d; 한국어'";
    let name = "NAME_CANARY_saved_2f48";
    let created = saved
        .put(
            &fixture.database.context,
            SavedQueryPut {
                scope: fixture.database.scope.clone(),
                query_id: None,
                name: name.into(),
                sql: sql.into(),
                expected_version: None,
            },
        )
        .await
        .unwrap();
    let loaded = saved
        .get(&fixture.database.context, created.query_id)
        .await
        .unwrap();
    assert_eq!(loaded.sql, sql);
    assert_eq!(loaded.name, name);
    let page = saved
        .list(
            &fixture.database.context,
            fixture.database.scope.clone(),
            PageInput {
                cursor: None,
                limit: 1,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].name, name);
    for filename in ["client-state.sqlite3", "client-state.sqlite3-wal"] {
        let path = fixture.journal_root.path().join(filename);
        if path.exists() {
            let bytes = std::fs::read(path).unwrap();
            for canary in [sql.as_bytes(), name.as_bytes()] {
                assert!(!bytes.windows(canary.len()).any(|part| part == canary));
            }
        }
    }
    let updated = saved
        .put(
            &fixture.database.context,
            SavedQueryPut {
                scope: fixture.database.scope.clone(),
                query_id: Some(created.query_id),
                name: "renamed".into(),
                sql: "SELECT 2".into(),
                expected_version: Some(created.version),
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.version.value(), 2);
    assert!(matches!(
        saved
            .delete(&fixture.database.context, created.query_id, 1)
            .await,
        Err(WorkbenchError::IdempotencyConflict)
    ));
    assert!(saved
        .delete(&fixture.database.context, created.query_id, 2)
        .await
        .unwrap());
    let count: i64 = fixture
        .store
        .read(|connection| {
            connection
                .query_row("SELECT COUNT(*) FROM operations", [], |row| row.get(0))
                .map_err(|_| WorkbenchError::StoreUnavailable)
        })
        .unwrap();
    assert_eq!(
        count, 0,
        "saving never dispatches target SQL or creates automatic history"
    );
}
#[tokio::test]
async fn saved_query_owner_scope_cursor_and_content_bounds_are_enforced() {
    let fixture = support::QueryFixture::new(
        support::Fixture::sqlite("CREATE TABLE source(id INTEGER)").await,
    );
    let saved = Arc::new(SavedQueries::new(
        fixture.database.runtime.clone(),
        fixture.store.clone(),
    ));
    for index in 0..3 {
        saved
            .put(
                &fixture.database.context,
                SavedQueryPut {
                    scope: fixture.database.scope.clone(),
                    query_id: None,
                    name: format!("query{index}"),
                    sql: format!("SELECT {index}"),
                    expected_version: None,
                },
            )
            .await
            .unwrap();
    }
    let first = saved
        .list(
            &fixture.database.context,
            fixture.database.scope.clone(),
            PageInput {
                cursor: None,
                limit: 1,
            },
        )
        .await
        .unwrap();
    let foreign = restoreplane_workbench::InvocationContext::new(
        fixture.database.context.principal().clone(),
        uuid::Uuid::new_v4(),
    );
    assert!(matches!(
        saved
            .list(
                &foreign,
                fixture.database.scope.clone(),
                PageInput {
                    cursor: first.next_cursor,
                    limit: 1
                }
            )
            .await,
        Err(WorkbenchError::CursorInvalid)
    ));
    let owner = restoreplane_workbench::NativePrincipal::new(
        "b".repeat(64),
        7,
        restoreplane_workbench::IntegrityLevel::Standard,
    )
    .unwrap();
    let foreign = restoreplane_workbench::InvocationContext::new(owner, uuid::Uuid::new_v4());
    assert!(matches!(
        saved.get(&foreign, first.items[0].query_id).await,
        Err(WorkbenchError::Forbidden)
    ));
    assert!(matches!(
        saved
            .put(
                &fixture.database.context,
                SavedQueryPut {
                    scope: fixture.database.scope.clone(),
                    query_id: None,
                    name: "oversize".into(),
                    sql: "x".repeat(201 * 1024),
                    expected_version: None
                }
            )
            .await,
        Err(WorkbenchError::InvalidInput)
    ));
}

const SQL_PRIVACY_CANARY: &str = "SQL_PRIVATE_saved_default_diagnostics_164c";
const NAME_PRIVACY_CANARY: &str = "NAME_PRIVATE_saved_default_diagnostics_54ab";
fn scan_private_files(root: &std::path::Path) -> usize {
    let mut files = 0;
    for entry in std::fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if entry.file_type().unwrap().is_dir() {
            files += scan_private_files(&path);
            continue;
        }
        let metadata = entry.metadata().unwrap();
        // The store's empty exclusive-lock file cannot contain saved content.
        if path
            .file_name()
            .is_some_and(|name| name == "client-state.lock")
        {
            assert_eq!(metadata.len(), 0);
            continue;
        }
        // The separately byte-range-locked anchor is closed metadata only
        // (owner/epoch/revision/phase), not a SQLite/temp content location.
        if path
            .file_name()
            .is_some_and(|name| name == "client-state.anchor")
        {
            assert!(metadata.len() < 1024);
            continue;
        }
        assert!(
            metadata.len() < 16 * 1024 * 1024,
            "privacy fixture file bound"
        );
        let bytes = std::fs::read(&path).unwrap();
        for marker in [SQL_PRIVACY_CANARY, NAME_PRIVACY_CANARY] {
            assert!(
                !bytes
                    .windows(marker.len())
                    .any(|part| part == marker.as_bytes()),
                "plaintext canary reached a persisted main/WAL/temp file"
            );
        }
        files += 1;
    }
    files
}

#[test]
fn saved_query_main_wal_temp_and_default_child_diagnostics_never_contain_plaintext() {
    let root = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "saved_privacy_default_diagnostics_child",
            "--ignored",
            "--nocapture",
        ])
        .env("SAVED_PRIVACY_SCRATCH", root.path())
        .env("TEMP", root.path())
        .env("TMP", root.path())
        .env_remove("RUST_LOG")
        .output()
        .unwrap();
    for bytes in [&output.stdout, &output.stderr] {
        for marker in [SQL_PRIVACY_CANARY, NAME_PRIVACY_CANARY] {
            assert!(
                !bytes
                    .windows(marker.len())
                    .any(|part| part == marker.as_bytes()),
                "default stdout/stderr must not contain saved SQL or label"
            );
        }
    }
    assert!(
        output.status.success(),
        "isolated saved-query privacy scenario failed; no captured content is printed"
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("saved_privacy_files_checked="));
}

#[test]
#[ignore = "isolated child invoked by the default-diagnostics privacy scenario"]
fn saved_privacy_default_diagnostics_child() {
    let root = std::path::PathBuf::from(
        std::env::var_os("SAVED_PRIVACY_SCRATCH").expect("private child fixture root"),
    );
    assert!(
        std::env::temp_dir().starts_with(&root),
        "child TEMP/TMP isolation must be effective"
    );
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async{
        let fixture=support::QueryFixture::new(support::Fixture::sqlite("CREATE TABLE source(id INTEGER)").await);
        let saved=Arc::new(SavedQueries::new(fixture.database.runtime.clone(),fixture.store.clone()));
        let sql=format!("--{SQL_PRIVACY_CANARY}\nSELECT '{}'", "x".repeat(120*1024));
        let created=saved.put(&fixture.database.context,SavedQueryPut{scope:fixture.database.scope.clone(),query_id:None,name:NAME_PRIVACY_CANARY.into(),sql:sql.clone(),expected_version:None}).await.unwrap();
        let loaded=saved.get(&fixture.database.context,created.query_id).await.unwrap();
        assert!(loaded.sql==sql && loaded.name==NAME_PRIVACY_CANARY);
        let error=saved.delete(&fixture.database.context,created.query_id,created.version.value()+1).await.err().unwrap();
        assert_eq!(error,WorkbenchError::IdempotencyConflict);
        let diagnostic=format!("{error:?}");
        assert!(!diagnostic.contains(SQL_PRIVACY_CANARY) && !diagnostic.contains(NAME_PRIVACY_CANARY));
        fixture.store.write(|connection|{
            // Temp-table and sort inputs contain ciphertext only. No plaintext
            // is rebound to SQLite to manufacture this temp-file stress.
            connection.execute_batch("PRAGMA temp_store=FILE;CREATE TEMP TABLE privacy_cipher AS WITH RECURSIVE n(x) AS(VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<64) SELECT encrypted_content AS cipher FROM saved_queries CROSS JOIN n;").map_err(|_|WorkbenchError::StoreUnavailable)?;
            let count:i64=connection.query_row("SELECT COUNT(*) FROM(SELECT cipher FROM privacy_cipher ORDER BY cipher)",[],|row|row.get(0)).map_err(|_|WorkbenchError::StoreUnavailable)?;
            assert_eq!(count,64);
            let files=scan_private_files(&root);
            assert!(files>=3,"source/main/WAL files must actually be inspected");
            println!("saved_privacy_files_checked={files};temp_store=file;diagnostic=redacted");
            Ok(())
        }).unwrap();
        // The production library and default Console have no subscriber/log
        // plugin. This child captures default diagnostics; a host installing
        // its own SQL-logging subscriber is a separate unsupported guarantee.
    });
}
