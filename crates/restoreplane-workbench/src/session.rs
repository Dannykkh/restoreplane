use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures::future::BoxFuture;
use restoreplane_domain::workbench::{
    AuthorizePurpose, BindingReceipt, Capability, Engine, Mode, Scope, Session, SessionRef,
    SessionState, TargetPurpose,
};
use restoreplane_domain::workbench_protection::WorkbenchRestoreProof;
use restoreplane_domain::WorkspaceId;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

use crate::credentials::CredentialProvider;
use crate::driver::{
    CertifiedCapabilities, DriverCancel, DriverConnection, DriverFactory, DriverFacts,
};
use crate::store::{ClientStore, UserRootPolicy};
use crate::WorkbenchError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntegrityLevel {
    Standard,
    Elevated,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativePrincipal {
    owner_id: String,
    logon_session_id: u32,
    integrity: IntegrityLevel,
}

impl NativePrincipal {
    /// Called by native token inspection, never deserialized from request JSON.
    pub fn new(
        owner_id: String,
        logon_session_id: u32,
        integrity: IntegrityLevel,
    ) -> Result<Self, WorkbenchError> {
        if owner_id.len() != 64
            || !owner_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(WorkbenchError::InvalidInput);
        }
        Ok(Self {
            owner_id,
            logon_session_id,
            integrity,
        })
    }
    pub fn owner_id(&self) -> &str {
        &self.owner_id
    }
    pub fn logon_session_id(&self) -> u32 {
        self.logon_session_id
    }
    pub fn ensure_standard(&self) -> Result<(), WorkbenchError> {
        if self.integrity == IntegrityLevel::Elevated {
            Err(WorkbenchError::Elevated)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Debug)]
pub struct InvocationContext {
    principal: NativePrincipal,
    window_instance_id: Uuid,
}

impl InvocationContext {
    pub fn new(principal: NativePrincipal, window_instance_id: Uuid) -> Self {
        Self {
            principal,
            window_instance_id,
        }
    }
    pub fn principal(&self) -> &NativePrincipal {
        &self.principal
    }
    pub fn window_instance_id(&self) -> Uuid {
        self.window_instance_id
    }
}

/// Native-only authority constructed after authenticated work.get has verified
/// owner, workspace, completed restore and provenance. Never deserialize this
/// capability from the webview or infer it from a connection's display name.
#[derive(Clone)]
pub struct RestoredSessionCheck {
    principal: NativePrincipal,
    workspace_id: WorkspaceId,
    proof: WorkbenchRestoreProof,
}
/// Native reconnect basis remains available after authorization retires a
/// metadata-only connection. It is never supplied by the webview.
pub(crate) struct ReconnectBasis {
    pub(crate) scope: Scope,
    pub(crate) credential: Uuid,
    pub(crate) mode: Mode,
    pub(crate) restored_check: Option<RestoredSessionCheck>,
}
impl RestoredSessionCheck {
    pub fn from_authenticated_work(
        principal: NativePrincipal,
        workspace_id: WorkspaceId,
        proof: WorkbenchRestoreProof,
    ) -> Result<Self, WorkbenchError> {
        principal.ensure_standard()?;
        if !proof.is_well_formed() {
            return Err(WorkbenchError::BindingUnproven);
        }
        Ok(Self {
            principal,
            workspace_id,
            proof,
        })
    }
    fn validate(
        &self,
        context: &InvocationContext,
        receipt: &BindingReceipt,
        facts: &DriverFacts,
    ) -> Result<(), WorkbenchError> {
        if self.principal != context.principal || self.workspace_id != receipt.scope.workspace_id {
            return Err(WorkbenchError::Forbidden);
        }
        if facts.engine != self.proof.engine
            || !facts.read_only_held
            || facts.actual_database_identity.as_deref()
                != Some(self.proof.actual_database_identity.as_str())
        {
            return Err(WorkbenchError::BindingUnproven);
        }
        match facts.engine {
            // The registered destination may be a new profile and display name.
            // Its actually opened file identity, rather than source provenance,
            // must equal the completed restore's final file identity.
            Engine::Sqlite => Ok(()),
            Engine::Postgresql
                if facts.context.database == self.proof.database_name
                    && facts.server_identity == self.proof.server_identity =>
            {
                Ok(())
            }
            _ => Err(WorkbenchError::BindingUnproven),
        }
    }
}

/// Metadata only: no SQL, secret reference, parameter or result enters this API.
#[async_trait]
pub trait BindingAuthorizer: Send + Sync {
    async fn authorize(
        &self,
        context: &InvocationContext,
        scope: &Scope,
        purpose: AuthorizePurpose,
        expected_revision: Option<&str>,
    ) -> Result<BindingReceipt, WorkbenchError>;
}

#[derive(Clone, Copy, Debug)]
pub struct RuntimeLimits {
    pub sessions: usize,
    pub sessions_per_target: usize,
    pub running_queries: usize,
    pub queued_requests: usize,
    pub connect_timeout: Duration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionLeaseKind {
    Snapshot,
    ReadExport,
    ManualTransaction,
    StandaloneWrite,
}
/// Native-only authority over one physical connection; never deserialized.
pub struct ConnectionLease {
    id: Uuid,
    reference: SessionRef,
    kind: ConnectionLeaseKind,
    expires: Mutex<Instant>,
    expires_at_ms: AtomicU64,
    entry: Weak<Entry>,
    ending: AtomicBool,
    ended: AtomicBool,
}
impl ConnectionLease {
    pub fn id(&self) -> Uuid {
        self.id
    }
    pub fn session(&self) -> SessionRef {
        self.reference
    }
    pub fn kind(&self) -> ConnectionLeaseKind {
        self.kind
    }
    pub fn expires_at_ms(&self) -> u64 {
        self.expires_at_ms.load(Ordering::SeqCst)
    }
    pub fn expired(&self) -> bool {
        self.expires
            .lock()
            .map_or(true, |expires| *expires <= Instant::now())
    }
}
impl Drop for ConnectionLease {
    fn drop(&mut self) {
        if !self.ended.load(Ordering::SeqCst) {
            if let Some(entry) = self.entry.upgrade() {
                entry.active.store(false, Ordering::SeqCst);
            }
        }
    }
}
enum ConnectionAccess<'a> {
    Ordinary,
    Leased(&'a ConnectionLease),
    Final(&'a ConnectionLease),
    Cleanup(&'a ConnectionLease),
    Execution(&'a ExecutionReservation, Option<&'a ConnectionLease>),
}
impl ConnectionAccess<'_> {
    fn cleanup(&self) -> bool {
        matches!(self, Self::Cleanup(_))
    }
}

impl Default for RuntimeLimits {
    fn default() -> Self {
        Self {
            sessions: 8,
            sessions_per_target: 2,
            running_queries: 4,
            queued_requests: 32,
            connect_timeout: Duration::from_secs(15),
        }
    }
}

impl RuntimeLimits {
    fn validate(self) -> Result<Self, WorkbenchError> {
        if self.sessions == 0
            || self.sessions > 8
            || self.sessions_per_target == 0
            || self.sessions_per_target > 2
            || self.running_queries == 0
            || self.running_queries > 4
            || self.queued_requests == 0
            || self.queued_requests > 32
            || self.connect_timeout.is_zero()
            || self.connect_timeout > Duration::from_secs(60)
        {
            return Err(WorkbenchError::Limit);
        }
        Ok(self)
    }
}

struct Entry {
    context: InvocationContext,
    receipt: BindingReceipt,
    credential_ref: Uuid,
    mode: Mode,
    restored_check: Option<RestoredSessionCheck>,
    generation: u64,
    execution_epoch: AtomicU64,
    next_execution_epoch: AtomicU64,
    execution_queue: Mutex<VecDeque<Uuid>>,
    execution_changed: tokio::sync::Notify,
    active: AtomicBool,
    busy: AtomicBool,
    facts: DriverFacts,
    certified: CertifiedCapabilities,
    connection: tokio::sync::Mutex<Option<Box<dyn DriverConnection>>>,
    cancel: Arc<dyn DriverCancel>,
    lease: Mutex<Option<Uuid>>,
    _session_permit: OwnedSemaphorePermit,
    _target_permit: OwnedSemaphorePermit,
}

/// A bounded FIFO ticket reserves its physical execution epoch before IPC reply.
/// It cannot be constructed or deserialized by the frontend.
pub struct ExecutionReservation {
    id: Uuid,
    reference: SessionRef,
    epoch: u64,
    entry: Weak<Entry>,
    phase: AtomicU8,
    cancelled: AtomicBool,
    permit: Mutex<Option<OwnedSemaphorePermit>>,
}
impl ExecutionReservation {
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
    pub fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
    pub fn started(&self) -> bool {
        self.phase.load(Ordering::SeqCst) == 1
    }
    /// True proves the user statement has not entered the physical executor.
    pub fn cancel_before_start(&self) -> bool {
        self.cancelled.store(true, Ordering::SeqCst);
        let queued = self
            .phase
            .compare_exchange(0, 2, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        if let Some(entry) = self.entry.upgrade() {
            entry.execution_changed.notify_waiters();
        }
        queued
    }
    pub fn finish(&self) {
        self.phase.store(3, Ordering::SeqCst);
        if let Some(entry) = self.entry.upgrade() {
            if let Ok(mut queue) = entry.execution_queue.lock() {
                queue.retain(|id| *id != self.id);
            }
            entry.execution_changed.notify_waiters();
        }
        if let Ok(mut permit) = self.permit.lock() {
            permit.take();
        }
    }
}
impl Drop for ExecutionReservation {
    fn drop(&mut self) {
        if self.phase.load(Ordering::SeqCst) == 1 {
            if let Some(entry) = self.entry.upgrade() {
                entry.active.store(false, Ordering::SeqCst);
            }
        }
        self.finish();
    }
}

pub struct WorkbenchState {
    principal: NativePrincipal,
    authorizer: Arc<dyn BindingAuthorizer>,
    credentials: Arc<dyn CredentialProvider>,
    drivers: Arc<dyn DriverFactory>,
    limits: RuntimeLimits,
    sessions: Mutex<HashMap<Uuid, Arc<Entry>>>,
    closed_windows: Mutex<HashSet<Uuid>>,
    targets: Mutex<HashMap<String, Weak<Semaphore>>>,
    session_slots: Arc<Semaphore>,
    running_slots: Arc<Semaphore>,
    request_slots: Arc<Semaphore>,
    control_slots: Arc<Semaphore>,
    generation: AtomicU64,
    credential_epoch: AtomicU64,
    snapshot_certification: Option<crate::browse::SnapshotCertification>,
    write_certification: Option<crate::generated_sql::WriteCertification>,
    store: tokio::sync::OnceCell<Arc<ClientStore>>,
}

impl WorkbenchState {
    /// No filesystem, DPAPI, driver or service call occurs before the integrity check.
    pub fn new(
        principal: NativePrincipal,
        authorizer: Arc<dyn BindingAuthorizer>,
        credentials: Arc<dyn CredentialProvider>,
        drivers: Arc<dyn DriverFactory>,
        limits: RuntimeLimits,
    ) -> Result<Self, WorkbenchError> {
        principal.ensure_standard()?;
        let limits = limits.validate()?;
        Ok(Self {
            principal,
            authorizer,
            credentials,
            drivers,
            limits,
            sessions: Mutex::new(HashMap::new()),
            closed_windows: Mutex::new(HashSet::new()),
            targets: Mutex::new(HashMap::new()),
            session_slots: Arc::new(Semaphore::new(limits.sessions)),
            running_slots: Arc::new(Semaphore::new(limits.running_queries)),
            request_slots: Arc::new(Semaphore::new(limits.queued_requests)),
            control_slots: Arc::new(Semaphore::new(8)),
            generation: AtomicU64::new(1),
            credential_epoch: AtomicU64::new(1),
            snapshot_certification: None,
            write_certification: None,
            store: tokio::sync::OnceCell::new(),
        })
    }

    pub async fn initialize_store(
        &self,
        root: PathBuf,
        policy: Arc<dyn UserRootPolicy>,
    ) -> Result<Arc<ClientStore>, WorkbenchError> {
        self.principal.ensure_standard()?;
        let store = self
            .store
            .get_or_try_init(|| async {
                let principal = self.principal.clone();
                tokio::task::spawn_blocking(move || {
                    ClientStore::open(&root, &principal, policy.as_ref()).map(Arc::new)
                })
                .await
                .map_err(|_| WorkbenchError::StoreUnavailable)?
            })
            .await?;
        Ok(store.clone())
    }

    pub fn principal(&self) -> &NativePrincipal {
        &self.principal
    }

    /// Native composition injects versions backed by executed isolation evidence.
    pub fn with_snapshot_certification(
        mut self,
        certification: crate::browse::SnapshotCertification,
    ) -> Self {
        self.snapshot_certification = Some(certification);
        self
    }

    pub fn with_write_certification(
        mut self,
        certification: crate::generated_sql::WriteCertification,
    ) -> Self {
        self.write_certification = Some(certification);
        self
    }
    pub async fn open(
        &self,
        context: &InvocationContext,
        scope: Scope,
        credential_ref: Uuid,
        expected_revision: Option<&str>,
        mode: Mode,
    ) -> Result<Session, WorkbenchError> {
        self.open_inner(
            context,
            scope,
            credential_ref,
            expected_revision,
            mode,
            None,
        )
        .await
    }
    /// Revalidates actual destination identity before publishing a readonly
    /// restored session. A failed proof never falls back to a source session.
    pub async fn open_checked(
        &self,
        context: &InvocationContext,
        scope: Scope,
        credential_ref: Uuid,
        expected_revision: Option<&str>,
        check: RestoredSessionCheck,
    ) -> Result<Session, WorkbenchError> {
        self.open_inner(
            context,
            scope,
            credential_ref,
            expected_revision,
            Mode::ReadOnly,
            Some(check),
        )
        .await
    }
    // All authority inputs remain explicit; this private function is not a wire DTO.
    #[allow(clippy::too_many_arguments)]
    async fn open_inner(
        &self,
        context: &InvocationContext,
        scope: Scope,
        credential_ref: Uuid,
        expected_revision: Option<&str>,
        mode: Mode,
        restored_check: Option<RestoredSessionCheck>,
    ) -> Result<Session, WorkbenchError> {
        self.check_context(context)?;
        if self
            .closed_windows
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .contains(&context.window_instance_id)
        {
            return Err(WorkbenchError::SessionStale);
        }
        let _request = self.request_slot()?;
        let credential_epoch = self.credential_epoch.load(Ordering::SeqCst);
        let session_permit = self
            .session_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| WorkbenchError::Limit)?;
        let key = serde_json::to_string(&scope).map_err(|_| WorkbenchError::InvalidInput)?;
        let target_slots = {
            let mut targets = self.targets.lock().map_err(|_| WorkbenchError::Io)?;
            targets.retain(|_, slots| slots.strong_count() != 0);
            match targets.get(&key).and_then(Weak::upgrade) {
                Some(slots) => slots,
                None => {
                    let slots = Arc::new(Semaphore::new(self.limits.sessions_per_target));
                    targets.insert(key, Arc::downgrade(&slots));
                    slots
                }
            }
        };
        let target_permit = target_slots
            .try_acquire_owned()
            .map_err(|_| WorkbenchError::Limit)?;
        let receipt = self
            .authorize(
                context,
                &scope,
                AuthorizePurpose::Connect,
                expected_revision,
            )
            .await?;
        let login = self
            .credentials
            .resolve(context, &scope, credential_ref)
            .await?;
        let mut connection = tokio::time::timeout(
            self.limits.connect_timeout,
            self.drivers.connect(&receipt.descriptor, login, mode),
        )
        .await
        .map_err(|_| WorkbenchError::Io)??;
        // Do not publish a connection whose descriptor changed during the login.
        let checked = self
            .authorize(
                context,
                &scope,
                AuthorizePurpose::Connect,
                Some(&receipt.descriptor_revision),
            )
            .await;
        match checked {
            Ok(checked)
                if checked.descriptor == receipt.descriptor
                    && checked.authority_epoch == receipt.authority_epoch => {}
            Ok(_) => {
                let _ = connection.close().await;
                return Err(WorkbenchError::BindingStale);
            }
            Err(error) => {
                let _ = connection.close().await;
                return Err(error);
            }
        }
        if mode == Mode::ReadOnly && !connection.facts().read_only_held {
            let _ = connection.close().await;
            return Err(WorkbenchError::Unsupported);
        }
        let mut facts = connection.facts().clone();
        if let Some(check) = &restored_check {
            if let Err(error) = check.validate(context, &receipt, &facts) {
                let _ = connection.close().await;
                return Err(error);
            }
            facts.context.purpose = TargetPurpose::Restored;
        }
        let certified = CertifiedCapabilities {
            verified_editing: mode != Mode::ReadOnly
                && restored_check.is_none()
                && self
                    .write_certification
                    .as_ref()
                    .is_some_and(|matrix| matrix.supports(&facts)),
            snapshot: self
                .snapshot_certification
                .as_ref()
                .is_some_and(|matrix| matrix.supports(facts.engine, &facts.server_version)),
            ..CertifiedCapabilities::default()
        };
        let cancel = connection.cancel_handle();
        let id = Uuid::new_v4();
        let generation = self.generation.fetch_add(1, Ordering::SeqCst);
        let entry = Arc::new(Entry {
            context: context.clone(),
            receipt,
            credential_ref,
            mode,
            restored_check,
            generation,
            execution_epoch: AtomicU64::new(0),
            next_execution_epoch: AtomicU64::new(1),
            execution_queue: Mutex::new(VecDeque::new()),
            execution_changed: tokio::sync::Notify::new(),
            active: AtomicBool::new(true),
            busy: AtomicBool::new(false),
            facts,
            certified,
            connection: tokio::sync::Mutex::new(Some(connection)),
            cancel,
            lease: Mutex::new(None),
            _session_permit: session_permit,
            _target_permit: target_permit,
        });
        let view = entry.view(id);
        let closed_windows = self.closed_windows.lock().map_err(|_| WorkbenchError::Io)?;
        if closed_windows.contains(&context.window_instance_id) {
            return Err(WorkbenchError::SessionStale);
        }
        let mut sessions = self.sessions.lock().map_err(|_| WorkbenchError::Io)?;
        if credential_epoch != self.credential_epoch.load(Ordering::SeqCst) {
            return Err(WorkbenchError::SessionStale);
        }
        sessions.insert(id, entry);
        Ok(view)
    }

    pub fn list(
        &self,
        context: &InvocationContext,
        scope: &Scope,
    ) -> Result<Vec<Session>, WorkbenchError> {
        self.check_context(context)?;
        let sessions = self.sessions.lock().map_err(|_| WorkbenchError::Io)?;
        let mut views = sessions
            .iter()
            .filter(|(_, entry)| {
                entry.context.window_instance_id == context.window_instance_id
                    && &entry.receipt.scope == scope
            })
            .map(|(id, entry)| entry.view(*id))
            .collect::<Vec<_>>();
        views.sort_by_key(|view| view.session.session_id);
        Ok(views)
    }

    pub fn get(
        &self,
        context: &InvocationContext,
        reference: SessionRef,
    ) -> Result<Session, WorkbenchError> {
        Ok(self
            .entry_including_retired(context, reference)?
            .view(reference.session_id))
    }
    pub fn engine(
        &self,
        context: &InvocationContext,
        reference: SessionRef,
    ) -> Result<Engine, WorkbenchError> {
        Ok(self
            .entry_including_retired(context, reference)?
            .facts
            .engine)
    }

    /// Internal composition hook. Callers must run query policy and durable mutation
    /// admission before choosing a mutating action. This is not a wire command.
    pub async fn with_connection<T: Send, F>(
        &self,
        context: &InvocationContext,
        reference: SessionRef,
        purpose: AuthorizePurpose,
        action: F,
    ) -> Result<T, WorkbenchError>
    where
        F: for<'a> FnOnce(&'a mut dyn DriverConnection) -> BoxFuture<'a, Result<T, WorkbenchError>>
            + Send,
    {
        self.with_access(
            context,
            reference,
            purpose,
            ConnectionAccess::Ordinary,
            action,
        )
        .await
    }

    pub async fn binding_receipt(
        &self,
        context: &InvocationContext,
        reference: SessionRef,
        purpose: AuthorizePurpose,
    ) -> Result<BindingReceipt, WorkbenchError> {
        let entry = self.entry(context, reference)?;
        if purpose.is_mutating() && entry.mode == Mode::ReadOnly {
            return Err(WorkbenchError::Forbidden);
        }
        let fresh = self
            .authorize(
                context,
                &entry.receipt.scope,
                purpose,
                Some(&entry.receipt.descriptor_revision),
            )
            .await?;
        if fresh.authority_epoch != entry.receipt.authority_epoch
            || fresh.descriptor != entry.receipt.descriptor
        {
            return Err(WorkbenchError::BindingStale);
        }
        Ok(fresh)
    }
    pub async fn authorize_scope(
        &self,
        context: &InvocationContext,
        scope: &Scope,
        purpose: AuthorizePurpose,
    ) -> Result<BindingReceipt, WorkbenchError> {
        context.principal.ensure_standard()?;
        if context.principal != self.principal
            || self
                .closed_windows
                .lock()
                .map_err(|_| WorkbenchError::Io)?
                .contains(&context.window_instance_id)
        {
            return Err(WorkbenchError::Forbidden);
        }
        self.authorize(context, scope, purpose, None).await
    }
    pub fn credential_reference(
        &self,
        context: &InvocationContext,
        reference: SessionRef,
    ) -> Result<Uuid, WorkbenchError> {
        Ok(self
            .entry_including_retired(context, reference)?
            .credential_ref)
    }
    pub fn reserve_execution(
        &self,
        context: &InvocationContext,
        reference: SessionRef,
        lease: Option<&ConnectionLease>,
    ) -> Result<ExecutionReservation, WorkbenchError> {
        let permit = self.request_slot()?;
        let entry = self.entry(context, reference)?;
        let current = entry.lease.lock().map_err(|_| WorkbenchError::Io)?;
        if current.is_some() != lease.is_some()
            || lease.is_some_and(|lease| {
                *current != Some(lease.id)
                    || lease.kind != ConnectionLeaseKind::ManualTransaction
                    || lease.expired()
                    || lease.ending.load(Ordering::SeqCst)
                    || !Weak::ptr_eq(&lease.entry, &Arc::downgrade(&entry))
            })
        {
            return Err(WorkbenchError::TargetBusy);
        }
        let mut queue = entry
            .execution_queue
            .lock()
            .map_err(|_| WorkbenchError::Io)?;
        let epoch = entry
            .next_execution_epoch
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                value.checked_add(1)
            })
            .map_err(|_| WorkbenchError::Limit)?;
        let id = Uuid::new_v4();
        queue.push_back(id);
        Ok(ExecutionReservation {
            id,
            reference,
            epoch,
            entry: Arc::downgrade(&entry),
            phase: AtomicU8::new(0),
            cancelled: AtomicBool::new(false),
            permit: Mutex::new(Some(permit)),
        })
    }
    pub async fn with_execution<T: Send, F>(
        &self,
        context: &InvocationContext,
        reservation: &ExecutionReservation,
        lease: Option<&ConnectionLease>,
        purpose: AuthorizePurpose,
        action: F,
    ) -> Result<T, WorkbenchError>
    where
        F: for<'a> FnOnce(&'a mut dyn DriverConnection) -> BoxFuture<'a, Result<T, WorkbenchError>>
            + Send,
    {
        let entry = self.entry(context, reservation.reference)?;
        loop {
            let changed = entry.execution_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if reservation.cancelled() || !entry.active.load(Ordering::SeqCst) {
                return Err(WorkbenchError::SessionStale);
            }
            if entry
                .execution_queue
                .lock()
                .map_err(|_| WorkbenchError::Io)?
                .front()
                == Some(&reservation.id)
            {
                break;
            }
            changed.await;
        }
        self.with_access(
            context,
            reservation.reference,
            purpose,
            ConnectionAccess::Execution(reservation, lease),
            action,
        )
        .await
    }

    pub async fn pin_connection(
        &self,
        context: &InvocationContext,
        reference: SessionRef,
        kind: ConnectionLeaseKind,
        lifetime: Duration,
    ) -> Result<ConnectionLease, WorkbenchError> {
        let maximum = match kind {
            ConnectionLeaseKind::Snapshot => Duration::from_secs(60),
            ConnectionLeaseKind::ReadExport => Duration::from_secs(300),
            _ => Duration::from_secs(30),
        };
        if lifetime.is_zero() || lifetime > maximum {
            return Err(WorkbenchError::Limit);
        }
        let _request = self.request_slot()?;
        let entry = self.entry(context, reference)?;
        let _connection = entry.connection.lock().await;
        if !entry.active.load(Ordering::SeqCst) {
            return Err(WorkbenchError::SessionStale);
        }
        let mut current = entry.lease.lock().map_err(|_| WorkbenchError::Io)?;
        if current.is_some()
            || !entry
                .execution_queue
                .lock()
                .map_err(|_| WorkbenchError::Io)?
                .is_empty()
        {
            return Err(WorkbenchError::TargetBusy);
        }
        let id = Uuid::new_v4();
        *current = Some(id);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| WorkbenchError::Io)?
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        Ok(ConnectionLease {
            id,
            reference,
            kind,
            expires: Mutex::new(Instant::now() + lifetime),
            expires_at_ms: AtomicU64::new(now.saturating_add(lifetime.as_millis() as u64)),
            entry: Arc::downgrade(&entry),
            ending: AtomicBool::new(false),
            ended: AtomicBool::new(false),
        })
    }
    pub async fn with_lease<T: Send, F>(
        &self,
        context: &InvocationContext,
        lease: &ConnectionLease,
        purpose: AuthorizePurpose,
        action: F,
    ) -> Result<T, WorkbenchError>
    where
        F: for<'a> FnOnce(&'a mut dyn DriverConnection) -> BoxFuture<'a, Result<T, WorkbenchError>>
            + Send,
    {
        self.with_access(
            context,
            lease.reference,
            purpose,
            ConnectionAccess::Leased(lease),
            action,
        )
        .await
    }
    /// Fresh authority is retained for COMMIT; queued ordinary work is excluded.
    pub async fn with_lease_final<T: Send, F>(
        &self,
        context: &InvocationContext,
        lease: &ConnectionLease,
        purpose: AuthorizePurpose,
        action: F,
    ) -> Result<T, WorkbenchError>
    where
        F: for<'a> FnOnce(&'a mut dyn DriverConnection) -> BoxFuture<'a, Result<T, WorkbenchError>>
            + Send,
    {
        let entry = self.entry(context, lease.reference)?;
        {
            let _current = entry.lease.lock().map_err(|_| WorkbenchError::Io)?;
            if !entry
                .execution_queue
                .lock()
                .map_err(|_| WorkbenchError::Io)?
                .is_empty()
            {
                return Err(WorkbenchError::TargetBusy);
            }
            if lease.ending.swap(true, Ordering::SeqCst) {
                return Err(WorkbenchError::SessionStale);
            }
        }
        self.with_access(
            context,
            lease.reference,
            purpose,
            ConnectionAccess::Final(lease),
            action,
        )
        .await
    }
    pub fn refresh_manual_idle(
        &self,
        context: &InvocationContext,
        lease: &ConnectionLease,
        lifetime: Duration,
    ) -> Result<(), WorkbenchError> {
        if lifetime.is_zero() || lifetime > Duration::from_secs(30) {
            return Err(WorkbenchError::Limit);
        }
        let entry = self.entry(context, lease.reference)?;
        if lease.kind != ConnectionLeaseKind::ManualTransaction
            || !Weak::ptr_eq(&lease.entry, &Arc::downgrade(&entry))
            || lease.ending.load(Ordering::SeqCst)
            || lease.expired()
        {
            return Err(WorkbenchError::SessionStale);
        }
        *lease.expires.lock().map_err(|_| WorkbenchError::Io)? = Instant::now() + lifetime;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| WorkbenchError::Io)?
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        lease.expires_at_ms.store(
            now.saturating_add(lifetime.as_millis() as u64),
            Ordering::SeqCst,
        );
        Ok(())
    }
    /// Cleanup is native-generated rollback/disconnect only, never a user SQL route.
    pub async fn with_lease_cleanup<T: Send, F>(
        &self,
        context: &InvocationContext,
        lease: &ConnectionLease,
        action: F,
    ) -> Result<T, WorkbenchError>
    where
        F: for<'a> FnOnce(&'a mut dyn DriverConnection) -> BoxFuture<'a, Result<T, WorkbenchError>>
            + Send,
    {
        if lease.ending.swap(true, Ordering::SeqCst) {
            return Err(WorkbenchError::SessionStale);
        }
        self.with_access(
            context,
            lease.reference,
            AuthorizePurpose::Read,
            ConnectionAccess::Cleanup(lease),
            action,
        )
        .await
    }
    pub async fn finish_lease(
        &self,
        context: &InvocationContext,
        lease: &ConnectionLease,
        reusable: bool,
    ) -> Result<(), WorkbenchError> {
        if reusable && !lease.ending.load(Ordering::SeqCst) {
            return Err(WorkbenchError::Forbidden);
        }
        let entry = self.entry_including_retired(context, lease.reference)?;
        {
            let _connection = entry.connection.lock().await;
            let mut current = entry.lease.lock().map_err(|_| WorkbenchError::Io)?;
            if *current != Some(lease.id) || !Weak::ptr_eq(&lease.entry, &Arc::downgrade(&entry)) {
                return Err(WorkbenchError::Forbidden);
            }
            if !reusable {
                entry.active.store(false, Ordering::SeqCst);
            }
            *current = None;
            lease.ended.store(true, Ordering::SeqCst);
        }
        if !reusable || !entry.active.load(Ordering::SeqCst) {
            // Keep only the native basis in the existing bounded session map.
            // Reconnect/explicit close removes it; no old physical driver or
            // SQLite file pin survives a lease retirement.
            self.disconnect_entry(&entry).await?;
        }
        Ok(())
    }
    fn check_access(
        &self,
        entry: &Arc<Entry>,
        access: &ConnectionAccess<'_>,
    ) -> Result<(), WorkbenchError> {
        let current = entry.lease.lock().map_err(|_| WorkbenchError::Io)?;
        match access {
            ConnectionAccess::Ordinary => {
                if current.is_some()
                    || !entry
                        .execution_queue
                        .lock()
                        .map_err(|_| WorkbenchError::Io)?
                        .is_empty()
                {
                    Err(WorkbenchError::TargetBusy)
                } else {
                    Ok(())
                }
            }
            ConnectionAccess::Execution(reservation, lease) => {
                if !Weak::ptr_eq(&reservation.entry, &Arc::downgrade(entry))
                    || reservation.cancelled()
                    || entry
                        .execution_queue
                        .lock()
                        .map_err(|_| WorkbenchError::Io)?
                        .front()
                        != Some(&reservation.id)
                {
                    return Err(WorkbenchError::SessionStale);
                }
                if current.is_some() != lease.is_some() {
                    return Err(WorkbenchError::TargetBusy);
                }
                if let Some(lease) = lease {
                    if *current != Some(lease.id)
                        || lease.expired()
                        || lease.ending.load(Ordering::SeqCst)
                    {
                        return Err(WorkbenchError::SessionStale);
                    }
                }
                Ok(())
            }
            ConnectionAccess::Leased(lease)
            | ConnectionAccess::Final(lease)
            | ConnectionAccess::Cleanup(lease) => {
                if *current != Some(lease.id)
                    || !Weak::ptr_eq(&lease.entry, &Arc::downgrade(entry))
                    || lease.ended.load(Ordering::SeqCst)
                {
                    return Err(WorkbenchError::Forbidden);
                }
                if !access.cleanup() && lease.expired() {
                    entry.active.store(false, Ordering::SeqCst);
                    return Err(WorkbenchError::SessionStale);
                }
                if matches!(access, ConnectionAccess::Leased(_))
                    && lease.ending.load(Ordering::SeqCst)
                {
                    return Err(WorkbenchError::SessionStale);
                }
                if !access.cleanup()
                    && !entry
                        .execution_queue
                        .lock()
                        .map_err(|_| WorkbenchError::Io)?
                        .is_empty()
                {
                    return Err(WorkbenchError::TargetBusy);
                }
                Ok(())
            }
        }
    }
    async fn with_access<T: Send, F>(
        &self,
        context: &InvocationContext,
        reference: SessionRef,
        purpose: AuthorizePurpose,
        access: ConnectionAccess<'_>,
        action: F,
    ) -> Result<T, WorkbenchError>
    where
        F: for<'a> FnOnce(&'a mut dyn DriverConnection) -> BoxFuture<'a, Result<T, WorkbenchError>>
            + Send,
    {
        let _request = if matches!(access, ConnectionAccess::Execution(..)) {
            None
        } else if access.cleanup() {
            Some(
                self.control_slots
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| WorkbenchError::Limit)?,
            )
        } else {
            Some(self.request_slot()?)
        };
        let entry = if access.cleanup() {
            self.entry_including_retired(context, reference)?
        } else {
            self.entry(context, reference)?
        };
        if purpose.is_mutating()
            && matches!(&access,ConnectionAccess::Leased(lease)|ConnectionAccess::Final(lease) if lease.kind==ConnectionLeaseKind::Snapshot)
        {
            return Err(WorkbenchError::Forbidden);
        }
        self.check_access(&entry, &access)?;
        if purpose.is_mutating() && entry.mode == Mode::ReadOnly {
            return Err(WorkbenchError::Forbidden);
        }
        if !access.cleanup() {
            let receipt = match self
                .authorize(
                    context,
                    &entry.receipt.scope,
                    purpose,
                    Some(&entry.receipt.descriptor_revision),
                )
                .await
            {
                Ok(receipt) => receipt,
                Err(error) => {
                    self.disconnect_entry(&entry).await?;
                    return Err(error);
                }
            };
            if receipt.authority_epoch != entry.receipt.authority_epoch {
                self.disconnect_entry(&entry).await?;
                return Err(WorkbenchError::BindingStale);
            }
        }
        let mut connection = entry.connection.lock().await;
        self.check_access(&entry, &access)?;
        if !access.cleanup() && !entry.active.load(Ordering::SeqCst) {
            return Err(WorkbenchError::SessionStale);
        }
        // A queued statement cannot inherit an authorization that expired waiting.
        if !access.cleanup() {
            let fresh = match self
                .authorize(
                    context,
                    &entry.receipt.scope,
                    purpose,
                    Some(&entry.receipt.descriptor_revision),
                )
                .await
            {
                Ok(receipt) => receipt,
                Err(error) => {
                    drop(connection);
                    self.disconnect_entry(&entry).await?;
                    return Err(error);
                }
            };
            if fresh.authority_epoch != entry.receipt.authority_epoch
                || fresh.descriptor != entry.receipt.descriptor
            {
                drop(connection);
                self.disconnect_entry(&entry).await?;
                return Err(WorkbenchError::BindingStale);
            }
        }
        let _running = if access.cleanup() {
            None
        } else if let ConnectionAccess::Execution(reservation, _) = &access {
            loop {
                let changed = entry.execution_changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if reservation.cancelled() || !entry.active.load(Ordering::SeqCst) {
                    return Err(WorkbenchError::SessionStale);
                }
                tokio::select! {
                    permit=self.running_slots.clone().acquire_owned()=>break Some(permit.map_err(|_|WorkbenchError::Limit)?),
                    _=changed=>continue,
                }
            }
        } else {
            Some(
                self.running_slots
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| WorkbenchError::Limit)?,
            )
        };
        self.check_access(&entry, &access)?;
        if !access.cleanup() && !entry.active.load(Ordering::SeqCst) {
            return Err(WorkbenchError::SessionStale);
        }
        let epoch = if let ConnectionAccess::Execution(reservation, _) = &access {
            reservation
                .phase
                .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
                .map_err(|_| WorkbenchError::SessionStale)?;
            reservation.epoch
        } else {
            entry
                .next_execution_epoch
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                    value.checked_add(1)
                })
                .map_err(|_| WorkbenchError::Limit)?
        };
        entry.execution_epoch.store(epoch, Ordering::SeqCst);
        entry.busy.store(true, Ordering::SeqCst);
        let mut guard = InFlightGuard {
            entry: &entry,
            completed: false,
        };
        let result = action(
            connection
                .as_deref_mut()
                .ok_or(WorkbenchError::SessionStale)?,
        )
        .await;
        guard.completed = true;
        entry.busy.store(false, Ordering::SeqCst);
        if result.is_err()
            && !matches!(
                &result,
                Err(WorkbenchError::StatementRejected(
                    crate::driver::DriverTransactionState::Active
                        | crate::driver::DriverTransactionState::Failed
                        | crate::driver::DriverTransactionState::Idle
                ))
            )
        {
            entry.active.store(false, Ordering::SeqCst);
            if let Some(mut native) = connection.take() {
                let _ = tokio::time::timeout(Duration::from_secs(5), native.close()).await;
            }
        }
        result
    }

    pub async fn request_cancel(
        &self,
        context: &InvocationContext,
        reference: SessionRef,
        expected_execution_epoch: u64,
    ) -> Result<(), WorkbenchError> {
        let entry = self.entry(context, reference)?;
        if !entry.busy.load(Ordering::SeqCst)
            || entry.execution_epoch.load(Ordering::SeqCst) != expected_execution_epoch
        {
            return Err(WorkbenchError::SessionStale);
        }
        // Retire before the independent cancel: a late server cancel can never hit
        // a subsequent query on the same socket. A higher layer may prove/reopen.
        entry.active.store(false, Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(5), entry.cancel.request())
            .await
            .map_err(|_| WorkbenchError::CancelUnsettled)?
    }

    pub async fn close(
        &self,
        context: &InvocationContext,
        reference: SessionRef,
    ) -> Result<(), WorkbenchError> {
        let entry = self.entry_including_retired(context, reference)?;
        self.retire(reference.session_id)?;
        self.disconnect_entry(&entry).await
    }
    /// Native cleanup only: discard the physical driver while retaining the
    /// original owner/window/generation basis for an explicit fresh reconnect.
    /// There is no corresponding wire command or new credential authority.
    pub(crate) async fn retire_connection(
        &self,
        context: &InvocationContext,
        reference: SessionRef,
    ) -> Result<(), WorkbenchError> {
        let entry = self.entry_including_retired(context, reference)?;
        self.disconnect_entry(&entry).await
    }
    async fn disconnect_entry(&self, entry: &Arc<Entry>) -> Result<(), WorkbenchError> {
        entry.active.store(false, Ordering::SeqCst);
        entry.execution_changed.notify_waiters();
        if entry.busy.load(Ordering::SeqCst) {
            let _ = tokio::time::timeout(Duration::from_secs(5), entry.cancel.request()).await;
        }
        let mut slot = tokio::time::timeout(Duration::from_secs(5), entry.connection.lock())
            .await
            .map_err(|_| WorkbenchError::CancelUnsettled)?;
        let Some(mut native) = slot.take() else {
            return Ok(());
        };
        drop(slot);
        tokio::time::timeout(Duration::from_secs(5), native.close())
            .await
            .map_err(|_| WorkbenchError::CancelUnsettled)?
    }

    pub async fn reconnect(
        &self,
        context: &InvocationContext,
        reference: SessionRef,
        expected_revision: &str,
    ) -> Result<Session, WorkbenchError> {
        let basis = self.reconnect_basis(context, reference)?;
        self.close(context, reference).await?;
        self.open_inner(
            context,
            basis.scope,
            basis.credential,
            Some(expected_revision),
            basis.mode,
            basis.restored_check,
        )
        .await
    }
    pub(crate) fn reconnect_basis(
        &self,
        context: &InvocationContext,
        reference: SessionRef,
    ) -> Result<ReconnectBasis, WorkbenchError> {
        let entry = self.entry_including_retired(context, reference)?;
        Ok(ReconnectBasis {
            scope: entry.receipt.scope.clone(),
            credential: entry.credential_ref,
            mode: entry.mode,
            restored_check: entry.restored_check.clone(),
        })
    }
    pub fn restored_check(
        &self,
        context: &InvocationContext,
        reference: SessionRef,
    ) -> Result<Option<RestoredSessionCheck>, WorkbenchError> {
        Ok(self
            .entry_including_retired(context, reference)?
            .restored_check
            .clone())
    }
    /// A mode change establishes a fresh physical connection and generation.
    /// No transaction or queued run is silently carried into the new mode.
    pub async fn set_mode(
        &self,
        context: &InvocationContext,
        reference: SessionRef,
        mode: Mode,
    ) -> Result<Session, WorkbenchError> {
        let entry = self.entry(context, reference)?;
        if entry.restored_check.is_some() && mode != Mode::ReadOnly {
            return Err(WorkbenchError::Forbidden);
        }
        if entry.busy.load(Ordering::SeqCst)
            || entry
                .lease
                .lock()
                .map_err(|_| WorkbenchError::Io)?
                .is_some()
            || !entry
                .execution_queue
                .lock()
                .map_err(|_| WorkbenchError::Io)?
                .is_empty()
        {
            return Err(WorkbenchError::TargetBusy);
        }
        let receipt = self
            .authorize(
                context,
                &entry.receipt.scope,
                if mode == Mode::ReadOnly {
                    AuthorizePurpose::Read
                } else {
                    AuthorizePurpose::Mutate
                },
                Some(&entry.receipt.descriptor_revision),
            )
            .await?;
        let credential = entry.credential_ref;
        let scope = receipt.scope.clone();
        let revision = receipt.descriptor_revision.clone();
        let check = entry.restored_check.clone();
        self.close(context, reference).await?;
        drop(entry);
        self.open_inner(context, scope, credential, Some(&revision), mode, check)
            .await
    }

    pub async fn close_window(&self, context: &InvocationContext) -> Result<(), WorkbenchError> {
        self.check_context(context)?;
        self.closed_windows
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .insert(context.window_instance_id);
        let references = self
            .sessions
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .iter()
            .filter(|(_, entry)| entry.context.window_instance_id == context.window_instance_id)
            .map(|(id, entry)| SessionRef {
                session_id: *id,
                connection_generation: entry.generation.into(),
            })
            .collect::<Vec<_>>();
        for reference in references {
            let _ = self.close(context, reference).await;
        }
        Ok(())
    }

    pub async fn invalidate_credential(
        &self,
        context: &InvocationContext,
        credential: Uuid,
    ) -> Result<(), WorkbenchError> {
        self.check_context(context)?;
        let affected = {
            let sessions = self.sessions.lock().map_err(|_| WorkbenchError::Io)?;
            self.credential_epoch.fetch_add(1, Ordering::SeqCst);
            sessions
                .iter()
                .filter(|(_, entry)| entry.credential_ref == credential)
                .map(|(id, entry)| {
                    (
                        entry.context.clone(),
                        SessionRef {
                            session_id: *id,
                            connection_generation: entry.generation.into(),
                        },
                    )
                })
                .collect::<Vec<_>>()
        };
        for (owner_context, reference) in affected {
            let _ = self.close(&owner_context, reference).await;
        }
        Ok(())
    }

    fn retire(&self, id: Uuid) -> Result<(), WorkbenchError> {
        if let Some(entry) = self
            .sessions
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .remove(&id)
        {
            entry.active.store(false, Ordering::SeqCst);
            entry.execution_changed.notify_waiters();
        }
        Ok(())
    }
    fn entry(
        &self,
        context: &InvocationContext,
        reference: SessionRef,
    ) -> Result<Arc<Entry>, WorkbenchError> {
        let entry = self.entry_including_retired(context, reference)?;
        if !entry.active.load(Ordering::SeqCst) {
            return Err(WorkbenchError::SessionStale);
        }
        Ok(entry)
    }
    fn entry_including_retired(
        &self,
        context: &InvocationContext,
        reference: SessionRef,
    ) -> Result<Arc<Entry>, WorkbenchError> {
        self.check_context(context)?;
        let entry = self
            .sessions
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .get(&reference.session_id)
            .cloned()
            .ok_or(WorkbenchError::SessionStale)?;
        if entry.context.window_instance_id != context.window_instance_id
            || entry.generation != reference.connection_generation.value()
        {
            return Err(WorkbenchError::SessionStale);
        }
        Ok(entry)
    }
    fn check_context(&self, context: &InvocationContext) -> Result<(), WorkbenchError> {
        context.principal.ensure_standard()?;
        if context.principal != self.principal || context.window_instance_id.is_nil() {
            return Err(WorkbenchError::Forbidden);
        }
        Ok(())
    }
    fn request_slot(&self) -> Result<OwnedSemaphorePermit, WorkbenchError> {
        self.request_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| WorkbenchError::Limit)
    }
    async fn authorize(
        &self,
        context: &InvocationContext,
        scope: &Scope,
        purpose: AuthorizePurpose,
        revision: Option<&str>,
    ) -> Result<BindingReceipt, WorkbenchError> {
        let receipt = tokio::time::timeout(
            Duration::from_secs(5),
            self.authorizer.authorize(context, scope, purpose, revision),
        )
        .await
        .map_err(|_| WorkbenchError::ServiceUnavailable)??;
        if &receipt.scope != scope
            || revision.is_some_and(|expected| expected != receipt.descriptor_revision)
            || receipt.descriptor_revision.len() != 64
        {
            return Err(WorkbenchError::BindingStale);
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| WorkbenchError::Io)?
            .as_millis() as u64;
        if receipt.authorized_at_ms.value() > now.saturating_add(30_000)
            || now.saturating_sub(receipt.authorized_at_ms.value()) > 30_000
        {
            return Err(WorkbenchError::AuthorizationExpired);
        }
        if purpose.is_mutating() && !receipt.observed_conflicts.is_empty() {
            return Err(WorkbenchError::TargetBusy);
        }
        Ok(receipt)
    }
}

struct InFlightGuard<'a> {
    entry: &'a Entry,
    completed: bool,
}
impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.entry.busy.store(false, Ordering::SeqCst);
        if !self.completed {
            self.entry.active.store(false, Ordering::SeqCst);
        }
    }
}

impl Entry {
    fn view(&self, id: Uuid) -> Session {
        let mut capabilities = BTreeMap::new();
        for (name, supported, reason) in [
            ("query", true, None),
            (
                "server_read_only",
                self.facts.read_only_held,
                Some("not_held"),
            ),
            (
                "read_only_lease",
                self.certified.read_only_lease,
                Some("live_certification_required"),
            ),
            (
                "verified_editing",
                self.certified.verified_editing,
                Some("codec_key_transaction_certification_required"),
            ),
            (
                "snapshot",
                self.certified.snapshot,
                Some("isolation_certification_required"),
            ),
        ] {
            capabilities.insert(
                name.into(),
                Capability {
                    supported,
                    reason: if supported {
                        None
                    } else {
                        reason.map(str::to_owned)
                    },
                },
            );
        }
        Session {
            session: SessionRef {
                session_id: id,
                connection_generation: self.generation.into(),
            },
            scope: self.receipt.scope.clone(),
            descriptor_revision: self.receipt.descriptor_revision.clone(),
            mode: self.mode,
            execution_epoch: self.execution_epoch.load(Ordering::SeqCst).into(),
            state: if !self.active.load(Ordering::SeqCst) {
                SessionState::Retiring
            } else if self.busy.load(Ordering::SeqCst) {
                SessionState::Busy
            } else {
                SessionState::Ready
            },
            actual_context: self.facts.context.clone(),
            capabilities,
        }
    }
}
