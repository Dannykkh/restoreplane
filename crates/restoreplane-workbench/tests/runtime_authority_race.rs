mod support;
use async_trait::async_trait;
use restoreplane_domain::workbench::*;
use restoreplane_domain::{ConnectionProfileId, DatabaseTargetId, WorkspaceId};
use restoreplane_workbench::credentials::CredentialProvider;
use restoreplane_workbench::driver::{DriverLogin, NativeDriverFactory, QueryLimits, RowSink};
use restoreplane_workbench::session::RuntimeLimits;
use restoreplane_workbench::{
    BindingAuthorizer, IntegrityLevel, InvocationContext, NativePrincipal, WorkbenchError,
    WorkbenchState,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::Notify;
use uuid::Uuid;

struct AuthorityRace {
    store: restoreplane_catalog::OperationalStore,
    epoch: String,
    calls: AtomicUsize,
    pause_on: AtomicUsize,
    read: Notify,
    release: Notify,
}
#[async_trait]
impl BindingAuthorizer for AuthorityRace {
    async fn authorize(
        &self,
        context: &InvocationContext,
        scope: &Scope,
        _: AuthorizePurpose,
        expected: Option<&str>,
    ) -> Result<BindingReceipt, WorkbenchError> {
        if expected.is_some_and(|value| value != "b".repeat(64)) {
            return Err(WorkbenchError::BindingStale);
        }
        // This is the actual v4 read transaction; pause only after it ended.
        let snapshot = self
            .store
            .workbench_binding()
            .snapshot(scope, context.principal().owner_id())
            .map_err(|_| WorkbenchError::BindingUnproven)?;
        let receipt = BindingReceipt {
            scope: scope.clone(),
            descriptor: snapshot.descriptor,
            descriptor_revision: "b".repeat(64),
            authority_epoch: self.epoch.clone(),
            authorized_at_ms: restoreplane_workbench::types::now_ms().into(),
            read_lease_expires_at_ms: None,
            observed_conflicts: snapshot.observed_conflicts,
        };
        if self.calls.fetch_add(1, Ordering::SeqCst) + 1 == self.pause_on.load(Ordering::SeqCst) {
            self.read.notify_one();
            self.release.notified().await;
        }
        Ok(receipt)
    }
}
struct EmptyCredentials;
#[async_trait]
impl CredentialProvider for EmptyCredentials {
    async fn resolve(
        &self,
        _: &InvocationContext,
        _: &Scope,
        _: Uuid,
    ) -> Result<DriverLogin, WorkbenchError> {
        Ok(DriverLogin {
            username: String::new(),
            secret: zeroize::Zeroizing::new(String::new()),
        })
    }
}
struct DiscardRows;
#[async_trait]
impl RowSink for DiscardRows {
    async fn columns(&mut self, _: Vec<Column>) -> Result<(), WorkbenchError> {
        Ok(())
    }
    async fn row(&mut self, _: Vec<TypedCell>) -> Result<(), WorkbenchError> {
        Ok(())
    }
}
fn start_external_activity(catalog: &std::path::Path, target: &std::path::Path) {
    rusqlite::Connection::open(target)
        .unwrap()
        .execute("UPDATE source SET id=id+10", [])
        .unwrap();
    restoreplane_catalog::rusqlite::Connection::open(catalog).unwrap().execute("INSERT INTO operation_runs(id,operation_kind,subject_type,subject_id,request_json,state,created_at_ms,updated_at_ms) VALUES('race-maintenance','control_store_restore','service','service','{}','running',200,200)",[]).unwrap();
}
fn clear_activity(catalog: &std::path::Path) {
    restoreplane_catalog::rusqlite::Connection::open(catalog)
        .unwrap()
        .execute("DELETE FROM operation_runs WHERE id='race-maintenance'", [])
        .unwrap();
}

#[tokio::test]
async fn activity_after_authority_read_is_observed_on_next_dispatch_without_claiming_a_mutex() {
    let source = support::Fixture::sqlite_mode(
        "CREATE TABLE source(id INTEGER PRIMARY KEY);INSERT INTO source VALUES(1)",
        Mode::ReadWrite,
    )
    .await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("catalog.db");
    let store = restoreplane_catalog::OperationalStore::open(path.clone()).unwrap();
    let scope = Scope {
        workspace_id: restoreplane_catalog::DEFAULT_WORKSPACE_ID
            .parse::<WorkspaceId>()
            .unwrap(),
        connection_profile_id: ConnectionProfileId::new(),
        database_target_id: DatabaseTargetId::new(),
    };
    store
        .configuration()
        .insert_connection(restoreplane_catalog::ConnectionProfileSpec::file(
            scope.connection_profile_id.clone(),
            "race source",
            "sqlite",
            source.path.to_string_lossy(),
            100,
        ))
        .unwrap();
    let id = restoreplane_source_sqlite::probe_sqlite_file(&source.path)
        .unwrap()
        .object_id()
        .to_owned();
    store
        .configuration()
        .insert_target(restoreplane_catalog::DatabaseTargetSpec::new(
            scope.database_target_id.clone(),
            scope.connection_profile_id.clone(),
            format!("file:{id}"),
            "source",
            "source",
            "native-race-fixture",
            110,
        ))
        .unwrap();
    let authority = Arc::new(AuthorityRace {
        store,
        epoch: Uuid::new_v4().to_string(),
        calls: AtomicUsize::new(0),
        pause_on: AtomicUsize::new(0),
        read: Notify::new(),
        release: Notify::new(),
    });
    // Native principal is an explicit test seam; catalog snapshots, target
    // writes and connection execution are real. No OS/service auth E2E claim.
    let principal = NativePrincipal::new("a".repeat(64), 7, IntegrityLevel::Standard).unwrap();
    let context = InvocationContext::new(principal.clone(), Uuid::new_v4());
    let state = Arc::new(
        WorkbenchState::new(
            principal,
            authority.clone(),
            Arc::new(EmptyCredentials),
            Arc::new(NativeDriverFactory::default()),
            RuntimeLimits::default(),
        )
        .unwrap(),
    );
    let mut session = state
        .open(
            &context,
            scope.clone(),
            Uuid::new_v4(),
            None,
            Mode::ReadWrite,
        )
        .await
        .unwrap()
        .session;
    for pause_on in [1, 2] {
        authority.calls.store(0, Ordering::SeqCst);
        authority.pause_on.store(pause_on, Ordering::SeqCst);
        let calls = Arc::new(AtomicUsize::new(0));
        let callback = calls.clone();
        let runtime = state.clone();
        let invocation = context.clone();
        let work = tokio::spawn(async move {
            runtime
                .with_connection(
                    &invocation,
                    session,
                    AuthorizePurpose::Mutate,
                    move |connection| {
                        Box::pin(async move {
                            callback.fetch_add(1, Ordering::SeqCst);
                            connection
                                .execute(
                                    "UPDATE source SET id=id+1",
                                    QueryLimits::default(),
                                    &mut DiscardRows,
                                )
                                .await?;
                            Ok(())
                        })
                    },
                )
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(3), authority.read.notified())
            .await
            .unwrap();
        start_external_activity(&path, &source.path);
        authority.release.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), work)
            .await
            .unwrap()
            .unwrap();
        if pause_on == 1 {
            assert!(matches!(result, Err(WorkbenchError::TargetBusy)));
            assert_eq!(
                calls.load(Ordering::SeqCst),
                0,
                "fresh read after the lock must observe the conflict"
            );
        } else {
            result.unwrap();
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "activity after the final snapshot is outside an atomic mutex"
            );
            let refused_callback = calls.clone();
            let refused = state
                .with_connection(&context, session, AuthorizePurpose::Mutate, move |_| {
                    Box::pin(async move {
                        refused_callback.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    })
                })
                .await;
            assert!(matches!(refused, Err(WorkbenchError::TargetBusy)));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
        let actual: i64 = rusqlite::Connection::open(&source.path)
            .unwrap()
            .query_row("SELECT id FROM source", [], |row| row.get(0))
            .unwrap();
        assert_eq!(actual, if pause_on == 1 { 11 } else { 22 });
        clear_activity(&path);
        authority.pause_on.store(0, Ordering::SeqCst);
        session = state
            .reconnect(&context, session, &"b".repeat(64))
            .await
            .unwrap()
            .session;
    }
    state.close_window(&context).await.unwrap();
}
