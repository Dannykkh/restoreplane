//! Owned verify-full fixtures only. Principal/authority fixtures are native
//! seams, not a real OS-medium IPC or WebView proof.
mod support;
use async_trait::async_trait;
use restoreplane_domain::workbench::*;
use restoreplane_domain::workbench_metadata::{CatalogKind, PageInput};
use restoreplane_domain::workbench_movement::*;
use restoreplane_workbench::data_movement::{
    FileCapabilities, ImportCoordinator, ImportPreviewInput, NativeSourceSelection,
};
use restoreplane_workbench::driver::{
    DriverConnection, DriverFactory, DriverLogin, NativeDriverFactory, QueryLimits, RowSink,
};
use restoreplane_workbench::generated_sql::WriteCertification;
use restoreplane_workbench::metadata::MetadataCatalog;
use restoreplane_workbench::movement_codec::{ExportFormat, ExportWriter, FormulaPolicy};
use restoreplane_workbench::WorkbenchError;
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use uuid::Uuid;
use zeroize::Zeroizing;

#[derive(Default)]
struct Rows(Vec<Vec<TypedCell>>);
#[async_trait]
impl RowSink for Rows {
    async fn columns(&mut self, _: Vec<Column>) -> Result<(), WorkbenchError> {
        Ok(())
    }
    async fn row(&mut self, row: Vec<TypedCell>) -> Result<(), WorkbenchError> {
        self.0.push(row);
        Ok(())
    }
}
async fn direct(connection: &mut dyn DriverConnection, sql: &str) -> Vec<Vec<TypedCell>> {
    let mut rows = Rows::default();
    connection
        .execute(sql, QueryLimits::default(), &mut rows)
        .await
        .unwrap_or_else(|error| panic!("owned fixture SQL {}", error.code()));
    rows.0
}
fn descriptor(data: &serde_json::Value, engine: Engine) -> ConnectDescriptor {
    let ca = data["ca_file"].as_str().unwrap();
    ConnectDescriptor {
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
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit owned WORKBENCH_LIVE_FIXTURES; no side database or credentials output"]
async fn owned_verified_tls_three_engines_import_exact_null_empty_decimal_and_bytes() {
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
        let descriptor = descriptor(data, engine);
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
        let table = format!("wb07_{}", Uuid::new_v4().simple());
        let binary = if engine == Engine::Postgresql {
            "BYTEA"
        } else {
            "BLOB"
        };
        direct(observer.as_mut(), &format!("CREATE TABLE {table}(id BIGINT PRIMARY KEY,label VARCHAR(80),amount DECIMAL(20,4),payload {binary})")).await;
        let fixture = support::QueryFixture::new(
            support::Fixture::connect(descriptor, username, secret, Mode::ReadWrite).await,
        );
        let ctx = &fixture.database.context;
        let session = fixture.database.session;
        let metadata = Arc::new(MetadataCatalog::new(
            fixture.database.runtime.clone(),
            fixture.streams.budget(),
        ));
        let files = Arc::new(FileCapabilities::new(
            fixture.database.runtime.clone(),
            fixture.streams.budget(),
        ));
        let certification = WriteCertification::from_verified_build("section06-native-20261117-current-exact".into(), vec![
            (Engine::Postgresql, "PostgreSQL 17.11 on x86_64-pc-linux-musl, compiled by gcc (Alpine 15.2.0) 15.2.0, 64-bit".into()),
            (Engine::Mysql, "8.4.13".into()), (Engine::Mariadb, "11.4.13-MariaDB-ubu2404".into()),
            (Engine::Sqlite, "3.54.1".into()),
        ]).unwrap();
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
                table.clone(),
                PageInput {
                    cursor: None,
                    limit: 100,
                },
            )
            .await
            .unwrap()
            .items
            .into_iter()
            .find(|item| item.name == table)
            .unwrap()
            .object;
        let relation = metadata
            .resolve_relation(ctx, session, &object)
            .await
            .unwrap();
        let columns = relation
            .columns
            .iter()
            .map(|column| column.column.clone())
            .collect::<Vec<_>>();
        let expected = vec![
            vec![
                TypedCell::I64 { value: "1".into() },
                TypedCell::Null,
                TypedCell::Decimal {
                    value: "123.4500".into(),
                    scale: Some(4),
                },
                TypedCell::Bytes {
                    base64: "AAEC/w==".into(),
                },
            ],
            vec![
                TypedCell::I64 { value: "2".into() },
                TypedCell::Text {
                    value: String::new(),
                },
                TypedCell::Decimal {
                    value: "0.0000".into(),
                    scale: Some(4),
                },
                TypedCell::Bytes {
                    base64: String::new(),
                },
            ],
            vec![
                TypedCell::I64 { value: "3".into() },
                TypedCell::Text {
                    value: "한글 '; -- 값".into(),
                },
                TypedCell::Decimal {
                    value: "-1.2500".into(),
                    scale: Some(4),
                },
                TypedCell::Null,
            ],
        ];
        let mut writer = ExportWriter::new(
            Vec::new(),
            ExportFormat::TypedJson,
            &columns,
            FormulaPolicy::Escape,
        )
        .unwrap();
        for row in &expected {
            writer.write_row(row).unwrap();
        }
        let source_dir = tempfile::tempdir().unwrap();
        let source_path = source_dir.path().join("source.json");
        std::fs::write(&source_path, writer.finish().unwrap()).unwrap();
        let handle = files
            .register_source(
                ctx,
                session,
                NativeSourceSelection::from_native_dialog(&source_path).unwrap(),
            )
            .unwrap();
        let source = files
            .source_columns(ctx, session, handle, MovementFormat::Json)
            .unwrap();
        let mapping = source
            .columns
            .iter()
            .map(|source| {
                (
                    source.source_key.clone(),
                    columns
                        .iter()
                        .find(|column| column.name == source.name)
                        .unwrap()
                        .column_id,
                )
            })
            .collect();
        let preview = imports
            .preview(
                ctx,
                ImportPreviewInput {
                    session,
                    object,
                    source_handle: handle,
                    format: MovementFormat::Json,
                    mapping,
                    page: PageInput {
                        cursor: None,
                        limit: 100,
                    },
                },
            )
            .await
            .unwrap();
        let input = ImportStartInput {
            session,
            preview_id: preview.preview_id,
            confirmed_warnings: preview.warnings,
        };
        let token = imports.prepare_start(ctx, &input).await.unwrap();
        let admitted = imports
            .start(ctx, &token.attempt_token, input.clone())
            .await
            .map_err(|error| error.error)
            .unwrap();
        let operation = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let operation = fixture
                    .journal
                    .get(ctx, &fixture.database.scope, admitted.operation_id)
                    .unwrap();
                if operation.state.is_terminal() {
                    break operation;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(operation.state, OperationState::Completed, "{name}");
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
        assert_eq!(status.progress.confirmed_committed_rows.value(), 3);
        assert_eq!(status.children.items.len(), 1);
        assert_eq!(status.children.items[0].state, OperationState::Committed);
        let observed = direct(
            observer.as_mut(),
            &format!("SELECT id,label,amount,payload FROM {table} ORDER BY id"),
        )
        .await;
        assert_eq!(observed, expected, "{name}");
        let duplicate = imports
            .start(ctx, &token.attempt_token, input)
            .await
            .map_err(|error| error.error)
            .unwrap();
        assert!(duplicate.deduplicated);
        assert_eq!(duplicate.operation_id, admitted.operation_id);
        direct(observer.as_mut(), &format!("DROP TABLE {table}")).await;
        println!("M3 boundary_violations=0; engine={name}; verified_tls=true; actual_import_rows=3; committed_children=1; duplicate_dispatches=0; os_standard_integrity_e2e=false");
    }
}
