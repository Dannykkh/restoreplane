use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use restoreplane_domain::workbench::{
    ActualContext, AuthorizePurpose, BindingReceipt, ConnectDescriptor, DatabaseIdentity, Engine,
    Mode, Route, Scope, SessionRef, TargetPurpose, Tls,
};
use restoreplane_domain::{ConnectionProfileId, DatabaseTargetId, WorkspaceId};
use restoreplane_workbench::credentials::{CredentialProvider, LocalCredentials};
use restoreplane_workbench::driver::{
    DriverCancel, DriverConnection, DriverFactory, DriverFacts, DriverLogin, DriverSummary,
    QueryLimits, RowSink,
};
use restoreplane_workbench::session::RuntimeLimits;
use restoreplane_workbench::store::{ClientStore, UserRootPolicy};
use restoreplane_workbench::{
    BindingAuthorizer, IntegrityLevel, InvocationContext, NativePrincipal, WorkbenchError,
    WorkbenchState,
};
use uuid::Uuid;
use zeroize::Zeroizing;

fn principal(owner: char, integrity: IntegrityLevel) -> NativePrincipal {
    NativePrincipal::new(owner.to_string().repeat(64), 7, integrity).unwrap()
}
fn context() -> InvocationContext {
    InvocationContext::new(principal('a', IntegrityLevel::Standard), Uuid::new_v4())
}
fn scope() -> Scope {
    Scope {
        workspace_id: WorkspaceId::new(),
        connection_profile_id: ConnectionProfileId::new(),
        database_target_id: DatabaseTargetId::new(),
    }
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

struct Authorizer {
    fail: AtomicBool,
    revision: Mutex<String>,
    calls: AtomicUsize,
}
impl Default for Authorizer {
    fn default() -> Self {
        Self {
            fail: AtomicBool::new(false),
            revision: Mutex::new("b".repeat(64)),
            calls: AtomicUsize::new(0),
        }
    }
}
#[async_trait]
impl BindingAuthorizer for Authorizer {
    async fn authorize(
        &self,
        _: &InvocationContext,
        scope: &Scope,
        _: AuthorizePurpose,
        expected: Option<&str>,
    ) -> Result<BindingReceipt, WorkbenchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            return Err(WorkbenchError::AuthorizationExpired);
        }
        let revision = self.revision.lock().unwrap().clone();
        if expected.is_some_and(|value| value != revision) {
            return Err(WorkbenchError::BindingStale);
        }
        Ok(BindingReceipt {
            scope: scope.clone(),
            descriptor: ConnectDescriptor {
                engine: Engine::Postgresql,
                route: Route::Tcp {
                    host: "example.invalid".into(),
                    port: 5432,
                },
                database: DatabaseIdentity {
                    engine_object_id: "database".into(),
                    name: "fixture".into(),
                    source_fingerprint: "fingerprint".into(),
                },
                tls: Tls {
                    mode: "verify-full".into(),
                    server_name: None,
                    root_trust: None,
                },
                transport_options: BTreeMap::new(),
                expected_server_identity: None,
            },
            descriptor_revision: revision,
            authority_epoch: "service-boot".into(),
            authorized_at_ms: now().into(),
            read_lease_expires_at_ms: Some((now() + 30_000).into()),
            observed_conflicts: Vec::new(),
        })
    }
}

#[derive(Default)]
struct Credentials {
    calls: AtomicUsize,
}
#[async_trait]
impl CredentialProvider for Credentials {
    async fn resolve(
        &self,
        _: &InvocationContext,
        _: &Scope,
        _: Uuid,
    ) -> Result<DriverLogin, WorkbenchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(DriverLogin {
            username: "interactive-only".into(),
            secret: Zeroizing::new("user-secret".into()),
        })
    }
}

struct Cancel;
#[async_trait]
impl DriverCancel for Cancel {
    async fn request(&self) -> Result<(), WorkbenchError> {
        Ok(())
    }
}
struct Connection {
    facts: DriverFacts,
}
#[async_trait]
impl DriverConnection for Connection {
    fn facts(&self) -> &DriverFacts {
        &self.facts
    }
    fn cancel_handle(&self) -> Arc<dyn DriverCancel> {
        Arc::new(Cancel)
    }
    async fn set_read_only(&mut self, value: bool) -> Result<(), WorkbenchError> {
        self.facts.read_only_held = value;
        Ok(())
    }
    async fn execute(
        &mut self,
        _: &str,
        _: QueryLimits,
        _: &mut dyn RowSink,
    ) -> Result<DriverSummary, WorkbenchError> {
        Ok(DriverSummary::default())
    }
    async fn close(&mut self) -> Result<(), WorkbenchError> {
        Ok(())
    }
}
#[derive(Default)]
struct Factory {
    calls: AtomicUsize,
    credentials_seen: Mutex<Vec<String>>,
    delay: AtomicBool,
    started: tokio::sync::Notify,
    hold: AtomicBool,
    release: tokio::sync::Notify,
}
#[async_trait]
impl DriverFactory for Factory {
    async fn connect(
        &self,
        _: &ConnectDescriptor,
        login: DriverLogin,
        mode: Mode,
    ) -> Result<Box<dyn DriverConnection>, WorkbenchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.credentials_seen.lock().unwrap().push(login.username);
        self.started.notify_one();
        if self.hold.load(Ordering::SeqCst) {
            self.release.notified().await;
        }
        if self.delay.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(60)).await;
        }
        Ok(Box::new(Connection {
            facts: DriverFacts {
                context: ActualContext {
                    endpoint: "example.invalid:5432".into(),
                    database: "fixture".into(),
                    role: "interactive-only".into(),
                    purpose: TargetPurpose::Source,
                },
                engine: Engine::Postgresql,
                server_version: "fixture".into(),
                server_identity: None,
                actual_database_identity: None,
                read_only_held: mode == Mode::ReadOnly,
                verified_tls: true,
            },
        }))
    }
}

fn state(
    authorizer: Arc<Authorizer>,
    credentials: Arc<Credentials>,
    factory: Arc<Factory>,
) -> WorkbenchState {
    WorkbenchState::new(
        principal('a', IntegrityLevel::Standard),
        authorizer,
        credentials,
        factory,
        RuntimeLimits::default(),
    )
    .unwrap()
}

#[tokio::test]
async fn separate_credentials_and_uncertified_capabilities_remain_explicit() {
    let authorizer = Arc::new(Authorizer::default());
    let credentials = Arc::new(Credentials::default());
    let factory = Arc::new(Factory::default());
    let state = state(authorizer.clone(), credentials.clone(), factory.clone());
    let session = state
        .open(&context(), scope(), Uuid::new_v4(), None, Mode::ReadOnly)
        .await
        .unwrap();
    assert_eq!(authorizer.calls.load(Ordering::SeqCst), 2);
    assert_eq!(credentials.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        factory.credentials_seen.lock().unwrap().as_slice(),
        ["interactive-only"]
    );
    assert!(!session.capabilities["verified_editing"].supported);
    assert!(!session.capabilities["read_only_lease"].supported);
}

#[tokio::test]
async fn foreign_owner_window_and_old_generation_are_rejected_before_callback() {
    let state = state(
        Arc::new(Authorizer::default()),
        Arc::new(Credentials::default()),
        Arc::new(Factory::default()),
    );
    let context = context();
    let session = state
        .open(&context, scope(), Uuid::new_v4(), None, Mode::ReadOnly)
        .await
        .unwrap();
    let foreign_owner = InvocationContext::new(
        principal('f', IntegrityLevel::Standard),
        context.window_instance_id(),
    );
    assert_eq!(
        state.get(&foreign_owner, session.session).unwrap_err(),
        WorkbenchError::Forbidden
    );
    let foreign_window = InvocationContext::new(context.principal().clone(), Uuid::new_v4());
    assert_eq!(
        state.get(&foreign_window, session.session).unwrap_err(),
        WorkbenchError::SessionStale
    );
    let old = SessionRef {
        connection_generation: (session.session.connection_generation.value() + 1).into(),
        ..session.session
    };
    assert_eq!(
        state.get(&context, old).unwrap_err(),
        WorkbenchError::SessionStale
    );
}

#[tokio::test]
async fn revision_expiry_and_readonly_mode_fail_before_mutating_callback() {
    let authorizer = Arc::new(Authorizer::default());
    let state = state(
        authorizer.clone(),
        Arc::new(Credentials::default()),
        Arc::new(Factory::default()),
    );
    let context = context();
    let session = state
        .open(&context, scope(), Uuid::new_v4(), None, Mode::ReadOnly)
        .await
        .unwrap();
    let denied = state
        .with_connection(&context, session.session, AuthorizePurpose::Mutate, |_| {
            Box::pin(async { panic!("must not dispatch") })
        })
        .await;
    assert_eq!(denied, Err::<(), _>(WorkbenchError::Forbidden));
    authorizer.fail.store(true, Ordering::SeqCst);
    let expired = state
        .with_connection(&context, session.session, AuthorizePurpose::Read, |_| {
            Box::pin(async { panic!("must not dispatch") })
        })
        .await;
    assert_eq!(expired, Err::<(), _>(WorkbenchError::AuthorizationExpired));
    assert_eq!(
        state.get(&context, session.session).unwrap().state,
        restoreplane_domain::workbench::SessionState::Retiring
    );
    authorizer.fail.store(false, Ordering::SeqCst);
    *authorizer.revision.lock().unwrap() = "c".repeat(64);
    assert!(state
        .with_connection(&context, session.session, AuthorizePurpose::Read, |_| {
            Box::pin(async { Ok(()) })
        })
        .await
        .is_err());
    state.close(&context, session.session).await.unwrap();
}

#[tokio::test]
async fn reconnect_closes_prior_generation_and_capacity_is_bounded() {
    let state = state(
        Arc::new(Authorizer::default()),
        Arc::new(Credentials::default()),
        Arc::new(Factory::default()),
    );
    let context = context();
    let scope = scope();
    let first = state
        .open(
            &context,
            scope.clone(),
            Uuid::new_v4(),
            None,
            Mode::ReadOnly,
        )
        .await
        .unwrap();
    let second = state
        .open(
            &context,
            scope.clone(),
            Uuid::new_v4(),
            None,
            Mode::ReadOnly,
        )
        .await
        .unwrap();
    assert_eq!(
        state
            .open(
                &context,
                scope.clone(),
                Uuid::new_v4(),
                None,
                Mode::ReadOnly
            )
            .await
            .unwrap_err(),
        WorkbenchError::Limit
    );
    let replacement = state
        .reconnect(&context, first.session, &"b".repeat(64))
        .await
        .unwrap();
    assert!(replacement.session.connection_generation > first.session.connection_generation);
    assert_eq!(
        state.get(&context, first.session).unwrap_err(),
        WorkbenchError::SessionStale
    );
    state.close(&context, second.session).await.unwrap();
    state.close_window(&context).await.unwrap();
    assert!(state.list(&context, &scope).unwrap().is_empty());
    assert_eq!(
        state
            .open(&context, scope, Uuid::new_v4(), None, Mode::ReadOnly)
            .await
            .unwrap_err(),
        WorkbenchError::SessionStale
    );
}

#[tokio::test]
async fn global_session_and_execution_permits_remain_bounded_and_reusable() {
    let state = Arc::new(state(
        Arc::new(Authorizer::default()),
        Arc::new(Credentials::default()),
        Arc::new(Factory::default()),
    ));
    let context = context();
    let mut sessions = Vec::new();
    for _ in 0..4 {
        let target = scope();
        for _ in 0..2 {
            sessions.push(
                state
                    .open(
                        &context,
                        target.clone(),
                        Uuid::new_v4(),
                        None,
                        Mode::ReadOnly,
                    )
                    .await
                    .unwrap(),
            );
        }
    }
    assert!(matches!(
        state
            .open(&context, scope(), Uuid::new_v4(), None, Mode::ReadOnly)
            .await,
        Err(WorkbenchError::Limit)
    ));
    let entered = Arc::new(tokio::sync::Barrier::new(5));
    let release = Arc::new(tokio::sync::Notify::new());
    let mut running = Vec::new();
    for session in sessions.iter().take(4) {
        let state = state.clone();
        let context = context.clone();
        let reference = session.session;
        let entered = entered.clone();
        let release = release.clone();
        running.push(tokio::spawn(async move {
            state
                .with_connection(&context, reference, AuthorizePurpose::Read, move |_| {
                    Box::pin(async move {
                        let released = release.notified();
                        tokio::pin!(released);
                        released.as_mut().enable();
                        entered.wait().await;
                        released.await;
                        Ok(())
                    })
                })
                .await
        }));
    }
    tokio::time::timeout(Duration::from_secs(2), entered.wait())
        .await
        .unwrap();
    let rejected = state
        .with_connection(
            &context,
            sessions[4].session,
            AuthorizePurpose::Read,
            |_| Box::pin(async { panic!("fifth callback cannot run") }),
        )
        .await;
    assert_eq!(rejected, Err::<(), _>(WorkbenchError::Limit));
    release.notify_waiters();
    for task in running {
        task.await.unwrap().unwrap();
    }
    state
        .with_connection(
            &context,
            sessions[4].session,
            AuthorizePurpose::Read,
            |_| Box::pin(async { Ok(()) }),
        )
        .await
        .unwrap();
    for session in sessions {
        state.close(&context, session.session).await.unwrap();
    }
    state
        .open(&context, scope(), Uuid::new_v4(), None, Mode::ReadOnly)
        .await
        .unwrap();
}

#[tokio::test]
async fn connection_wait_keeps_executor_responsive_and_closed_window_cannot_publish() {
    let factory = Arc::new(Factory::default());
    factory.delay.store(true, Ordering::SeqCst);
    let state = Arc::new(state(
        Arc::new(Authorizer::default()),
        Arc::new(Credentials::default()),
        factory.clone(),
    ));
    let context = context();
    let task_context = context.clone();
    let task_state = state.clone();
    let task = tokio::spawn(async move {
        task_state
            .open(&task_context, scope(), Uuid::new_v4(), None, Mode::ReadOnly)
            .await
    });
    factory.started.notified().await;
    tokio::time::timeout(
        Duration::from_millis(30),
        tokio::time::sleep(Duration::from_millis(1)),
    )
    .await
    .unwrap();
    state.close_window(&context).await.unwrap();
    assert_eq!(
        task.await.unwrap().unwrap_err(),
        WorkbenchError::SessionStale
    );
}

#[tokio::test]
async fn credential_revocation_fences_a_login_already_waiting_in_the_driver() {
    let factory = Arc::new(Factory::default());
    factory.hold.store(true, Ordering::SeqCst);
    let state = Arc::new(state(
        Arc::new(Authorizer::default()),
        Arc::new(Credentials::default()),
        factory.clone(),
    ));
    let context = context();
    let credential = Uuid::new_v4();
    let target = scope();
    let task_state = state.clone();
    let task_context = context.clone();
    let task_target = target.clone();
    let pending = tokio::spawn(async move {
        task_state
            .open(&task_context, task_target, credential, None, Mode::ReadOnly)
            .await
    });
    factory.started.notified().await;
    state
        .invalidate_credential(&context, credential)
        .await
        .unwrap();
    factory.release.notify_one();
    assert_eq!(
        pending.await.unwrap().unwrap_err(),
        WorkbenchError::SessionStale
    );
    assert!(state.list(&context, &target).unwrap().is_empty());
    factory.hold.store(false, Ordering::SeqCst);
    state
        .open(&context, target, credential, None, Mode::ReadOnly)
        .await
        .unwrap();
}

#[tokio::test]
async fn physical_lease_excludes_ordinary_work_and_cleanup_survives_authority_expiry() {
    use restoreplane_workbench::session::ConnectionLeaseKind;
    let authorizer = Arc::new(Authorizer::default());
    let state = state(
        authorizer.clone(),
        Arc::new(Credentials::default()),
        Arc::new(Factory::default()),
    );
    let context = context();
    let session = state
        .open(&context, scope(), Uuid::new_v4(), None, Mode::ReadWrite)
        .await
        .unwrap();
    let lease = state
        .pin_connection(
            &context,
            session.session,
            ConnectionLeaseKind::Snapshot,
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    let ordinary = state
        .with_connection(&context, session.session, AuthorizePurpose::Read, |_| {
            Box::pin(async { panic!("pinned socket cannot be borrowed") })
        })
        .await;
    assert_eq!(ordinary, Err::<(), _>(WorkbenchError::TargetBusy));
    state
        .with_lease(&context, &lease, AuthorizePurpose::Read, |_| {
            Box::pin(async { Ok(()) })
        })
        .await
        .unwrap();
    let mutate = state
        .with_lease(&context, &lease, AuthorizePurpose::Mutate, |_| {
            Box::pin(async { panic!("readonly snapshot cannot mutate") })
        })
        .await;
    assert_eq!(mutate, Err::<(), _>(WorkbenchError::Forbidden));
    authorizer.fail.store(true, Ordering::SeqCst);
    state
        .with_lease_cleanup(&context, &lease, |_| Box::pin(async { Ok(()) }))
        .await
        .unwrap();
    assert!(state
        .with_lease(&context, &lease, AuthorizePurpose::Read, |_| Box::pin(
            async { Ok(()) }
        ))
        .await
        .is_err());
    state.finish_lease(&context, &lease, true).await.unwrap();
    authorizer.fail.store(false, Ordering::SeqCst);
    state
        .with_connection(&context, session.session, AuthorizePurpose::Read, |_| {
            Box::pin(async { Ok(()) })
        })
        .await
        .unwrap();
}

struct RootPolicy {
    calls: AtomicUsize,
}
impl UserRootPolicy for RootPolicy {
    fn ensure_private_root(&self, path: &std::path::Path) -> Result<(), WorkbenchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        std::fs::create_dir_all(path).map_err(|_| WorkbenchError::Io)
    }
}

#[test]
fn elevated_refusal_happens_before_store_policy_files_or_dpapi() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("never-created");
    let principal = principal('a', IntegrityLevel::Elevated);
    let policy = RootPolicy {
        calls: AtomicUsize::new(0),
    };
    assert!(matches!(
        ClientStore::open(&root, &principal, &policy),
        Err(WorkbenchError::Elevated)
    ));
    assert!(matches!(
        LocalCredentials::open(&root, &principal, &policy),
        Err(WorkbenchError::Elevated)
    ));
    assert!(!root.exists());
    assert_eq!(policy.calls.load(Ordering::SeqCst), 0);
    assert!(matches!(
        WorkbenchState::new(
            principal,
            Arc::new(Authorizer::default()),
            Arc::new(Credentials::default()),
            Arc::new(Factory::default()),
            RuntimeLimits::default()
        ),
        Err(WorkbenchError::Elevated)
    ));
}

#[test]
fn client_store_enforces_single_process_owner_and_does_not_recreate_invalid_state() {
    let directory = tempfile::tempdir().unwrap();
    let policy = RootPolicy {
        calls: AtomicUsize::new(0),
    };
    let owner = principal('a', IntegrityLevel::Standard);
    let store = ClientStore::open(directory.path(), &owner, &policy).unwrap();
    assert!(ClientStore::open(directory.path(), &owner, &policy).is_err());
    let original = store.journal_epoch().to_owned();
    drop(store);
    assert!(ClientStore::open(
        directory.path(),
        &principal('f', IntegrityLevel::Standard),
        &policy
    )
    .is_err());
    let reopened = ClientStore::open(directory.path(), &owner, &policy).unwrap();
    assert_eq!(reopened.journal_epoch(), original);
    drop(reopened);
    let connection =
        rusqlite::Connection::open(directory.path().join("client-state.sqlite3")).unwrap();
    connection.pragma_update(None, "user_version", 999).unwrap();
    drop(connection);
    assert!(ClientStore::open(directory.path(), &owner, &policy).is_err());
    let connection =
        rusqlite::Connection::open(directory.path().join("client-state.sqlite3")).unwrap();
    assert_eq!(
        connection
            .pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
            .unwrap(),
        999
    );
}

#[test]
fn experimental_v1_with_missing_transaction_links_stays_write_disabled() {
    let directory = tempfile::tempdir().unwrap();
    let policy = RootPolicy {
        calls: AtomicUsize::new(0),
    };
    let owner = principal('a', IntegrityLevel::Standard);
    drop(ClientStore::open(directory.path(), &owner, &policy).unwrap());
    let path = directory.path().join("client-state.sqlite3");
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection.execute_batch("ALTER TABLE statements RENAME COLUMN transaction_operation_id TO experimental_transaction_id").unwrap();
    drop(connection);
    assert!(matches!(
        ClientStore::open(directory.path(), &owner, &policy),
        Err(WorkbenchError::StoreUnavailable)
    ));
    let connection = rusqlite::Connection::open(&path).unwrap();
    assert!(connection
        .prepare("SELECT experimental_transaction_id FROM statements")
        .is_ok());
    assert_eq!(
        connection
            .pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
            .unwrap(),
        1
    );
}

#[cfg(windows)]
#[tokio::test]
async fn user_credentials_are_encrypted_scope_bound_and_never_plaintext_files() {
    let directory = tempfile::tempdir().unwrap();
    let policy = RootPolicy {
        calls: AtomicUsize::new(0),
    };
    let context = context();
    let scope = scope();
    let credentials =
        LocalCredentials::open(directory.path(), context.principal(), &policy).unwrap();
    let secret = "workbench-credential-file-canary-4021";
    let reference = credentials
        .put(
            &context,
            scope.clone(),
            None,
            "fixture".into(),
            "user@tenant".into(),
            Zeroizing::new(secret.into()),
        )
        .unwrap();
    let bytes = std::fs::read(directory.path().join(format!("{reference}.credential"))).unwrap();
    assert!(!bytes
        .windows(secret.len())
        .any(|window| window == secret.as_bytes()));
    let login = credentials
        .resolve(&context, &scope, reference)
        .await
        .unwrap();
    assert_eq!(login.username, "user@tenant");
    assert_eq!(login.secret.as_str(), secret);
    let other_scope = Scope {
        workspace_id: WorkspaceId::new(),
        ..scope.clone()
    };
    assert!(credentials
        .resolve(&context, &other_scope, reference)
        .await
        .is_err());
    let blank = credentials
        .put(
            &context,
            scope.clone(),
            None,
            "blank".into(),
            "user@tenant".into(),
            Zeroizing::new(String::new()),
        )
        .unwrap();
    assert!(credentials
        .resolve(&context, &scope, blank)
        .await
        .unwrap()
        .secret
        .is_empty());
    assert!(credentials.delete(&context, reference).unwrap());
    assert!(!credentials.delete(&context, reference).unwrap());
}
