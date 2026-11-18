#![allow(dead_code)]
use async_trait::async_trait;
use restoreplane_domain::workbench::{
    AuthorizePurpose, BindingReceipt, ConnectDescriptor, DatabaseIdentity, Engine, Mode, Route,
    Scope, SessionRef, Tls,
};
use restoreplane_domain::{ConnectionProfileId, DatabaseTargetId, WorkspaceId};
use restoreplane_workbench::credentials::CredentialProvider;
use restoreplane_workbench::driver::{DriverFactory, DriverLogin, NativeDriverFactory};
use restoreplane_workbench::session::RuntimeLimits;
use restoreplane_workbench::{
    BindingAuthorizer, IntegrityLevel, InvocationContext, NativePrincipal, WorkbenchError,
    WorkbenchState,
};
use std::collections::BTreeMap;
use std::sync::Arc;
use uuid::Uuid;
use zeroize::Zeroizing;

struct Authority {
    descriptor: ConnectDescriptor,
    scope: Scope,
    epoch: Arc<std::sync::Mutex<String>>,
    available: Arc<std::sync::atomic::AtomicBool>,
    rotate_after_read: Arc<std::sync::atomic::AtomicBool>,
}
#[async_trait]
impl BindingAuthorizer for Authority {
    async fn authorize(
        &self,
        _: &InvocationContext,
        scope: &Scope,
        _: AuthorizePurpose,
        revision: Option<&str>,
    ) -> Result<BindingReceipt, WorkbenchError> {
        if !self.available.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(WorkbenchError::ServiceUnavailable);
        }
        if scope != &self.scope || revision.is_some_and(|value| value != "b".repeat(64)) {
            return Err(WorkbenchError::BindingStale);
        }
        let receipt = BindingReceipt {
            scope: scope.clone(),
            descriptor: self.descriptor.clone(),
            descriptor_revision: "b".repeat(64),
            authority_epoch: self.epoch.lock().map_err(|_| WorkbenchError::Io)?.clone(),
            authorized_at_ms: restoreplane_workbench::types::now_ms().into(),
            read_lease_expires_at_ms: None,
            observed_conflicts: Vec::new(),
        };
        if self
            .rotate_after_read
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            *self.epoch.lock().map_err(|_| WorkbenchError::Io)? = Uuid::new_v4().to_string();
        }
        Ok(receipt)
    }
}
struct Credentials {
    username: String,
    secret: Zeroizing<String>,
}
#[async_trait]
impl CredentialProvider for Credentials {
    async fn resolve(
        &self,
        _: &InvocationContext,
        _: &Scope,
        _: Uuid,
    ) -> Result<DriverLogin, WorkbenchError> {
        Ok(DriverLogin {
            username: self.username.clone(),
            secret: self.secret.clone(),
        })
    }
}
pub struct Fixture {
    pub runtime: Arc<WorkbenchState>,
    pub context: InvocationContext,
    pub session: SessionRef,
    pub scope: Scope,
    pub path: std::path::PathBuf,
    pub authority_available: Arc<std::sync::atomic::AtomicBool>,
    pub authority_epoch: Arc<std::sync::Mutex<String>>,
    pub rotate_authority_after_read: Arc<std::sync::atomic::AtomicBool>,
    _directory: Option<tempfile::TempDir>,
}
impl Fixture {
    pub async fn sqlite(setup: &str) -> Self {
        Self::sqlite_mode(setup, Mode::ReadOnly).await
    }
    pub async fn sqlite_mode(setup: &str, mode: Mode) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.sqlite3");
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection.execute_batch(setup).unwrap();
        drop(connection);
        let object_id = restoreplane_source_sqlite::probe_sqlite_file(&path)
            .unwrap()
            .object_id()
            .to_owned();
        let descriptor = ConnectDescriptor {
            engine: Engine::Sqlite,
            route: Route::File {
                canonical_path: path.to_string_lossy().into_owned(),
            },
            database: DatabaseIdentity {
                engine_object_id: format!("file:{object_id}"),
                name: "fixture".into(),
                source_fingerprint: "fixture".into(),
            },
            tls: Tls {
                mode: "disable".into(),
                server_name: None,
                root_trust: None,
            },
            transport_options: BTreeMap::new(),
            expected_server_identity: None,
        };
        let mut fixture = Self::connect(
            descriptor,
            String::new(),
            Zeroizing::new(String::new()),
            mode,
        )
        .await;
        fixture.path = path;
        fixture._directory = Some(directory);
        fixture
    }
    pub async fn connect(
        descriptor: ConnectDescriptor,
        username: String,
        secret: Zeroizing<String>,
        mode: Mode,
    ) -> Self {
        Self::connect_with_factory(
            descriptor,
            username,
            secret,
            mode,
            Arc::new(NativeDriverFactory::default()),
        )
        .await
    }
    pub async fn connect_with_factory(
        descriptor: ConnectDescriptor,
        username: String,
        secret: Zeroizing<String>,
        mode: Mode,
        factory: Arc<dyn DriverFactory>,
    ) -> Self {
        let scope = Scope {
            workspace_id: WorkspaceId::new(),
            connection_profile_id: ConnectionProfileId::new(),
            database_target_id: DatabaseTargetId::new(),
        };
        // Explicit native seam: this proves runtime behavior, not a real Windows
        // standard-integrity Tauri invocation or service transport authorization.
        let principal = NativePrincipal::new("a".repeat(64), 7, IntegrityLevel::Standard).unwrap();
        let context = InvocationContext::new(principal.clone(), Uuid::new_v4());
        let available = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let authority_epoch = Arc::new(std::sync::Mutex::new(Uuid::new_v4().to_string()));
        let rotate_after_read = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let runtime = Arc::new(
            WorkbenchState::new(
                principal,
                Arc::new(Authority {
                    descriptor,
                    scope: scope.clone(),
                    epoch: authority_epoch.clone(),
                    available: available.clone(),
                    rotate_after_read: rotate_after_read.clone(),
                }),
                Arc::new(Credentials { username, secret }),
                factory,
                RuntimeLimits::default(),
            )
            .unwrap(),
        );
        let session = runtime
            .open(&context, scope.clone(), Uuid::new_v4(), None, mode)
            .await
            .unwrap()
            .session;
        Self {
            runtime,
            context,
            session,
            scope,
            path: std::path::PathBuf::new(),
            authority_available: available,
            authority_epoch,
            rotate_authority_after_read: rotate_after_read,
            _directory: None,
        }
    }
}

struct QueryRoot;
impl restoreplane_workbench::store::UserRootPolicy for QueryRoot {
    fn ensure_private_root(&self, root: &std::path::Path) -> Result<(), WorkbenchError> {
        std::fs::create_dir_all(root).map_err(|_| WorkbenchError::Io)
    }
}
pub struct QueryFixture {
    pub database: Fixture,
    pub coordinator: Arc<restoreplane_workbench::query::QueryCoordinator>,
    pub streams: Arc<restoreplane_workbench::result_stream::ResultStreams>,
    pub journal: Arc<restoreplane_workbench::journal::OperationJournal>,
    pub store: Arc<restoreplane_workbench::store::ClientStore>,
    pub journal_root: tempfile::TempDir,
}
impl QueryFixture {
    pub fn new(database: Fixture) -> Self {
        let journal_root = tempfile::tempdir().unwrap();
        let store = Arc::new(
            restoreplane_workbench::store::ClientStore::open(
                journal_root.path(),
                database.context.principal(),
                &QueryRoot,
            )
            .unwrap(),
        );
        let journal = Arc::new(
            restoreplane_workbench::journal::OperationJournal::open(store.clone()).unwrap(),
        );
        let streams = Arc::new(
            restoreplane_workbench::result_stream::ResultStreams::new_pull(
                database.runtime.clone(),
            )
            .unwrap(),
        );
        let coordinator = Arc::new(restoreplane_workbench::query::QueryCoordinator::new(
            database.runtime.clone(),
            journal.clone(),
            streams.clone(),
        ));
        Self {
            database,
            coordinator,
            streams,
            journal,
            store,
            journal_root,
        }
    }
}
