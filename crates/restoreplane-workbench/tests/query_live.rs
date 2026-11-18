mod support;
use async_trait::async_trait;
use restoreplane_domain::workbench::*;
use restoreplane_domain::workbench_metadata::Limits;
use restoreplane_domain::workbench_query::*;
use restoreplane_workbench::driver::{
    DriverConnection, DriverFactory, DriverLogin, NativeDriverFactory, QueryLimits, RowSink,
};
use restoreplane_workbench::WorkbenchError;
use std::collections::BTreeMap;
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
        assert!(self.rows.len() < 100);
        self.rows.push(row);
        Ok(())
    }
}

struct CommitAckFault {
    sends: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}
struct FaultConnection {
    inner: Box<dyn DriverConnection>,
    sends: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}
#[async_trait]
impl DriverFactory for CommitAckFault {
    async fn connect(
        &self,
        descriptor: &ConnectDescriptor,
        login: DriverLogin,
        mode: Mode,
    ) -> Result<Box<dyn DriverConnection>, WorkbenchError> {
        let inner = NativeDriverFactory::default()
            .connect(descriptor, login, mode)
            .await?;
        Ok(Box::new(FaultConnection {
            inner,
            sends: self.sends.clone(),
        }))
    }
}
#[async_trait]
impl DriverConnection for FaultConnection {
    fn facts(&self) -> &restoreplane_workbench::driver::DriverFacts {
        self.inner.facts()
    }
    fn cancel_handle(&self) -> std::sync::Arc<dyn restoreplane_workbench::driver::DriverCancel> {
        self.inner.cancel_handle()
    }
    async fn set_read_only(&mut self, value: bool) -> Result<(), WorkbenchError> {
        self.inner.set_read_only(value).await
    }
    async fn transaction_read_only(&mut self) -> Result<bool, WorkbenchError> {
        self.inner.transaction_read_only().await
    }
    async fn configure_limits(&mut self, limit: std::time::Duration) -> Result<(), WorkbenchError> {
        self.inner.configure_limits(limit).await
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
        self.inner.execute_bound(sql, params, limits, sink).await
    }
    async fn transaction_control(
        &mut self,
        command: restoreplane_workbench::driver::TransactionCommand,
    ) -> Result<(), WorkbenchError> {
        if matches!(
            command,
            restoreplane_workbench::driver::TransactionCommand::Commit
        ) {
            self.sends.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.transaction_control(command).await?;
            return Err(WorkbenchError::CancelUnsettled);
        }
        self.inner.transaction_control(command).await
    }
    async fn close(&mut self) -> Result<(), WorkbenchError> {
        self.inner.close().await
    }
}
#[tokio::test]
#[ignore = "owned actual PG commit + injected bridge acknowledgement loss; not a packet-loss simulation"]
async fn positive_pg_commit_with_discarded_ack_quarantines_and_never_resends() {
    let bytes = Zeroizing::new(
        std::fs::read(
            std::env::var_os("WORKBENCH_LIVE_FIXTURES").expect("owned fixture config required"),
        )
        .unwrap(),
    );
    let config: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let data = &config["postgresql"];
    let descriptor = descriptor(data, Engine::Postgresql);
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
    let table = format!("wb05_ack_{}", uuid::Uuid::new_v4().simple());
    direct(
        observer.as_mut(),
        &format!("CREATE TABLE {table}(id BIGINT PRIMARY KEY)"),
    )
    .await;
    let sends = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut fixture = support::QueryFixture::new(
        support::Fixture::connect_with_factory(
            descriptor,
            username,
            secret,
            Mode::ReadWrite,
            std::sync::Arc::new(CommitAckFault {
                sends: sends.clone(),
            }),
        )
        .await,
    );
    let tx = fixture
        .coordinator
        .transaction_begin(
            &fixture.database.context,
            TransactionBeginInput {
                session: fixture.database.session,
                isolation: "read_committed".into(),
                read_only: false,
            },
        )
        .await
        .unwrap();
    let query = reviewed(&fixture, &format!("INSERT INTO {table} VALUES(1)")).await;
    terminal(&fixture, query.admission.operation_id).await;
    let input = TransactionInput {
        session: fixture.database.session,
        transaction_id: tx.transaction_id,
    };
    let token = fixture
        .coordinator
        .prepare_commit(&fixture.database.context, input.clone())
        .await
        .unwrap();
    assert!(fixture
        .coordinator
        .transaction_commit(
            &fixture.database.context,
            token.attempt_token.clone(),
            input.clone()
        )
        .await
        .is_err());
    let operation = fixture
        .journal
        .get(
            &fixture.database.context,
            &fixture.database.scope,
            tx.transaction_id,
        )
        .unwrap();
    assert_eq!(operation.state, OperationState::OutcomeUnknown);
    assert_eq!(
        operation.transaction_outcome,
        TransactionOutcome::OutcomeUnknown
    );
    assert!(
        fixture
            .coordinator
            .transaction_commit(&fixture.database.context, token.attempt_token, input)
            .await
            .unwrap()
            .deduplicated
    );
    assert_eq!(sends.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        direct(
            observer.as_mut(),
            &format!("SELECT COUNT(*)::bigint FROM {table}")
        )
        .await[0][0],
        TypedCell::I64 { value: "1".into() }
    );
    let replacement = fixture
        .coordinator
        .reconnect(
            &fixture.database.context,
            fixture.database.session,
            &"b".repeat(64),
        )
        .await
        .unwrap();
    fixture.database.session = replacement.session;
    let review = fixture
        .coordinator
        .review(
            &fixture.database.context,
            QueryReviewInput {
                session: fixture.database.session,
                sql: format!("INSERT INTO {table} VALUES(2)"),
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
            max_rows: 200,
            max_bytes: 512 * 1024,
            statement_timeout_ms: 30_000,
            lock_wait_ms: 2000,
        },
    };
    let token = fixture
        .coordinator
        .prepare_query(&fixture.database.context, input.clone())
        .await
        .unwrap();
    assert!(matches!(
        fixture
            .coordinator
            .run(&fixture.database.context, token.attempt_token, input)
            .await,
        Err(WorkbenchError::OutcomeUnknown)
    ));
    assert_eq!(
        direct(
            observer.as_mut(),
            &format!("SELECT COUNT(*)::bigint FROM {table}")
        )
        .await[0][0],
        TypedCell::I64 { value: "1".into() }
    );
    assert_eq!(sends.load(std::sync::atomic::Ordering::SeqCst), 1);
    fixture
        .coordinator
        .retire_session(&fixture.database.context, fixture.database.session, false)
        .await
        .unwrap();
    direct(observer.as_mut(), &format!("DROP TABLE {table}")).await;
    observer.close().await.unwrap();
    println!("05 actualPG verifiedTLS positiveCommit=true bridgeAckDiscarded=true observedRows=1 unknown=true duplicateCommitSends=0 newMutationBlocked=true realPacketLoss=false osStandardIntegrityE2E=false");
}
async fn direct(connection: &mut dyn DriverConnection, sql: &str) -> Vec<Vec<TypedCell>> {
    let mut sink = Sink::default();
    connection
        .execute(
            sql,
            QueryLimits {
                max_rows: 100,
                max_bytes: 512 * 1024,
                ..QueryLimits::default()
            },
            &mut sink,
        )
        .await
        .unwrap_or_else(|e| panic!("owned setup/observer error {}", e.code()));
    sink.rows
}
#[tokio::test]
#[ignore = "owned verified TLS MySQL/MariaDB fixtures; no external database"]
async fn full_autocommit_update_then_ddl_then_server_error_preserves_confirmed_commits() {
    let bytes = Zeroizing::new(
        std::fs::read(
            std::env::var_os("WORKBENCH_LIVE_FIXTURES").expect("owned fixture config required"),
        )
        .unwrap(),
    );
    let config: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    for (name, engine) in [("mysql", Engine::Mysql), ("mariadb", Engine::Mariadb)] {
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
        let table = format!("wb05_partial_{}", uuid::Uuid::new_v4().simple());
        let ddl = format!("{table}_ddl");
        direct(
            observer.as_mut(),
            &format!("CREATE TABLE {table}(id BIGINT PRIMARY KEY,label VARCHAR(32)) ENGINE=InnoDB"),
        )
        .await;
        direct(
            observer.as_mut(),
            &format!("INSERT INTO {table} VALUES(1,'before')"),
        )
        .await;
        let fixture = support::QueryFixture::new(
            support::Fixture::connect(descriptor, username, secret, Mode::Full).await,
        );
        let run=reviewed(&fixture,&format!("UPDATE {table} SET label='after' WHERE id=1; CREATE TABLE {ddl}(id BIGINT PRIMARY KEY) ENGINE=InnoDB; INSERT INTO {table}(id,label) VALUES(1,'duplicate')")).await;
        let op = terminal(&fixture, run.admission.operation_id).await;
        assert_eq!(op.state, OperationState::Partial, "{name}");
        assert_eq!(op.statements.len(), 3);
        assert_eq!(op.statements[0].effect_evidence, EffectEvidence::Committed);
        assert_eq!(op.statements[1].effect_evidence, EffectEvidence::Committed);
        assert_eq!(op.statements[2].outcome, StatementStatus::Failed);
        assert_eq!(op.statements[2].effect_evidence, EffectEvidence::RolledBack);
        assert_eq!(
            direct(
                observer.as_mut(),
                &format!("SELECT label FROM {table} WHERE id=1")
            )
            .await,
            vec![vec![TypedCell::Text {
                value: "after".into()
            }]]
        );
        assert!(direct(observer.as_mut(), &format!("SELECT id FROM {ddl}"))
            .await
            .is_empty());
        fixture
            .coordinator
            .retire_session(&fixture.database.context, fixture.database.session, false)
            .await
            .unwrap();
        direct(observer.as_mut(), &format!("DROP TABLE {ddl}")).await;
        direct(observer.as_mut(), &format!("DROP TABLE {table}")).await;
        observer.close().await.unwrap();
        println!("05 {name} full partial commit observed: UPDATE committed, DDL committed, duplicate rejection rolled back; earlier data retained; external effects not certified");
    }
}
async fn reviewed(fixture: &support::QueryFixture, sql: &str) -> QueryAdmission {
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
        .unwrap_or_else(|e| panic!("review {}", e.code()));
    let input = QueryRunInput {
        session: fixture.database.session,
        review_id: review.review_id,
        confirmed_warnings: review.warnings,
        limits: Limits {
            max_rows: 200,
            max_bytes: 512 * 1024,
            statement_timeout_ms: 30_000,
            lock_wait_ms: 2000,
        },
    };
    let token = fixture
        .coordinator
        .prepare_query(&fixture.database.context, input.clone())
        .await
        .unwrap_or_else(|e| panic!("prepare {}", e.code()));
    fixture
        .coordinator
        .run(&fixture.database.context, token.attempt_token, input)
        .await
        .unwrap_or_else(|e| panic!("admit {}", e.code()))
}
async fn terminal(fixture: &support::QueryFixture, id: uuid::Uuid) -> Operation {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
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
    .expect("owned operation did not settle")
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
#[tokio::test]
#[ignore = "requires explicit owned WORKBENCH_LIVE_FIXTURES; no credentials or SQL output"]
async fn tls_three_engines_query_boundaries_manual_tx_and_readonly_holds_are_observed() {
    let bytes = Zeroizing::new(
        std::fs::read(
            std::env::var_os("WORKBENCH_LIVE_FIXTURES").expect("owned fixture config required"),
        )
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
            .expect("owned verified TLS setup");
        assert!(observer.facts().verified_tls);
        let table = format!("wb05_{}", uuid::Uuid::new_v4().simple());
        direct(
            observer.as_mut(),
            &format!("CREATE TABLE {table}(id BIGINT PRIMARY KEY,label VARCHAR(80))"),
        )
        .await;
        let fixture = support::QueryFixture::new(
            support::Fixture::connect(
                descriptor.clone(),
                username.clone(),
                secret.clone(),
                Mode::ReadWrite,
            )
            .await,
        );
        let admission = reviewed(&fixture, &format!("INSERT INTO {table} VALUES(1,'one')")).await;
        let operation = terminal(&fixture, admission.admission.operation_id).await;
        assert_eq!(operation.state, OperationState::Completed, "{name}");
        assert_eq!(
            operation.statements[0].effect_evidence,
            EffectEvidence::Committed
        );
        let tx = fixture
            .coordinator
            .transaction_begin(
                &fixture.database.context,
                TransactionBeginInput {
                    session: fixture.database.session,
                    isolation: "repeatable_read".into(),
                    read_only: false,
                },
            )
            .await
            .unwrap_or_else(|e| panic!("{name} native BEGIN {}", e.code()));
        let update = reviewed(
            &fixture,
            &format!("UPDATE {table} SET label='pending' WHERE id=1"),
        )
        .await;
        let operation = terminal(&fixture, update.admission.operation_id).await;
        assert_eq!(
            operation.statements[0].effect_evidence,
            EffectEvidence::PendingTransaction,
            "{name}"
        );
        assert_eq!(
            direct(
                observer.as_mut(),
                &format!("SELECT label FROM {table} WHERE id=1")
            )
            .await[0][0],
            TypedCell::Text {
                value: "one".into()
            }
        );
        let rejected = reviewed(
            &fixture,
            &format!("INSERT INTO {table} VALUES(1,'duplicate')"),
        )
        .await;
        let failed = terminal(&fixture, rejected.admission.operation_id).await;
        assert_eq!(
            failed.state,
            OperationState::Failed,
            "{name} observed rejection"
        );
        assert_eq!(
            failed.statements[0].effect_evidence,
            EffectEvidence::PendingTransaction
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let state = fixture
                    .coordinator
                    .transaction_get(&fixture.database.context, fixture.database.session)
                    .await
                    .unwrap()
                    .unwrap()
                    .state;
                if state == TransactionState::FailedNeedsRollback {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let rollback = fixture
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
        assert_eq!(rollback.state, TransactionState::RolledBack);
        let tx = fixture
            .coordinator
            .transaction_begin(
                &fixture.database.context,
                TransactionBeginInput {
                    session: fixture.database.session,
                    isolation: "repeatable_read".into(),
                    read_only: false,
                },
            )
            .await
            .unwrap();
        let update = reviewed(
            &fixture,
            &format!("UPDATE {table} SET label='committed' WHERE id=1"),
        )
        .await;
        terminal(&fixture, update.admission.operation_id).await;
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
        assert!(fixture
            .coordinator
            .transaction_commit(
                &fixture.database.context,
                "forged-token".into(),
                input.clone()
            )
            .await
            .is_err());
        assert!(
            fixture
                .coordinator
                .transaction_commit(&fixture.database.context, token.attempt_token, input)
                .await
                .unwrap()
                .deduplicated
        );
        assert_eq!(
            direct(
                observer.as_mut(),
                &format!("SELECT label FROM {table} WHERE id=1")
            )
            .await[0][0],
            TypedCell::Text {
                value: "committed".into()
            }
        );
        let readonly = support::QueryFixture::new(
            support::Fixture::connect(
                descriptor.clone(),
                username.clone(),
                secret.clone(),
                Mode::ReadOnly,
            )
            .await,
        );
        let query = reviewed(
            &readonly,
            &format!("SELECT id,label FROM {table} ORDER BY id"),
        )
        .await;
        let operation = terminal(&readonly, query.admission.operation_id).await;
        assert_eq!(
            operation.state,
            OperationState::Completed,
            "{name} actual readonly hold"
        );
        assert_eq!(
            operation.statements[0].effect_evidence,
            EffectEvidence::ReadOnly
        );
        let tx = readonly
            .coordinator
            .transaction_begin(
                &readonly.database.context,
                TransactionBeginInput {
                    session: readonly.database.session,
                    isolation: "default".into(),
                    read_only: true,
                },
            )
            .await
            .unwrap();
        let input = TransactionInput {
            session: readonly.database.session,
            transaction_id: tx.transaction_id,
        };
        let token = readonly
            .coordinator
            .prepare_commit(&readonly.database.context, input.clone())
            .await
            .unwrap();
        readonly
            .coordinator
            .transaction_commit(&readonly.database.context, token.attempt_token, input)
            .await
            .unwrap();
        if engine == Engine::Postgresql {
            let basis = support::QueryFixture::new(
                support::Fixture::connect(
                    descriptor.clone(),
                    username.clone(),
                    secret.clone(),
                    Mode::ReadOnly,
                )
                .await,
            );
            let review = basis
                .coordinator
                .review(
                    &basis.database.context,
                    QueryReviewInput {
                        session: basis.database.session,
                        sql: format!("SELECT id FROM {table}"),
                        purpose: QueryPurpose::Run,
                    },
                )
                .await
                .unwrap();
            let input = QueryRunInput {
                session: basis.database.session,
                review_id: review.review_id,
                confirmed_warnings: review.warnings,
                limits: Limits {
                    max_rows: 200,
                    max_bytes: 512 * 1024,
                    statement_timeout_ms: 30_000,
                    lock_wait_ms: 2000,
                },
            };
            let token = basis
                .coordinator
                .prepare_query(&basis.database.context, input.clone())
                .await
                .unwrap();
            basis
                .database
                .runtime
                .with_connection(
                    &basis.database.context,
                    basis.database.session,
                    AuthorizePurpose::Read,
                    |connection| {
                        Box::pin(async move {
                            direct(connection, "SET TIME ZONE '+03:00'").await;
                            Ok(())
                        })
                    },
                )
                .await
                .unwrap();
            let rejected = basis
                .coordinator
                .run(&basis.database.context, token.attempt_token, input)
                .await
                .unwrap();
            let operation = terminal(&basis, rejected.admission.operation_id).await;
            assert_eq!(operation.state, OperationState::Failed);
            let dispatch: i64 = basis
                .store
                .read(|connection| {
                    connection
                        .query_row(
                            "SELECT dispatch_count FROM operations WHERE operation_id=?1",
                            [rejected.admission.operation_id.to_string()],
                            |row| row.get(0),
                        )
                        .map_err(|_| WorkbenchError::StoreUnavailable)
                })
                .unwrap();
            assert_eq!(dispatch,0,"changed native settings invalidate reviewed intent before the user statement is sent");
            basis
                .coordinator
                .retire_session(&basis.database.context, basis.database.session, false)
                .await
                .unwrap();
            let child = format!("{table}_child");
            direct(observer.as_mut(),&format!("CREATE TABLE {child}(id BIGINT PRIMARY KEY,parent_id BIGINT REFERENCES {table}(id) DEFERRABLE INITIALLY DEFERRED)")).await;
            let failed = reviewed(
                &fixture,
                &format!(
                    "INSERT INTO {table} VALUES(2,'prior'); INSERT INTO {child} VALUES(1,999)"
                ),
            )
            .await;
            let operation = terminal(&fixture, failed.admission.operation_id).await;
            assert_eq!(operation.state, OperationState::Partial);
            assert_eq!(
                operation.statements[0].effect_evidence,
                EffectEvidence::Committed
            );
            assert_ne!(
                operation.statements[1].effect_evidence,
                EffectEvidence::Committed
            );
            assert_eq!(operation.statements[1].outcome, StatementStatus::Failed);
            assert_eq!(
                operation.statements[1].effect_evidence,
                EffectEvidence::RolledBack
            );
            assert_eq!(
                direct(
                    observer.as_mut(),
                    &format!("SELECT COUNT(*)::bigint FROM {child}")
                )
                .await[0][0],
                TypedCell::I64 { value: "0".into() }
            );
            direct(observer.as_mut(), &format!("DROP TABLE {child}")).await;
        }
        readonly
            .coordinator
            .retire_session(&readonly.database.context, readonly.database.session, false)
            .await
            .unwrap();
        fixture
            .coordinator
            .retire_session(&fixture.database.context, fixture.database.session, false)
            .await
            .unwrap();
        direct(observer.as_mut(), &format!("DROP TABLE {table}")).await;
        observer.close().await.unwrap();
        println!("05 owned engine={name}; verifiedTLS/fully-drained-completion/manual-pending/rollback/commit-dedupe/forged-token-refusal/actual-readonly-hold=true");
    }
}

// Native Explain regression tests; execution is subject to the owned test lease.
#[path = "support/explain_wire_probe.rs"]
mod explain_wire_probe;

async fn explain_rejected_without_dispatch(
    fixture: &support::QueryFixture,
    purpose: QueryPurpose,
    wire: &str,
    probe: &explain_wire_probe::Probe,
) {
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
        "native Explain gate must reject before admission"
    );
    assert_eq!(
        probe.count(wire),
        0,
        "rejected user SQL must never enter the actual native adapter"
    );
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
}

async fn explain_positive(
    fixture: &support::QueryFixture,
    name: &str,
    engine: Engine,
    purpose: QueryPurpose,
    wire: &str,
    probe: &explain_wire_probe::Probe,
) {
    use restoreplane_domain::workbench_metadata::{Count, PageInput};
    use restoreplane_domain::workbench_stream::{Ack, Completion, FrameBatch, ResultEvent};

    let facts = fixture
        .database
        .runtime
        .with_connection(
            &fixture.database.context,
            fixture.database.session,
            AuthorizePurpose::Read,
            move |connection| Box::pin(async move { Ok(connection.facts().clone()) }),
        )
        .await
        .unwrap();
    assert_eq!(facts.engine, engine);
    assert!(facts.verified_tls);
    assert!(!facts.server_version.is_empty());
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
        .await
        .unwrap_or_else(|error| panic!("{name} native Explain review {}", error.code()));
    assert_eq!(
        review.digest,
        blake3::hash(wire.as_bytes()).to_hex().to_string()
    );
    let analyze = purpose == QueryPurpose::ExplainAnalyze;
    let expected_warning = if analyze {
        "explain_analyze_executes"
    } else {
        "plan_is_not_effect_proof"
    };
    assert!(review
        .warnings
        .iter()
        .any(|warning| warning == expected_warning));
    if analyze {
        assert_eq!(review.required_mode, Mode::Full);
    }
    let input = QueryRunInput {
        session: fixture.database.session,
        review_id: review.review_id,
        confirmed_warnings: review.warnings,
        limits: Limits {
            max_rows: 200,
            max_bytes: 512 * 1024,
            statement_timeout_ms: 30_000,
            lock_wait_ms: 2_000,
        },
    };
    let prepared = fixture
        .coordinator
        .prepare_query(&fixture.database.context, input.clone())
        .await
        .unwrap();
    let token = prepared.attempt_token;
    let admission = fixture
        .coordinator
        .run(&fixture.database.context, token.clone(), input.clone())
        .await
        .unwrap();
    assert!(admission.execution_epoch.value() > 0);
    let results = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
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
            if matches!(results.count, Count::Exact { .. }) {
                break results.items;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Explain worker failed to settle within the existing tiny-fixture watchdog");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].completeness, Some(Completion::Complete));
    assert_eq!(
        probe.count(wire),
        1,
        "the exact reviewed wire must reach the actual adapter once"
    );
    let identity = &results[0].identity;
    assert_eq!(identity.query_run_id, admission.query_run_id);
    assert_eq!(identity.session_id, fixture.database.session.session_id);
    assert_eq!(
        identity.connection_generation,
        fixture.database.session.connection_generation
    );
    let mut sequence = 0u64;
    let mut schema_count = 0usize;
    let mut row_count = 0usize;
    let mut terminal_complete = false;
    let mut actual_timing_seen = false;
    for _ in 0..64 {
        let pull = fixture
            .streams
            .frames(&fixture.database.context, identity, Some(sequence))
            .unwrap();
        let batch: FrameBatch =
            serde_json::from_value(serde_json::to_value(&pull).unwrap()).unwrap();
        assert_eq!(batch.frames.len(), 1, "one retained bounded frame per pull");
        let frame = &batch.frames[0];
        assert_eq!(&frame.identity, identity);
        assert_eq!(frame.sequence.value(), sequence + 1);
        match &frame.event {
            ResultEvent::Schema { columns, .. } => {
                schema_count += 1;
                assert!(!columns.is_empty());
            }
            ResultEvent::Rows { start_row, rows } => {
                assert_eq!(start_row.value(), row_count as u64);
                row_count += rows.len();
                actual_timing_seen |= rows.iter().flat_map(|row| &row.cells).any(|cell| {
                    matches!(cell, TypedCell::Text { value } if value.contains("actual time="))
                });
            }
            ResultEvent::Terminal {
                completeness,
                rows_seen,
                error_code,
                ..
            } => {
                assert_eq!(*completeness, Completion::Complete);
                assert_eq!(rows_seen.value(), row_count as u64);
                assert!(error_code.is_none());
                terminal_complete = true;
            }
        }
        sequence = frame.sequence.value();
        pull.mark_sent().unwrap();
        drop(pull);
        let acknowledged = fixture
            .streams
            .ack(
                &fixture.database.context,
                &Ack {
                    identity: identity.clone(),
                    highest_contiguous_sequence: sequence.into(),
                },
            )
            .unwrap();
        assert_eq!(acknowledged.accepted_through.value(), sequence);
        if !batch.has_more {
            break;
        }
    }
    assert_eq!(schema_count, 1);
    assert!(row_count > 0 && terminal_complete);
    if analyze {
        assert!(
            actual_timing_seen,
            "actual server Analyze result required; plan text is not logged"
        );
    }
    let operation = terminal(fixture, admission.admission.operation_id).await;
    assert_eq!(operation.statements.len(), 1);
    assert_eq!(operation.statements[0].outcome, StatementStatus::Succeeded);
    if analyze {
        // A returned plan is evidence of server execution, not proof that arbitrary effects were absent.
        assert_eq!(operation.state, OperationState::OutcomeUnknown);
        assert_eq!(
            operation.statements[0].effect_evidence,
            EffectEvidence::OutcomeUnknown
        );
        assert_eq!(
            fixture
                .database
                .runtime
                .get(&fixture.database.context, fixture.database.session)
                .unwrap()
                .state,
            SessionState::Retiring
        );
        let callback = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = callback.clone();
        let stale = fixture
            .database
            .runtime
            .with_connection(
                &fixture.database.context,
                fixture.database.session,
                AuthorizePurpose::Read,
                move |_| {
                    Box::pin(async move {
                        observed.store(true, std::sync::atomic::Ordering::SeqCst);
                        Ok(())
                    })
                },
            )
            .await;
        assert!(matches!(stale, Err(WorkbenchError::SessionStale)));
        assert!(!callback.load(std::sync::atomic::Ordering::SeqCst));
    } else {
        assert_eq!(operation.state, OperationState::Completed);
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
    }
    // Same immutable attempt returns durable status; no new statement is dispatched, even after Analyze retirement.
    let duplicate = fixture
        .coordinator
        .run(&fixture.database.context, token, input)
        .await
        .unwrap();
    assert!(duplicate.admission.deduplicated);
    assert_eq!(
        duplicate.admission.operation_id,
        admission.admission.operation_id
    );
    assert_eq!(duplicate.query_run_id, admission.query_run_id);
    assert_eq!(duplicate.execution_epoch, admission.execution_epoch);
    let dispatches: i64 = fixture
        .store
        .read(|connection| {
            connection
                .query_row(
                    "SELECT COALESCE(SUM(dispatch_count),0) FROM operations",
                    [],
                    |row| row.get(0),
                )
                .map_err(|_| WorkbenchError::StoreUnavailable)
        })
        .unwrap();
    assert_eq!(dispatches, 1);
    assert_eq!(
        probe.count(wire),
        1,
        "same-token status lookup must not resend SQL"
    );
    println!("Explain actual fixture {name}: verified TLS, exact reviewed digest, schema1/rows{row_count}/Complete/pull ACK; analyze={analyze}, duplicate same-id/epoch, dispatch1");
}

#[tokio::test]
#[ignore = "requires explicit owned WORKBENCH_LIVE_FIXTURES; exact Explain matrix only, no credentials/SQL/plan output"]
async fn owned_tls_explain_purpose_matrix_roundtrip() {
    let bytes = Zeroizing::new(
        std::fs::read(
            std::env::var_os("WORKBENCH_LIVE_FIXTURES").expect("owned fixture config required"),
        )
        .unwrap(),
    );
    let config: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let source = "SELECT 1 AS exact_source /* 한글 원문 */\r\n";
    let original = source.as_bytes().to_vec();
    for (name, engine) in [
        ("postgresql", Engine::Postgresql),
        ("mysql", Engine::Mysql),
        ("mariadb", Engine::Mariadb),
    ] {
        let data = &config[name];
        let plan_wire = format!("EXPLAIN {source}");
        let analyze_wire = match engine {
            Engine::Postgresql => Some(format!("EXPLAIN ANALYZE {source}")),
            Engine::Mysql => Some(format!("EXPLAIN ANALYZE FORMAT=TREE {source}")),
            _ => None,
        };
        let mut watched = vec![source, plan_wire.as_str()];
        if let Some(wire) = &analyze_wire {
            watched.push(wire.as_str());
        }
        let probe = explain_wire_probe::Probe::new(&watched);
        let plan = support::QueryFixture::new(
            support::Fixture::connect_with_factory(
                descriptor(data, engine),
                data["username"].as_str().unwrap().to_owned(),
                Zeroizing::new(data["password"].as_str().unwrap().to_owned()),
                Mode::ReadOnly,
                probe.factory(),
            )
            .await,
        );
        explain_rejected_without_dispatch(&plan, QueryPurpose::ExplainPlan, source, &probe).await;
        explain_rejected_without_dispatch(&plan, QueryPurpose::ExplainAnalyze, &plan_wire, &probe)
            .await;
        if let Some(wire) = &analyze_wire {
            explain_rejected_without_dispatch(&plan, QueryPurpose::ExplainAnalyze, wire, &probe)
                .await;
        }
        explain_positive(
            &plan,
            name,
            engine,
            QueryPurpose::ExplainPlan,
            &plan_wire,
            &probe,
        )
        .await;
        if let Some(wire) = analyze_wire {
            let read_write = support::QueryFixture::new(
                support::Fixture::connect_with_factory(
                    descriptor(data, engine),
                    data["username"].as_str().unwrap().to_owned(),
                    Zeroizing::new(data["password"].as_str().unwrap().to_owned()),
                    Mode::ReadWrite,
                    probe.factory(),
                )
                .await,
            );
            explain_rejected_without_dispatch(
                &read_write,
                QueryPurpose::ExplainAnalyze,
                &wire,
                &probe,
            )
            .await;
            let full = support::QueryFixture::new(
                support::Fixture::connect_with_factory(
                    descriptor(data, engine),
                    data["username"].as_str().unwrap().to_owned(),
                    Zeroizing::new(data["password"].as_str().unwrap().to_owned()),
                    Mode::Full,
                    probe.factory(),
                )
                .await,
            );
            explain_rejected_without_dispatch(&full, QueryPurpose::ExplainPlan, &wire, &probe)
                .await;
            explain_positive(
                &full,
                name,
                engine,
                QueryPurpose::ExplainAnalyze,
                &wire,
                &probe,
            )
            .await;
        }
        assert_eq!(source.as_bytes(), original.as_slice());
    }
}
