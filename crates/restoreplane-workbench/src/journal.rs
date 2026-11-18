//! Metadata-only admission and effect evidence; no target exactly-once promise.
use crate::recovery::RecoveryReport;
use crate::{session::InvocationContext, store::ClientStore, WorkbenchError};
use restoreplane_domain::workbench::*;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;
use zeroize::Zeroizing;

mod continuation;
mod edit_attempts;
mod import_attempts;
mod movement;
mod movement_projection;
mod partial_cleanup;
pub(crate) use continuation::continuation_basis_matches;
pub use continuation::NativeContinuationGrant;
mod query_attempts;

const TOKEN_MS: i64 = 86_400_000;
const RETAIN_MS: i64 = 30 * TOKEN_MS;
const EMBEDDED_STATEMENTS: usize = 100;
pub trait JournalClock: Send + Sync {
    fn now_ms(&self) -> Result<i64, WorkbenchError>;
}
pub struct SystemClock;
impl JournalClock for SystemClock {
    fn now_ms(&self) -> Result<i64, WorkbenchError> {
        i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| WorkbenchError::Io)?
                .as_millis(),
        )
        .map_err(|_| WorkbenchError::Io)
    }
}
#[derive(Clone, Copy)]
pub struct JournalLimits {
    pub operations: usize,
    pub statements_per_operation: usize,
    pub file_and_wal_bytes: u64,
    pub prepared_attempts: usize,
}
impl Default for JournalLimits {
    fn default() -> Self {
        Self {
            operations: 100_000,
            statements_per_operation: 10_000,
            file_and_wal_bytes: 128 * 1024 * 1024,
            prepared_attempts: 1024,
        }
    }
}
#[derive(Clone)]
struct IssuedAttempt {
    owner: String,
    logon_session_id: u32,
    window: Uuid,
    scope: Scope,
    command: String,
    digest: String,
    journal_epoch: String,
    expires: i64,
    descriptor_revision: String,
    authority_epoch: String,
    session: Option<SessionRef>,
}
struct AdmissionLookup<'a> {
    owner: &'a str,
    context: &'a InvocationContext,
    receipt: &'a BindingReceipt,
    token: &'a str,
    digest: &'a str,
    kind: OperationKind,
    session: Option<SessionRef>,
    boot: Uuid,
    now: i64,
}
/// Trusted native policy supplies these digests/classes, never a JS authority flag.
#[derive(Clone, Debug)]
pub struct StatementSpec {
    pub digest: String,
    pub effect_class: EffectClass,
    pub certified_read_only: bool,
    pub transaction_id: Option<Uuid>,
}
#[derive(Clone, Debug)]
pub struct ImportBatch {
    pub parent_operation_id: Uuid,
    pub batch_index: u32,
    pub source_digest: String,
    pub batch_digest: String,
    pub byte_start: u64,
    pub byte_end: u64,
    pub row_start: u64,
    pub row_end: u64,
}
#[derive(Clone, Debug)]
pub struct AdmissionSpec {
    pub kind: OperationKind,
    pub session: Option<SessionRef>,
    pub transaction_id: Option<Uuid>,
    pub statements: Vec<StatementSpec>,
    pub batch: Option<ImportBatch>,
}
/// Constructible only after the durable 0->1 dispatch CAS has committed.
#[derive(Debug)]
pub struct DispatchPermit {
    operation_id: Uuid,
    version: u64,
}
#[derive(Debug)]
pub struct CommitPermit {
    operation_id: Uuid,
    version: u64,
}
impl CommitPermit {
    pub fn operation_id(&self) -> Uuid {
        self.operation_id
    }
    pub fn version(&self) -> u64 {
        self.version
    }
}
pub struct CommitAdmission {
    pub admission: Admission,
    pub permit: Option<CommitPermit>,
}
impl DispatchPermit {
    pub fn operation_id(&self) -> Uuid {
        self.operation_id
    }
    pub fn version(&self) -> u64 {
        self.version
    }
}
#[derive(Clone, Debug)]
pub enum Transition {
    Running,
    CommitRequested,
    CommitConfirmed,
    RollbackConfirmed,
    ReadCompleted(ResultCompleteness),
    StatementGroupCompleted(ResultCompleteness),
    StatementGroupFailed,
    ReadInterrupted,
    EffectsUnknown,
    FailedBeforeSend,
    CancelledBeforeSend,
    Partial,
    JobCompleted,
}
#[derive(Clone, Debug)]
pub struct StatementUpdate {
    pub ordinal: u32,
    pub expected_version: u64,
    pub outcome: StatementStatus,
    pub effect_evidence: EffectEvidence,
    pub rows_matched: Option<u64>,
    pub rows_changed: Option<u64>,
    pub error_code: Option<String>,
}

pub struct OperationJournal {
    store: Arc<ClientStore>,
    boot: Uuid,
    clock: Arc<dyn JournalClock>,
    limits: JournalLimits,
    attempts: Mutex<HashMap<Uuid, IssuedAttempt>>,
    disabled: AtomicBool,
    recovery: RecoveryReport,
    commit_mac_key: Zeroizing<[u8; 32]>,
    continuations: Mutex<HashMap<Uuid, std::sync::Weak<NativeContinuationGrant>>>,
}
impl OperationJournal {
    pub fn open(store: Arc<ClientStore>) -> Result<Self, WorkbenchError> {
        Self::with_clock(store, JournalLimits::default(), Arc::new(SystemClock))
    }
    /// Clock/quotas are injectable native dependencies for deterministic faults.
    pub fn with_clock(
        store: Arc<ClientStore>,
        limits: JournalLimits,
        clock: Arc<dyn JournalClock>,
    ) -> Result<Self, WorkbenchError> {
        if limits.operations == 0
            || limits.operations > 100_000
            || limits.statements_per_operation == 0
            || limits.statements_per_operation > 10_000
            || limits.prepared_attempts == 0
            || limits.prepared_attempts > 1024
            || limits.file_and_wal_bytes < 64 * 1024
            || limits.file_and_wal_bytes > 128 * 1024 * 1024
        {
            return Err(WorkbenchError::Limit);
        }
        let boot = Uuid::new_v4();
        let now = clock.now_ms()?;
        let result = store.write(|c| {
            let check: String = c
                .query_row("PRAGMA quick_check", [], |r| r.get(0))
                .map_err(sql_error)?;
            if check != "ok" {
                return Err(WorkbenchError::StoreUnavailable);
            }
            current_epoch(c, store.owner_id())?;
            crate::recovery::old_boot(c, store.owner_id(), &boot.to_string(), now)
                .map_err(sql_error)
        });
        let (disabled, recovery) = match result {
            Ok(report) => (false, report),
            Err(_) => (true, RecoveryReport::default()),
        };
        Ok(Self {
            store,
            boot,
            clock,
            limits,
            attempts: Mutex::new(HashMap::new()),
            disabled: AtomicBool::new(disabled),
            recovery,
            continuations: Mutex::new(HashMap::new()),
            commit_mac_key: restoreplane_secrets::current_user::ephemeral_user_key()
                .map_err(|_| WorkbenchError::Io)?,
        })
    }
    pub fn write_enabled(&self) -> bool {
        !self.disabled.load(Ordering::SeqCst) && self.store.write_enabled()
    }
    fn commit_mac(
        &self,
        context: &InvocationContext,
        receipt: &BindingReceipt,
        session: Option<SessionRef>,
        digest: &str,
        header: &str,
    ) -> Result<blake3::Hash, WorkbenchError> {
        let binding = payload_digest(
            "native.commit.mac.v1",
            &json!({"header":header,"owner":context.principal().owner_id(),"logon":context.principal().logon_session_id().to_string(),"window":context.window_instance_id(),"scope":receipt.scope,"revision":receipt.descriptor_revision,"authority":receipt.authority_epoch,"session":session,"payloadDigest":digest}),
        )?;
        Ok(blake3::keyed_hash(&self.commit_mac_key, binding.as_bytes()))
    }
    fn verify_commit_token(
        &self,
        context: &InvocationContext,
        receipt: &BindingReceipt,
        session: SessionRef,
        digest: &str,
        token: &str,
        now: i64,
    ) -> Result<Uuid, WorkbenchError> {
        if token.len() > 512 {
            return Err(WorkbenchError::AttemptExpired);
        }
        let (header, mac) = token
            .rsplit_once('.')
            .ok_or(WorkbenchError::AttemptExpired)?;
        let fields = header.split('.').collect::<Vec<_>>();
        if fields.len() != 6 || fields[0] != "wbc1" || fields[1] != self.boot.to_string() {
            return Err(WorkbenchError::AttemptExpired);
        }
        let epoch = self
            .store
            .read(|c| current_epoch(c, self.store.owner_id()))?;
        if fields[2] != epoch {
            return Err(WorkbenchError::AttemptExpired);
        }
        let time = |text: &str| {
            text.parse::<i64>()
                .ok()
                .filter(|v| *v >= 0 && v.to_string() == text)
        };
        let issued = time(fields[3]).ok_or(WorkbenchError::AttemptExpired)?;
        let expires = time(fields[4]).ok_or(WorkbenchError::AttemptExpired)?;
        if expires.checked_sub(issued) != Some(TOKEN_MS) || now < issued || now >= expires {
            return Err(WorkbenchError::AttemptExpired);
        }
        let nonce = Uuid::parse_str(fields[5]).map_err(|_| WorkbenchError::AttemptExpired)?;
        if nonce.is_nil() || nonce.to_string() != fields[5] {
            return Err(WorkbenchError::AttemptExpired);
        }
        if !digest_valid(mac) {
            return Err(WorkbenchError::IdempotencyConflict);
        }
        let supplied =
            blake3::Hash::from_hex(mac).map_err(|_| WorkbenchError::IdempotencyConflict)?;
        // Hash::eq uses the library's constant-time 32-byte comparison.
        if self.commit_mac(context, receipt, Some(session), digest, header)? != supplied {
            return Err(WorkbenchError::IdempotencyConflict);
        }
        Ok(nonce)
    }
    pub fn process_boot_epoch(&self) -> Uuid {
        self.boot
    }
    pub fn recovery_report(&self) -> &RecoveryReport {
        &self.recovery
    }
    fn context(&self, context: &InvocationContext) -> Result<(), WorkbenchError> {
        context.principal().ensure_standard()?;
        if context.principal().owner_id() != self.store.owner_id()
            || context.window_instance_id().is_nil()
        {
            return Err(WorkbenchError::Forbidden);
        }
        Ok(())
    }
    fn write<T>(
        &self,
        work: impl FnOnce(&mut Connection) -> Result<T, WorkbenchError>,
    ) -> Result<T, WorkbenchError> {
        if !self.write_enabled() {
            return Err(WorkbenchError::StoreUnavailable);
        }
        let result = self.store.write(work);
        if matches!(
            result,
            Err(WorkbenchError::StoreUnavailable | WorkbenchError::Io)
        ) {
            self.disabled.store(true, Ordering::SeqCst)
        }
        result
    }
    pub fn prepare(
        &self,
        context: &InvocationContext,
        receipt: &BindingReceipt,
        session: Option<SessionRef>,
        command: &str,
        payload: &Value,
    ) -> Result<PreparedAttempt, WorkbenchError> {
        self.context(context)?;
        if !self.write_enabled() {
            return Err(WorkbenchError::StoreUnavailable);
        }
        if !matches!(
            command,
            "query.run"
                | "transaction.commit"
                | "edit.apply"
                | "import.start"
                | "import.continue"
                | "transaction.begin"
                | "export.start"
                | "native.protection.link"
        ) {
            return Err(WorkbenchError::InvalidInput);
        }
        let digest = payload_digest(command, payload)?;
        let epoch = self
            .store
            .read(|c| current_epoch(c, self.store.owner_id()))?;
        let now = self.clock.now_ms()?;
        validate_receipt(receipt, now)?;
        let expires = now
            .checked_add(TOKEN_MS)
            .ok_or(WorkbenchError::InvalidInput)?;
        if now < 0 {
            return Err(WorkbenchError::InvalidInput);
        }
        let mut attempts = self
            .attempts
            .lock()
            .map_err(|_| WorkbenchError::StoreUnavailable)?;
        attempts.retain(|_, attempt| attempt.expires > now);
        if attempts.len() >= self.limits.prepared_attempts {
            return Err(WorkbenchError::Limit);
        }
        let id = Uuid::new_v4();
        let token = if command == "transaction.commit" {
            let header = format!("wbc1.{}.{}.{}.{}.{}", self.boot, epoch, now, expires, id);
            let mac = self.commit_mac(context, receipt, session, &digest, &header)?;
            format!("{header}.{}", mac.to_hex())
        } else {
            id.to_string()
        };
        attempts.insert(
            id,
            IssuedAttempt {
                owner: self.store.owner_id().into(),
                logon_session_id: context.principal().logon_session_id(),
                window: context.window_instance_id(),
                scope: receipt.scope.clone(),
                command: command.into(),
                digest: digest.clone(),
                journal_epoch: epoch,
                expires,
                descriptor_revision: receipt.descriptor_revision.clone(),
                authority_epoch: receipt.authority_epoch.clone(),
                session,
            },
        );
        Ok(PreparedAttempt {
            attempt_token: token,
            expires_at_ms: (expires as u64).into(),
            payload_digest: digest,
        })
    }
    #[allow(
        clippy::too_many_arguments,
        reason = "native admission explicitly binds context, exact intent, and the fresh authority receipt"
    )]
    pub fn admit(
        &self,
        context: &InvocationContext,
        token: &str,
        command: &str,
        payload: &Value,
        receipt: &BindingReceipt,
        credential_ref: Option<Uuid>,
        spec: AdmissionSpec,
    ) -> Result<Admission, WorkbenchError> {
        self.context(context)?;
        let now = self.clock.now_ms()?;
        let digest = payload_digest(command, payload)?;
        validate_spec(command, &spec, self.limits.statements_per_operation)?;
        let token_id = if command == "edit.apply" && token.starts_with("e1.") {
            self.edit_admission_nonce(
                context,
                receipt,
                spec.session.ok_or(WorkbenchError::InvalidInput)?,
                &digest,
                token,
            )?
        } else if command == "import.start" && token.starts_with("i1.") {
            self.import_admission_nonce(
                context,
                receipt,
                spec.session.ok_or(WorkbenchError::InvalidInput)?,
                &digest,
                token,
            )?
        } else if command == "query.run" && token.starts_with("q1.") {
            self.query_admission_nonce(
                context,
                receipt,
                spec.session.ok_or(WorkbenchError::InvalidInput)?,
                &digest,
                token,
            )?
        } else {
            let nonce = Uuid::parse_str(token).map_err(|_| WorkbenchError::AttemptExpired)?;
            if nonce.is_nil() || nonce.to_string() != token {
                return Err(WorkbenchError::AttemptExpired);
            }
            nonce
        };
        let lookup = AdmissionLookup {
            owner: self.store.owner_id(),
            context,
            receipt,
            token,
            digest: &digest,
            kind: spec.kind,
            session: spec.session,
            boot: self.boot,
            now,
        };
        if let Some(existing) = self.store.read(|c| existing_admission(c, &lookup))? {
            self.attempts
                .lock()
                .map_err(|_| WorkbenchError::StoreUnavailable)?
                .remove(&token_id);
            return Ok(existing);
        }
        let attempt = self
            .attempts
            .lock()
            .map_err(|_| WorkbenchError::StoreUnavailable)?
            .get(&token_id)
            .cloned();
        let Some(attempt) = attempt else {
            // Another native caller may have committed and evicted the same
            // pending token between the first lookup and the memory lookup.
            return self
                .store
                .read(|c| existing_admission(c, &lookup))?
                .ok_or(WorkbenchError::AttemptExpired);
        };
        if attempt.expires <= now {
            return Err(WorkbenchError::AttemptExpired);
        }
        if attempt.owner != self.store.owner_id()
            || attempt.window != context.window_instance_id()
            || attempt.scope != receipt.scope
            || attempt.logon_session_id != context.principal().logon_session_id()
        {
            return Err(WorkbenchError::Forbidden);
        }
        if attempt.descriptor_revision != receipt.descriptor_revision
            || attempt.authority_epoch != receipt.authority_epoch
            || attempt.session != spec.session
        {
            return Err(WorkbenchError::BindingStale);
        }
        if attempt.command != command || attempt.digest != digest {
            return Err(WorkbenchError::IdempotencyConflict);
        }
        validate_receipt(receipt, now)?;
        let batch_grant = match &spec.batch {
            Some(batch) => self.active_continuation(
                context,
                &receipt.scope,
                batch.parent_operation_id,
                spec.session.ok_or(WorkbenchError::InvalidInput)?,
                receipt,
            )?,
            None => None,
        };
        let result=self.write(|c|{
            let tx=c.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql_error)?;
            let now=self.clock.now_ms()?;
            if now>=attempt.expires{return Err(WorkbenchError::AttemptExpired)}
            validate_receipt(receipt,now)?;
            if current_epoch(&tx,self.store.owner_id())?!=attempt.journal_epoch{return Err(WorkbenchError::AttemptExpired)}
            let scope=&attempt.scope; let owner=self.store.owner_id();
            if let Some(existing)=existing_admission(&tx,&lookup)?{return Ok(existing)}
            let proven_read=spec.kind==OperationKind::ExportJob || spec.kind==OperationKind::Query&&!spec.statements.is_empty()&&spec.statements.iter().all(|s|s.effect_class==EffectClass::Read&&s.certified_read_only);
            if !proven_read&&quarantined(&tx,owner,scope,None)?{return Err(WorkbenchError::OutcomeUnknown)}
            self.quota(&tx,spec.statements.len())?;
            if spec.kind==OperationKind::ManualTransaction{
                let session=spec.session.as_ref().ok_or(WorkbenchError::InvalidInput)?;
                let active:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE owner_id=?1 AND session_id=?2 AND connection_generation=?3 AND kind='manual_transaction' AND state IN('accepted','dispatching','running','commit_requested','outcome_unknown'))",params![owner,session.session_id.to_string(),signed(session.connection_generation.value())?],|r|r.get(0)).map_err(sql_error)?;
                if active{return Err(WorkbenchError::IdempotencyConflict)}
            }
            let binding=ensure_binding(&tx,owner,receipt,credential_ref,now)?;
            validate_containing_tx(&tx,owner,scope,spec.transaction_id,spec.session.as_ref(),context,self.boot)?;
            for statement in &spec.statements{validate_containing_tx(&tx,owner,scope,statement.transaction_id,spec.session.as_ref(),context,self.boot)?;}
            if let Some(batch)=&spec.batch{validate_batch(&tx,owner,scope,&binding,context,batch,batch_grant.as_deref())?;}
            let id=Uuid::new_v4(); let batch=spec.batch.as_ref();
            let retain=now.checked_add(RETAIN_MS).ok_or(WorkbenchError::InvalidInput)?;
            tx.execute("INSERT INTO operations(owner_id,workspace_id,target_id,binding_id,operation_id,kind,idempotency_key,payload_digest,journal_epoch,process_boot_epoch,window_instance_id,session_id,connection_generation,transaction_operation_id,transaction_kind,parent_operation_id,parent_kind,batch_index,source_digest,batch_digest,source_byte_start,source_byte_end,source_row_start,source_row_end,state,transaction_outcome,token_expires_at_ms,dedup_retain_until_ms,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,'accepted',?25,?26,?27,?28,?28)",params![owner,scope.workspace_id.to_string(),scope.database_target_id.to_string(),binding,id.to_string(),spec.kind.as_str(),token,digest,attempt.journal_epoch,self.boot.to_string(),context.window_instance_id().to_string(),spec.session.as_ref().map(|s|s.session_id.to_string()),spec.session.as_ref().map(|s|signed(s.connection_generation.value())).transpose()?.unwrap_or(0),spec.transaction_id.map(|id|id.to_string()),spec.transaction_id.map(|_|"manual_transaction"),batch.map(|b|b.parent_operation_id.to_string()),batch.map(|_|"import_job"),batch.map(|b|i64::from(b.batch_index)),batch.map(|b|b.source_digest.as_str()),batch.map(|b|b.batch_digest.as_str()),batch.map(|b|signed(b.byte_start)).transpose()?,batch.map(|b|signed(b.byte_end)).transpose()?,batch.map(|b|signed(b.row_start)).transpose()?,batch.map(|b|signed(b.row_end)).transpose()?,if spec.transaction_id.is_some(){"open"}else{"none"},attempt.expires,retain,now]).map_err(sql_error)?;
            for (ordinal,statement) in spec.statements.iter().enumerate(){
                tx.execute("INSERT INTO statements(owner_id,workspace_id,target_id,operation_id,ordinal,statement_digest,effect_class,outcome,effect_evidence,transaction_operation_id,transaction_kind,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,'not_sent',?8,?9,?10,?11,?11)",params![owner,scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string(),ordinal as i64,statement.digest,statement.effect_class.as_str(),if statement.certified_read_only{"read_only"}else{"none"},statement.transaction_id.map(|id|id.to_string()),statement.transaction_id.map(|_|"manual_transaction"),now]).map_err(sql_error)?;
            }
            tx.commit().map_err(sql_error)?;
            Ok(Admission{operation_id:id,version:1.into(),deduplicated:false})
        });
        if result.is_ok() || matches!(result, Err(WorkbenchError::OutcomeUnknown)) {
            self.attempts
                .lock()
                .map_err(|_| WorkbenchError::StoreUnavailable)?
                .remove(&token_id);
        }
        result
    }
    fn quota(&self, c: &Connection, statements: usize) -> Result<(), WorkbenchError> {
        let count: i64 = c
            .query_row("SELECT COUNT(*) FROM operations", [], |r| r.get(0))
            .map_err(sql_error)?;
        if count >= self.limits.operations as i64 {
            return Err(WorkbenchError::Limit);
        }
        let active:i64=c.query_row("SELECT COUNT(*) FROM operations WHERE state IN('accepted','dispatching','running','commit_requested')",[],|r|r.get(0)).map_err(sql_error)?;
        let active_statements:i64=c.query_row("SELECT COUNT(*) FROM statements s JOIN operations o USING(owner_id,workspace_id,target_id,operation_id) WHERE o.state IN('accepted','dispatching','running','commit_requested') OR s.effect_evidence='pending_transaction'",[],|r|r.get(0)).map_err(sql_error)?;
        let mut bytes = 0u64;
        for name in ["client-state.sqlite3", "client-state.sqlite3-wal"] {
            match std::fs::metadata(self.store.root().join(name)) {
                Ok(m) => bytes = bytes.checked_add(m.len()).ok_or(WorkbenchError::Limit)?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(WorkbenchError::StoreUnavailable),
            }
        }
        let reserve = (active as u64)
            .checked_add(1)
            .and_then(|n| n.checked_mul(65536))
            .and_then(|n| {
                n.checked_add(
                    (statements as u64)
                        .checked_add(active_statements as u64)?
                        .checked_mul(1024)?,
                )
            })
            .ok_or(WorkbenchError::Limit)?;
        if bytes.checked_add(reserve).ok_or(WorkbenchError::Limit)? > self.limits.file_and_wal_bytes
        {
            return Err(WorkbenchError::Limit);
        }
        Ok(())
    }
    pub fn mark_dispatch(
        &self,
        context: &InvocationContext,
        scope: &Scope,
        id: Uuid,
        expected: u64,
        execution_epoch: u64,
        fresh: &BindingReceipt,
    ) -> Result<DispatchPermit, WorkbenchError> {
        self.context(context)?;
        let now = self.clock.now_ms()?;
        validate_receipt(fresh, now)?;
        if &fresh.scope != scope {
            return Err(WorkbenchError::Forbidden);
        }
        self.write(|c|{
            let tx=c.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql_error)?;
            let now=self.clock.now_ms()?;validate_receipt(fresh,now)?;
            let operation=read_operation(&tx,self.store.owner_id(),scope,id)?;
            let bound:i64=tx.query_row("SELECT execution_epoch FROM operations WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4",params![self.store.owner_id(),scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string()],|r|r.get(0)).map_err(sql_error)?;
            if bound!=0&&u64::try_from(bound).ok()!=Some(execution_epoch){return Err(WorkbenchError::IdempotencyConflict)}
            let proven_read=matches!(operation.kind,OperationKind::Query|OperationKind::ExportJob)&&is_readonly(&tx,self.store.owner_id(),scope,id)?;
            if !proven_read&&quarantined(&tx,self.store.owner_id(),scope,Some(id))?{
                let changed=tx.execute("UPDATE operations SET state='cancelled',result_completeness='cancelled',failure_code='WB_OUTCOME_UNKNOWN',version=version+1,terminal_at_ms=MAX(created_at_ms,?8),updated_at_ms=MAX(updated_at_ms,?8) WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4 AND version=?5 AND state='accepted' AND dispatch_count=0 AND window_instance_id=?6 AND process_boot_epoch=?7",params![self.store.owner_id(),scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string(),signed(expected)?,context.window_instance_id().to_string(),self.boot.to_string(),now]).map_err(sql_error)?;
                if changed!=1{return Err(WorkbenchError::IdempotencyConflict)}
                tx.commit().map_err(sql_error)?;return Err(WorkbenchError::OutcomeUnknown)
            }
            if !fresh.observed_conflicts.is_empty()&&!proven_read{return Err(WorkbenchError::TargetBusy)}
            let changed=tx.execute("UPDATE operations SET state='dispatching',dispatch_count=1,execution_epoch=?8,version=version+1,updated_at_ms=MAX(updated_at_ms,?9) WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4 AND version=?5 AND state='accepted' AND dispatch_count=0 AND token_expires_at_ms>?9 AND process_boot_epoch=?6 AND window_instance_id=?7 AND journal_epoch=(SELECT journal_epoch FROM journal_header WHERE singleton=1) AND binding_id IN(SELECT binding_id FROM target_bindings WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND connection_profile_id=?10 AND descriptor_revision=?11 AND authority_epoch=?12 AND state='active')",params![self.store.owner_id(),scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string(),signed(expected)?,self.boot.to_string(),context.window_instance_id().to_string(),signed(execution_epoch)?,now,scope.connection_profile_id.to_string(),fresh.descriptor_revision,fresh.authority_epoch]).map_err(sql_error)?;
            if changed!=1{return Err(WorkbenchError::IdempotencyConflict)}
            tx.commit().map_err(sql_error)?;
            Ok(DispatchPermit{operation_id:id,version:expected.checked_add(1).ok_or(WorkbenchError::Limit)?})
        })
    }
    /// COMMIT belongs to the existing manual-transaction operation. Only the
    /// winner receives a nonserializable permit; duplicates are status reads.
    pub fn request_commit(
        &self,
        context: &InvocationContext,
        token: &str,
        payload: &Value,
        receipt: &BindingReceipt,
        expected: u64,
    ) -> Result<CommitAdmission, WorkbenchError> {
        self.context(context)?;
        let now = self.clock.now_ms()?;
        validate_receipt(receipt, now)?;
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct CommitPayload {
            session: SessionRef,
            transaction_id: Uuid,
        }
        let input: CommitPayload =
            serde_json::from_value(payload.clone()).map_err(|_| WorkbenchError::InvalidInput)?;
        let session = input.session;
        let id = input.transaction_id;
        let digest = payload_digest("transaction.commit", payload)?;
        let token_id = self.verify_commit_token(context, receipt, session, &digest, token, now)?;
        let lookup = AdmissionLookup {
            owner: self.store.owner_id(),
            context,
            receipt,
            token,
            digest: &digest,
            kind: OperationKind::ManualTransaction,
            session: Some(session),
            boot: self.boot,
            now,
        };
        // The durable transaction can answer a repeated COMMIT as status even
        // after its pending token is evicted. This path cannot issue a permit.
        if let Some(admission) = self
            .store
            .read(|c| existing_commit_status(c, &lookup, id))?
        {
            self.attempts
                .lock()
                .map_err(|_| WorkbenchError::StoreUnavailable)?
                .remove(&token_id);
            return Ok(CommitAdmission {
                admission,
                permit: None,
            });
        }
        let attempt = self
            .attempts
            .lock()
            .map_err(|_| WorkbenchError::StoreUnavailable)?
            .get(&token_id)
            .cloned();
        let Some(attempt) = attempt else {
            return self
                .store
                .read(|c| existing_commit_status(c, &lookup, id))?
                .map(|admission| CommitAdmission {
                    admission,
                    permit: None,
                })
                .ok_or(WorkbenchError::AttemptExpired);
        };
        if now >= attempt.expires {
            return Err(WorkbenchError::AttemptExpired);
        }
        if attempt.owner != self.store.owner_id()
            || attempt.window != context.window_instance_id()
            || attempt.scope != receipt.scope
            || attempt.logon_session_id != context.principal().logon_session_id()
        {
            return Err(WorkbenchError::Forbidden);
        }
        if attempt.command != "transaction.commit" || attempt.digest != digest {
            return Err(WorkbenchError::IdempotencyConflict);
        }
        if attempt.descriptor_revision != receipt.descriptor_revision
            || attempt.authority_epoch != receipt.authority_epoch
        {
            return Err(WorkbenchError::BindingStale);
        }
        if attempt.session != Some(session) {
            return Err(WorkbenchError::SessionStale);
        }
        if !receipt.observed_conflicts.is_empty() {
            return Err(WorkbenchError::TargetBusy);
        }
        let result=self.write(|c|{
            let tx=c.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql_error)?;
            let now=self.clock.now_ms()?;
            if now>=attempt.expires{return Err(WorkbenchError::AttemptExpired)}
            validate_receipt(receipt,now)?;
            if current_epoch(&tx,self.store.owner_id())?!=attempt.journal_epoch{return Err(WorkbenchError::AttemptExpired)}
            let old=read_operation(&tx,self.store.owner_id(),&receipt.scope,id)?;
            if old.kind!=OperationKind::ManualTransaction{return Err(WorkbenchError::InvalidInput)}
            if let Some(admission)=existing_commit_status(&tx,&lookup,id)?{return Ok(CommitAdmission{admission,permit:None})}
            if quarantined(&tx,self.store.owner_id(),&receipt.scope,Some(id))?{return Err(WorkbenchError::OutcomeUnknown)}
            let changed=tx.execute("UPDATE operations SET state='commit_requested',transaction_outcome='commit_requested',version=version+1,updated_at_ms=MAX(updated_at_ms,?10) WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4 AND state='running' AND version=?5 AND session_id=?6 AND connection_generation=?7 AND window_instance_id=?8 AND process_boot_epoch=?9 AND binding_id IN(SELECT binding_id FROM target_bindings WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND descriptor_revision=?11 AND authority_epoch=?12 AND connection_profile_id=?13)",params![self.store.owner_id(),receipt.scope.workspace_id.to_string(),receipt.scope.database_target_id.to_string(),id.to_string(),signed(expected)?,session.session_id.to_string(),signed(session.connection_generation.value())?,context.window_instance_id().to_string(),self.boot.to_string(),now,receipt.descriptor_revision,receipt.authority_epoch,receipt.scope.connection_profile_id.to_string()]).map_err(sql_error)?;
            if changed!=1{return Err(WorkbenchError::IdempotencyConflict)}
            tx.commit().map_err(sql_error)?;let version=expected.checked_add(1).ok_or(WorkbenchError::Limit)?;
            Ok(CommitAdmission{admission:Admission{operation_id:id,version:version.into(),deduplicated:false},permit:Some(CommitPermit{operation_id:id,version})})
        });
        if result.is_ok() || matches!(result, Err(WorkbenchError::OutcomeUnknown)) {
            self.attempts
                .lock()
                .map_err(|_| WorkbenchError::StoreUnavailable)?
                .remove(&token_id);
        }
        result
    }
    pub fn scope_for_operation(
        &self,
        context: &InvocationContext,
        id: Uuid,
    ) -> Result<Scope, WorkbenchError> {
        self.context(context)?;
        self.store.read(|c|{
            let mut query=c.prepare("SELECT json_object('workspaceId',o.workspace_id,'connectionProfileId',b.connection_profile_id,'databaseTargetId',o.target_id) FROM operations o JOIN target_bindings b USING(owner_id,workspace_id,target_id,binding_id) WHERE o.owner_id=?1 AND o.operation_id=?2 LIMIT 2").map_err(sql_error)?;
            let rows=query.query_map(params![self.store.owner_id(),id.to_string()],|r|r.get::<_,String>(0)).map_err(sql_error)?.collect::<rusqlite::Result<Vec<_>>>().map_err(sql_error)?;
            if rows.len()!=1{return Err(WorkbenchError::Forbidden)}serde_json::from_str(&rows[0]).map_err(|_|WorkbenchError::StoreUnavailable)
        })
    }
    pub fn get(
        &self,
        context: &InvocationContext,
        scope: &Scope,
        id: Uuid,
    ) -> Result<Operation, WorkbenchError> {
        self.context(context)?;
        self.store.read(|c| {
            let mut operation = read_operation(c, self.store.owner_id(), scope, id)?;
            operation.statements =
                read_statements(c, self.store.owner_id(), scope, id, 0, EMBEDDED_STATEMENTS)?;
            operation.effect_summary =
                effect_summary(c, self.store.owner_id(), scope, id, &operation)?;
            // A failed local finalize cannot turn a durable send marker into a
            // success/rollback claim. This conservative view survives until a
            // later successful restart reconciliation can persist the outcome.
            if !self.write_enabled()
                && matches!(
                    operation.state,
                    OperationState::Dispatching
                        | OperationState::Running
                        | OperationState::CommitRequested
                )
            {
                if operation.kind == OperationKind::ExportJob {
                    operation.state = OperationState::Interrupted;
                    operation.transaction_outcome = TransactionOutcome::None;
                    operation.effect_summary = EffectSummary::None;
                    operation.result_completeness = ResultCompleteness::Interrupted;
                } else {
                    operation.state = OperationState::OutcomeUnknown;
                    operation.transaction_outcome = TransactionOutcome::OutcomeUnknown;
                    operation.effect_summary = EffectSummary::Unknown;
                }
            }
            Ok(operation)
        })
    }
    pub fn statements(
        &self,
        context: &InvocationContext,
        scope: &Scope,
        id: Uuid,
        after: Option<u32>,
        limit: usize,
    ) -> Result<Vec<StatementOutcome>, WorkbenchError> {
        self.context(context)?;
        if limit == 0 || limit > 200 {
            return Err(WorkbenchError::Limit);
        }
        self.store.read(|c| {
            read_operation(c, self.store.owner_id(), scope, id)?;
            read_statements(
                c,
                self.store.owner_id(),
                scope,
                id,
                after.map_or(0, |n| i64::from(n) + 1),
                limit,
            )
        })
    }
    pub fn list(
        &self,
        context: &InvocationContext,
        scope: &Scope,
        state: Option<OperationState>,
        after: Option<(u64, Uuid)>,
        limit: usize,
    ) -> Result<Vec<Operation>, WorkbenchError> {
        self.context(context)?;
        if limit == 0 || limit > 200 {
            return Err(WorkbenchError::Limit);
        }
        let ids=self.store.read(|c|{
            let mut query=c.prepare("SELECT o.operation_id FROM operations o JOIN target_bindings b USING(owner_id,workspace_id,target_id,binding_id) WHERE o.owner_id=?1 AND o.workspace_id=?2 AND o.target_id=?3 AND b.connection_profile_id=?4 AND (?5 IS NULL OR o.state=?5 OR (?5='outcome_unknown' AND ?9=1 AND o.state IN('dispatching','running','commit_requested'))) AND (?6 IS NULL OR o.updated_at_ms>?6 OR (o.updated_at_ms=?6 AND o.operation_id>?7)) ORDER BY o.updated_at_ms,o.operation_id LIMIT ?8").map_err(sql_error)?;
            let rows=query.query_map(params![self.store.owner_id(),scope.workspace_id.to_string(),scope.database_target_id.to_string(),scope.connection_profile_id.to_string(),state.map(OperationState::as_str),after.map(|(time,_)|signed(time)).transpose()?,after.map(|(_,id)|id.to_string()),limit as i64,!self.write_enabled()],|r|r.get::<_,String>(0)).map_err(sql_error)?;
            rows.map(|r|uuid(&r.map_err(sql_error)?)).collect::<Result<Vec<_>,_>>()
        })?;
        self.store.read(|c| {
            ids.into_iter()
                .map(|id| {
                    let mut operation = read_operation(c, self.store.owner_id(), scope, id)?;
                    operation.effect_summary =
                        effect_summary(c, self.store.owner_id(), scope, id, &operation)?;
                    if !self.write_enabled()
                        && matches!(
                            operation.state,
                            OperationState::Dispatching
                                | OperationState::Running
                                | OperationState::CommitRequested
                        )
                    {
                        if operation.kind == OperationKind::ExportJob {
                            operation.state = OperationState::Interrupted;
                            operation.transaction_outcome = TransactionOutcome::None;
                            operation.effect_summary = EffectSummary::None;
                            operation.result_completeness = ResultCompleteness::Interrupted;
                        } else {
                            operation.state = OperationState::OutcomeUnknown;
                            operation.transaction_outcome = TransactionOutcome::OutcomeUnknown;
                            operation.effect_summary = EffectSummary::Unknown;
                        }
                    }
                    Ok(operation)
                })
                .collect()
        })
    }
    pub fn transition(
        &self,
        context: &InvocationContext,
        scope: &Scope,
        id: Uuid,
        expected: u64,
        next: Transition,
    ) -> Result<Operation, WorkbenchError> {
        self.context(context)?;
        let now = self.clock.now_ms()?;
        self.write(|c|{
            let tx=c.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql_error)?;
            let old=read_operation(&tx,self.store.owner_id(),scope,id)?;
            if old.version.value()!=expected{return Err(WorkbenchError::IdempotencyConflict)}
            let (dispatch,window,boot):(i64,String,String)=tx.query_row("SELECT dispatch_count,window_instance_id,process_boot_epoch FROM operations WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4",params![self.store.owner_id(),scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).map_err(sql_error)?;
            let historical_proof=old.state==OperationState::OutcomeUnknown && matches!(next,Transition::CommitConfirmed|Transition::RollbackConfirmed);
            if !historical_proof&&(window!=context.window_instance_id().to_string()||boot!=self.boot.to_string()){return Err(WorkbenchError::Forbidden)}
            let read_only=is_readonly(&tx,self.store.owner_id(),scope,id)?;
            if matches!(next,Transition::CommitRequested)&&quarantined(&tx,self.store.owner_id(),scope,Some(id))?{return Err(WorkbenchError::OutcomeUnknown)}
            let running=matches!(old.state,OperationState::Dispatching|OperationState::Running);
            let (state,outcome,completion)=match next {
                Transition::Running if old.state==OperationState::Dispatching =>(OperationState::Running,if old.kind==OperationKind::ManualTransaction{TransactionOutcome::Open}else{old.transaction_outcome},old.result_completeness),
                Transition::CommitRequested if old.state==OperationState::Running&&!read_only =>(OperationState::CommitRequested,TransactionOutcome::CommitRequested,old.result_completeness),
                Transition::CommitConfirmed if matches!(old.state,OperationState::Running|OperationState::CommitRequested|OperationState::OutcomeUnknown)&&!read_only =>(OperationState::Committed,TransactionOutcome::Committed,old.result_completeness),
                Transition::RollbackConfirmed if matches!(old.state,OperationState::Running|OperationState::CommitRequested|OperationState::OutcomeUnknown)&&!read_only =>(OperationState::RolledBack,TransactionOutcome::RolledBack,old.result_completeness),
                Transition::ReadCompleted(completeness) if running&&read_only&&old.kind!=OperationKind::ExportJob&&matches!(completeness,ResultCompleteness::Complete|ResultCompleteness::Capped) =>{
                    let pending:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM statements WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4 AND outcome<>'succeeded')",params![self.store.owner_id(),scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string()],|r|r.get(0)).map_err(sql_error)?;
                    if pending{return Err(WorkbenchError::OutcomeUnknown)}(OperationState::Completed,old.transaction_outcome,completeness)
                },
                Transition::StatementGroupCompleted(completeness) if old.state==OperationState::Running&&old.kind==OperationKind::Query&&matches!(completeness,ResultCompleteness::Complete|ResultCompleteness::Capped)=>{
                    let unproven:bool=tx.query_row("SELECT NOT EXISTS(SELECT 1 FROM statements WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4) OR EXISTS(SELECT 1 FROM statements WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4 AND (outcome<>'succeeded' OR effect_evidence IN('none','outcome_unknown')))",params![self.store.owner_id(),scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string()],|r|r.get(0)).map_err(sql_error)?;
                    if unproven{return Err(WorkbenchError::OutcomeUnknown)}
                    (OperationState::Completed,old.transaction_outcome,completeness)
                },
                Transition::StatementGroupFailed if old.state==OperationState::Running&&old.kind==OperationKind::Query=>{
                    let (failed,succeeded,unproven):(bool,bool,bool)=tx.query_row("SELECT EXISTS(SELECT 1 FROM statements WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4 AND outcome='failed'),EXISTS(SELECT 1 FROM statements WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4 AND outcome='succeeded'),EXISTS(SELECT 1 FROM statements WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4 AND (outcome IN('sent','outcome_unknown') OR effect_evidence='outcome_unknown' OR (effect_evidence='none' AND outcome<>'not_sent') OR (effect_class='unknown' AND outcome<>'not_sent' AND effect_evidence<>'read_only')))",params![self.store.owner_id(),scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).map_err(sql_error)?;
                    if !failed||unproven{return Err(WorkbenchError::OutcomeUnknown)}
                    (if succeeded{OperationState::Partial}else{OperationState::Failed},old.transaction_outcome,if succeeded{ResultCompleteness::Partial}else{ResultCompleteness::Interrupted})
                },
                Transition::ReadInterrupted if running&&read_only =>(OperationState::Interrupted,old.transaction_outcome,ResultCompleteness::Interrupted),
                Transition::EffectsUnknown if dispatch==1&&old.kind==OperationKind::ExportJob&&running =>(OperationState::Interrupted,TransactionOutcome::None,ResultCompleteness::Interrupted),
                Transition::EffectsUnknown if dispatch==1&&old.kind!=OperationKind::ExportJob&&matches!(old.state,OperationState::Dispatching|OperationState::Running|OperationState::CommitRequested|OperationState::OutcomeUnknown) =>(OperationState::OutcomeUnknown,TransactionOutcome::OutcomeUnknown,ResultCompleteness::Interrupted),
                Transition::FailedBeforeSend if old.state==OperationState::Accepted&&dispatch==0 =>(OperationState::Failed,TransactionOutcome::None,ResultCompleteness::None),
                Transition::CancelledBeforeSend if old.state==OperationState::Accepted&&dispatch==0 =>(OperationState::Cancelled,TransactionOutcome::None,ResultCompleteness::Cancelled),
                Transition::Partial if old.state==OperationState::Running=>{
                    let unknown:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND parent_operation_id=?4 AND state='outcome_unknown')",params![self.store.owner_id(),scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string()],|r|r.get(0)).map_err(sql_error)?;
                    if unknown{(OperationState::OutcomeUnknown,TransactionOutcome::OutcomeUnknown,ResultCompleteness::Partial)}else{(OperationState::Partial,old.transaction_outcome,ResultCompleteness::Partial)}
                },
                Transition::JobCompleted if old.state==OperationState::Running&&matches!(old.kind,OperationKind::ImportJob|OperationKind::ProtectionLink)=>{
                    let unresolved:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND parent_operation_id=?4 AND state<>'committed')",params![self.store.owner_id(),scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string()],|r|r.get(0)).map_err(sql_error)?;
                    if unresolved{return Err(WorkbenchError::OutcomeUnknown)}
                    (OperationState::Completed,TransactionOutcome::None,ResultCompleteness::Complete)
                },
                _=>return Err(WorkbenchError::InvalidInput),
            };
            let changed=tx.execute("UPDATE operations SET state=?6,transaction_outcome=?7,result_completeness=?8,version=version+1,terminal_at_ms=?9,updated_at_ms=MAX(updated_at_ms,?10) WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4 AND version=?5",params![self.store.owner_id(),scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string(),signed(expected)?,state.as_str(),outcome.as_str(),completion.as_str(),state.is_terminal().then_some(now.max(old.created_at_ms.value() as i64)),now]).map_err(sql_error)?;
            if changed!=1{return Err(WorkbenchError::IdempotencyConflict)}
            if matches!(outcome,TransactionOutcome::Committed|TransactionOutcome::RolledBack|TransactionOutcome::OutcomeUnknown){
                settle_statements(&tx,self.store.owner_id(),scope,id,old.kind,outcome,now)?;
            }
            tx.commit().map_err(sql_error)?;Ok(())
        })?;
        self.get(context, scope, id)
    }
    pub fn record_statement(
        &self,
        context: &InvocationContext,
        scope: &Scope,
        id: Uuid,
        operation_version: u64,
        update: StatementUpdate,
    ) -> Result<Operation, WorkbenchError> {
        self.context(context)?;
        let now = self.clock.now_ms()?;
        if let Some(code) = &update.error_code {
            validate_code(code)?;
        }
        self.write(|c|{
            let tx=c.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql_error)?;
            let operation=read_operation(&tx,self.store.owner_id(),scope,id)?;
            if operation.version.value()!=operation_version||!matches!(operation.state,OperationState::Dispatching|OperationState::Running|OperationState::CommitRequested){return Err(WorkbenchError::IdempotencyConflict)}
            let native:bool=tx.query_row("SELECT window_instance_id=?5 AND process_boot_epoch=?6 FROM operations WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4",params![self.store.owner_id(),scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string(),context.window_instance_id().to_string(),self.boot.to_string()],|r|r.get(0)).map_err(sql_error)?;
            if !native{return Err(WorkbenchError::Forbidden)}
            let old=read_statements(&tx,self.store.owner_id(),scope,id,i64::from(update.ordinal),1)?.into_iter().next().filter(|s|s.ordinal==update.ordinal).ok_or(WorkbenchError::InvalidInput)?;
            if old.version.value()!=update.expected_version{return Err(WorkbenchError::IdempotencyConflict)}
            let legal=match old.outcome {
                StatementStatus::NotSent=>matches!(update.outcome,StatementStatus::Sent|StatementStatus::Cancelled)
                    ||(operation.kind==OperationKind::VerifiedApply&&update.outcome==StatementStatus::Succeeded&&update.rows_matched==Some(1)&&update.rows_changed==Some(0)&&update.effect_evidence==EffectEvidence::PendingTransaction&&update.error_code.is_none()),
                StatementStatus::Sent=>matches!(update.outcome,StatementStatus::Succeeded|StatementStatus::Failed|StatementStatus::OutcomeUnknown)||(update.outcome==StatementStatus::Cancelled&&old.effect_evidence==EffectEvidence::ReadOnly),
                _=>false,
            };
            if !legal|| (update.effect_evidence==EffectEvidence::ReadOnly&&old.effect_evidence!=EffectEvidence::ReadOnly)
                ||(update.effect_evidence==EffectEvidence::PendingTransaction&&old.transaction_id.is_none()&&!matches!(operation.kind,OperationKind::ManualTransaction|OperationKind::VerifiedApply|OperationKind::ImportBatch))
                ||(update.outcome==StatementStatus::Sent&&update.effect_evidence!=old.effect_evidence)
                ||(old.effect_evidence==EffectEvidence::ReadOnly&&update.effect_evidence!=EffectEvidence::ReadOnly)
                ||(update.outcome==StatementStatus::OutcomeUnknown&&old.effect_evidence!=EffectEvidence::ReadOnly&&update.effect_evidence!=EffectEvidence::OutcomeUnknown){return Err(WorkbenchError::InvalidInput)}
            let changed=tx.execute("UPDATE statements SET outcome=?7,effect_evidence=?8,matched_rows=?9,changed_rows=?10,failure_code=?11,version=version+1,updated_at_ms=MAX(updated_at_ms,?12) WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4 AND ordinal=?5 AND version=?6",params![self.store.owner_id(),scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string(),i64::from(update.ordinal),signed(update.expected_version)?,update.outcome.as_str(),update.effect_evidence.as_str(),update.rows_matched.map(|v|v.to_string()),update.rows_changed.map(|v|v.to_string()),update.error_code,now]).map_err(sql_error)?;
            if changed!=1{return Err(WorkbenchError::IdempotencyConflict)}
            let changed=tx.execute("UPDATE operations SET version=version+1,updated_at_ms=MAX(updated_at_ms,?6) WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4 AND version=?5",params![self.store.owner_id(),scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string(),signed(operation_version)?,now]).map_err(sql_error)?;
            if changed!=1{return Err(WorkbenchError::IdempotencyConflict)}
            tx.commit().map_err(sql_error)?;Ok(())
        })?;
        self.get(context, scope, id)
    }
    pub fn acknowledge_unknown(
        &self,
        context: &InvocationContext,
        scope: &Scope,
        id: Uuid,
        expected: u64,
    ) -> Result<Operation, WorkbenchError> {
        self.context(context)?;
        let now = self.clock.now_ms()?;
        self.write(|c|{
            let tx=c.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql_error)?;
            read_operation(&tx,self.store.owner_id(),scope,id)?;
            let changed=tx.execute("UPDATE operations SET operator_acknowledged_at_ms=?6,version=version+1,updated_at_ms=MAX(updated_at_ms,?6) WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4 AND version=?5 AND state='outcome_unknown'",params![self.store.owner_id(),scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string(),signed(expected)?,now]).map_err(sql_error)?;
            if changed!=1{return Err(WorkbenchError::IdempotencyConflict)}tx.commit().map_err(sql_error)?;Ok(())
        })?;
        self.get(context, scope, id)
    }
    /// Keep identity/key/digest/version tombstones. Uncertainty is never pruned.
    pub fn tombstone_eligible(&self, context: &InvocationContext) -> Result<usize, WorkbenchError> {
        self.context(context)?;
        let now = self.clock.now_ms()?;
        self.write(|c|{
            let tx=c.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql_error)?;
            let changed=tx.execute("UPDATE operations SET tombstoned_at_ms=?2,version=version+1,updated_at_ms=MAX(updated_at_ms,?2) WHERE owner_id=?1 AND state IN('committed','rolled_back','failed','completed','cancelled','interrupted','partial') AND terminal_at_ms IS NOT NULL AND terminal_at_ms<=?3 AND dedup_retain_until_ms<=?2 AND tombstoned_at_ms IS NULL AND NOT EXISTS(SELECT 1 FROM operations child WHERE child.owner_id=operations.owner_id AND child.workspace_id=operations.workspace_id AND child.target_id=operations.target_id AND child.parent_operation_id=operations.operation_id AND child.state IN('accepted','dispatching','running','commit_requested','outcome_unknown'))",params![self.store.owner_id(),now,now.saturating_sub(RETAIN_MS)]).map_err(sql_error)?;
            tx.commit().map_err(sql_error)?;Ok(changed)
        })
    }
    /// Explicit reconciliation boundary; old operation epochs remain immutable.
    pub fn rotate_epoch_after_reconciliation(
        &self,
        context: &InvocationContext,
    ) -> Result<Uuid, WorkbenchError> {
        self.context(context)?;
        let now = self.clock.now_ms()?;
        let epoch = Uuid::new_v4();
        self.write(|c|{
            let tx=c.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql_error)?;
            let active:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE state IN('accepted','dispatching','running','commit_requested'))",[],|r|r.get(0)).map_err(sql_error)?;
            if active{return Err(WorkbenchError::IdempotencyConflict)}
            tx.execute("UPDATE journal_header SET journal_epoch=?2,updated_at_ms=MAX(updated_at_ms,?3) WHERE singleton=1 AND owner_id=?1",params![self.store.owner_id(),epoch.to_string(),now]).map_err(sql_error)?;
            tx.commit().map_err(sql_error)?;Ok(())
        })?;
        self.attempts
            .lock()
            .map_err(|_| WorkbenchError::StoreUnavailable)?
            .clear();
        Ok(epoch)
    }
}

fn sql_error(_: rusqlite::Error) -> WorkbenchError {
    WorkbenchError::StoreUnavailable
}
fn uuid(value: &str) -> Result<Uuid, WorkbenchError> {
    Uuid::parse_str(value)
        .ok()
        .filter(|id| !id.is_nil())
        .ok_or(WorkbenchError::StoreUnavailable)
}
fn signed(value: u64) -> Result<i64, WorkbenchError> {
    i64::try_from(value).map_err(|_| WorkbenchError::InvalidInput)
}
fn positive(value: i64) -> Result<u64, WorkbenchError> {
    u64::try_from(value)
        .ok()
        .filter(|v| *v > 0)
        .ok_or(WorkbenchError::StoreUnavailable)
}
struct DurableAttempt {
    id: String,
    digest: String,
    version: i64,
    window: String,
    boot: String,
    epoch: String,
    expires: i64,
    session: Option<String>,
    generation: i64,
    profile: String,
    revision: String,
    authority: String,
    current_epoch: String,
}
fn existing_admission(
    c: &Connection,
    lookup: &AdmissionLookup<'_>,
) -> Result<Option<Admission>, WorkbenchError> {
    let scope = &lookup.receipt.scope;
    let existing=c.query_row("SELECT o.operation_id,o.payload_digest,o.version,o.window_instance_id,o.process_boot_epoch,o.journal_epoch,o.token_expires_at_ms,o.session_id,o.connection_generation,b.connection_profile_id,b.descriptor_revision,b.authority_epoch,h.journal_epoch FROM operations o JOIN target_bindings b USING(owner_id,workspace_id,target_id,binding_id) JOIN journal_header h ON h.owner_id=o.owner_id AND h.singleton=1 WHERE o.owner_id=?1 AND o.workspace_id=?2 AND o.target_id=?3 AND o.kind=?4 AND o.idempotency_key=?5",params![lookup.owner,scope.workspace_id.to_string(),scope.database_target_id.to_string(),lookup.kind.as_str(),lookup.token],|row|Ok(DurableAttempt{id:row.get(0)?,digest:row.get(1)?,version:row.get(2)?,window:row.get(3)?,boot:row.get(4)?,epoch:row.get(5)?,expires:row.get(6)?,session:row.get(7)?,generation:row.get(8)?,profile:row.get(9)?,revision:row.get(10)?,authority:row.get(11)?,current_epoch:row.get(12)?})).optional().map_err(sql_error)?;
    let Some(existing) = existing else {
        return Ok(None);
    };
    if existing.expires <= lookup.now
        || existing.boot != lookup.boot.to_string()
        || existing.epoch != existing.current_epoch
    {
        return Err(WorkbenchError::AttemptExpired);
    }
    uuid(&existing.current_epoch)?;
    if existing.window != lookup.context.window_instance_id().to_string()
        || existing.profile != scope.connection_profile_id.to_string()
    {
        return Err(WorkbenchError::Forbidden);
    }
    if existing.digest != lookup.digest {
        return Err(WorkbenchError::IdempotencyConflict);
    }
    if existing.revision != lookup.receipt.descriptor_revision
        || existing.authority != lookup.receipt.authority_epoch
        || existing.session != lookup.session.map(|s| s.session_id.to_string())
        || existing.generation
            != lookup
                .session
                .map(|s| signed(s.connection_generation.value()))
                .transpose()?
                .unwrap_or(0)
    {
        return Err(WorkbenchError::BindingStale);
    }
    validate_receipt(lookup.receipt, lookup.now)?;
    Ok(Some(Admission {
        operation_id: uuid(&existing.id)?,
        version: positive(existing.version)?.into(),
        deduplicated: true,
    }))
}
fn existing_commit_status(
    c: &Connection,
    lookup: &AdmissionLookup<'_>,
    id: Uuid,
) -> Result<Option<Admission>, WorkbenchError> {
    let operation = read_operation(c, lookup.owner, &lookup.receipt.scope, id)?;
    if operation.kind != OperationKind::ManualTransaction {
        return Err(WorkbenchError::InvalidInput);
    }
    if !matches!(
        operation.state,
        OperationState::CommitRequested
            | OperationState::Committed
            | OperationState::RolledBack
            | OperationState::OutcomeUnknown
    ) {
        return Ok(None);
    }
    let session = lookup.session.ok_or(WorkbenchError::SessionStale)?;
    let valid:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM operations o JOIN target_bindings b USING(owner_id,workspace_id,target_id,binding_id) JOIN journal_header h ON h.owner_id=o.owner_id AND h.singleton=1 WHERE o.owner_id=?1 AND o.workspace_id=?2 AND o.target_id=?3 AND o.operation_id=?4 AND o.session_id=?5 AND o.connection_generation=?6 AND o.window_instance_id=?7 AND o.process_boot_epoch=?8 AND o.journal_epoch=h.journal_epoch AND o.token_expires_at_ms>?9 AND b.descriptor_revision=?10 AND b.authority_epoch=?11 AND b.connection_profile_id=?12)",params![lookup.owner,lookup.receipt.scope.workspace_id.to_string(),lookup.receipt.scope.database_target_id.to_string(),id.to_string(),session.session_id.to_string(),signed(session.connection_generation.value())?,lookup.context.window_instance_id().to_string(),lookup.boot.to_string(),lookup.now,lookup.receipt.descriptor_revision,lookup.receipt.authority_epoch,lookup.receipt.scope.connection_profile_id.to_string()],|r|r.get(0)).map_err(sql_error)?;
    if !valid {
        return Err(WorkbenchError::Forbidden);
    }
    Ok(Some(Admission {
        operation_id: id,
        version: operation.version,
        deduplicated: true,
    }))
}
fn digest_valid(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn validate_code(value: &str) -> Result<(), WorkbenchError> {
    if value.starts_with("WB_")
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    {
        Ok(())
    } else {
        Err(WorkbenchError::InvalidInput)
    }
}
fn quarantined(
    c: &Connection,
    owner: &str,
    scope: &Scope,
    except: Option<Uuid>,
) -> Result<bool, WorkbenchError> {
    c.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND kind<>'export_job' AND state='outcome_unknown' AND operator_acknowledged_at_ms IS NULL AND (?4 IS NULL OR operation_id<>?4))",params![owner,scope.workspace_id.to_string(),scope.database_target_id.to_string(),except.map(|id|id.to_string())],|r|r.get(0)).map_err(sql_error)
}
fn current_epoch(c: &Connection, owner: &str) -> Result<String, WorkbenchError> {
    let epoch:String=c.query_row("SELECT journal_epoch FROM journal_header WHERE singleton=1 AND schema_version=1 AND owner_id=?1",[owner],|r|r.get(0)).map_err(sql_error)?;
    uuid(&epoch)?;
    Ok(epoch)
}
pub fn payload_digest(command: &str, payload: &Value) -> Result<String, WorkbenchError> {
    if !payload.is_object() {
        return Err(WorkbenchError::InvalidInput);
    }
    fn canonical(value: &Value, depth: usize) -> Result<Value, WorkbenchError> {
        if depth > 32 {
            return Err(WorkbenchError::Limit);
        }
        Ok(match value {
            Value::Object(values) => {
                let sorted: BTreeMap<_, _> = values
                    .iter()
                    .map(|(key, value)| Ok((key.clone(), canonical(value, depth + 1)?)))
                    .collect::<Result<_, WorkbenchError>>()?;
                Value::Object(sorted.into_iter().collect())
            }
            Value::Array(values) => {
                if values.len() > 32768 {
                    return Err(WorkbenchError::Limit);
                }
                Value::Array(
                    values
                        .iter()
                        .map(|v| canonical(v, depth + 1))
                        .collect::<Result<_, _>>()?,
                )
            }
            Value::Number(value) if !value.is_i64() && !value.is_u64() => {
                return Err(WorkbenchError::InvalidInput)
            }
            value => value.clone(),
        })
    }
    let bytes = serde_json::to_vec(&canonical(
        &json!({"command":command,"payload":payload}),
        0,
    )?)
    .map_err(|_| WorkbenchError::InvalidInput)?;
    if bytes.len() > 1024 * 1024 {
        return Err(WorkbenchError::Limit);
    }
    let mut hash = blake3::Hasher::new();
    hash.update(b"restoreplane.workbench.intent.v1\0");
    hash.update(&bytes);
    Ok(hash.finalize().to_hex().to_string())
}
pub(crate) fn validate_receipt(receipt: &BindingReceipt, now: i64) -> Result<(), WorkbenchError> {
    if !digest_valid(&receipt.descriptor_revision)
        || Uuid::parse_str(&receipt.authority_epoch).is_err()
        || signed(receipt.authorized_at_ms.value())? > now
        || now.saturating_sub(signed(receipt.authorized_at_ms.value())?) > 30_000
    {
        return Err(WorkbenchError::BindingStale);
    }
    Ok(())
}
fn validate_spec(command: &str, spec: &AdmissionSpec, max: usize) -> Result<(), WorkbenchError> {
    let compatible = match command {
        "query.run" => spec.kind == OperationKind::Query,
        "transaction.begin" => spec.kind == OperationKind::ManualTransaction,
        "export.start" => spec.kind == OperationKind::ExportJob,
        "native.protection.link" => spec.kind == OperationKind::ProtectionLink,
        "edit.apply" => spec.kind == OperationKind::VerifiedApply,
        "import.start" => spec.kind == OperationKind::ImportJob,
        "import.continue" => spec.kind == OperationKind::ImportBatch,
        _ => false,
    };
    if !compatible
        || spec.statements.len() > max
        || (spec.kind == OperationKind::ImportBatch) != spec.batch.is_some()
        || spec.transaction_id.is_some() && spec.kind != OperationKind::Query
        || spec.kind == OperationKind::ManualTransaction && spec.session.is_none()
        || spec.kind == OperationKind::ExportJob && !spec.statements.is_empty()
    {
        return Err(WorkbenchError::InvalidInput);
    }
    if let Some(session) = &spec.session {
        if session.session_id.is_nil() || session.connection_generation.value() == 0 {
            return Err(WorkbenchError::InvalidInput);
        }
    }
    for statement in &spec.statements {
        if !digest_valid(&statement.digest)
            || statement.certified_read_only && statement.effect_class != EffectClass::Read
        {
            return Err(WorkbenchError::InvalidInput);
        }
    }
    if let Some(transaction) = spec.transaction_id {
        if spec
            .statements
            .iter()
            .any(|statement| statement.transaction_id != Some(transaction))
        {
            return Err(WorkbenchError::InvalidInput);
        }
    }
    if let Some(batch) = &spec.batch {
        if batch.parent_operation_id.is_nil()
            || !digest_valid(&batch.source_digest)
            || !digest_valid(&batch.batch_digest)
            || batch.byte_end <= batch.byte_start
            || batch.row_end <= batch.row_start
        {
            return Err(WorkbenchError::InvalidInput);
        }
    }
    Ok(())
}
pub(crate) fn ensure_binding(
    c: &Connection,
    owner: &str,
    receipt: &BindingReceipt,
    credential: Option<Uuid>,
    now: i64,
) -> Result<String, WorkbenchError> {
    let scope = &receipt.scope;
    let proposed = Uuid::new_v4().to_string();
    let source = blake3::hash(
        &serde_json::to_vec(&receipt.descriptor.database)
            .map_err(|_| WorkbenchError::InvalidInput)?,
    )
    .to_hex()
    .to_string();
    c.execute("INSERT OR IGNORE INTO target_bindings(owner_id,workspace_id,target_id,binding_id,connection_profile_id,descriptor_revision,authority_epoch,source_identity_digest,engine,credential_ref,state,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,'active',?11,?11)",params![owner,scope.workspace_id.to_string(),scope.database_target_id.to_string(),proposed,scope.connection_profile_id.to_string(),receipt.descriptor_revision,receipt.authority_epoch,source,receipt.descriptor.engine.as_str(),credential.map(|id|id.to_string()),now]).map_err(sql_error)?;
    let (id,profile,identity,state):(String,String,String,String)=c.query_row("SELECT binding_id,connection_profile_id,source_identity_digest,state FROM target_bindings WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND descriptor_revision=?4 AND authority_epoch=?5",params![owner,scope.workspace_id.to_string(),scope.database_target_id.to_string(),receipt.descriptor_revision,receipt.authority_epoch],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).map_err(sql_error)?;
    if profile != scope.connection_profile_id.to_string() || identity != source || state != "active"
    {
        return Err(WorkbenchError::BindingStale);
    }
    Ok(id)
}
fn validate_containing_tx(
    c: &Connection,
    owner: &str,
    scope: &Scope,
    id: Option<Uuid>,
    session: Option<&SessionRef>,
    context: &InvocationContext,
    boot: Uuid,
) -> Result<(), WorkbenchError> {
    let Some(id) = id else { return Ok(()) };
    let session = session.ok_or(WorkbenchError::InvalidInput)?;
    let exists:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM operations o JOIN target_bindings b USING(owner_id,workspace_id,target_id,binding_id) WHERE o.owner_id=?1 AND o.workspace_id=?2 AND o.target_id=?3 AND o.operation_id=?4 AND o.kind='manual_transaction' AND o.state='running' AND o.transaction_outcome IN('none','open') AND o.session_id=?5 AND o.connection_generation=?6 AND b.connection_profile_id=?7 AND o.window_instance_id=?8 AND o.process_boot_epoch=?9)",params![owner,scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string(),session.session_id.to_string(),signed(session.connection_generation.value())?,scope.connection_profile_id.to_string(),context.window_instance_id().to_string(),boot.to_string()],|r|r.get(0)).map_err(sql_error)?;
    if exists {
        Ok(())
    } else {
        Err(WorkbenchError::Forbidden)
    }
}
fn validate_batch(
    c: &Connection,
    owner: &str,
    scope: &Scope,
    binding: &str,
    context: &InvocationContext,
    batch: &ImportBatch,
    grant: Option<&NativeContinuationGrant>,
) -> Result<(), WorkbenchError> {
    if let Some(grant) = grant {
        grant.durable(c, owner)?;
    }
    let parent:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4 AND kind='import_job' AND ((binding_id=?5 AND window_instance_id=?6) OR ?7) AND state IN('accepted','dispatching','running'))",params![owner,scope.workspace_id.to_string(),scope.database_target_id.to_string(),batch.parent_operation_id.to_string(),binding,context.window_instance_id().to_string(),grant.is_some()],|r|r.get(0)).map_err(sql_error)?;
    if !parent {
        return Err(WorkbenchError::Forbidden);
    }
    let prior:Option<(i64,String,String,i64,i64)>=c.query_row("SELECT batch_index,state,source_digest,source_byte_end,source_row_end FROM operations WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND parent_operation_id=?4 ORDER BY batch_index DESC LIMIT 1",params![owner,scope.workspace_id.to_string(),scope.database_target_id.to_string(),batch.parent_operation_id.to_string()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional().map_err(sql_error)?;
    match prior {
        None if batch.batch_index == 0 && batch.byte_start == 0 && batch.row_start == 0 => Ok(()),
        Some((index, state, digest, bytes, rows))
            if state == "committed"
                && i64::from(batch.batch_index) == index + 1
                && digest == batch.source_digest
                && signed(batch.byte_start)? == bytes
                && signed(batch.row_start)? == rows =>
        {
            Ok(())
        }
        Some((_, state, _, _, _)) if state == "outcome_unknown" => {
            Err(WorkbenchError::OutcomeUnknown)
        }
        _ => Err(WorkbenchError::IdempotencyConflict),
    }
}

fn read_operation(
    c: &Connection,
    owner: &str,
    scope: &Scope,
    id: Uuid,
) -> Result<Operation, WorkbenchError> {
    let encoded:String=c.query_row("SELECT json_object('operationId',o.operation_id,'version',CAST(o.version AS TEXT),'kind',o.kind,'state',o.state,'effectSummary','none','transactionOutcome',o.transaction_outcome,'resultCompleteness',o.result_completeness,'transactionId',CASE WHEN o.kind='manual_transaction' THEN o.operation_id ELSE o.transaction_operation_id END,'statements',json('[]'),'parentOperationId',o.parent_operation_id,'batchIndex',o.batch_index,'acknowledgedAtMs',CASE WHEN o.operator_acknowledged_at_ms IS NOT NULL THEN CAST(o.operator_acknowledged_at_ms AS TEXT) END,'createdAtMs',CAST(o.created_at_ms AS TEXT),'updatedAtMs',CAST(o.updated_at_ms AS TEXT)) FROM operations o JOIN target_bindings b USING(owner_id,workspace_id,target_id,binding_id) WHERE o.owner_id=?1 AND o.workspace_id=?2 AND o.target_id=?3 AND o.operation_id=?4 AND b.connection_profile_id=?5",params![owner,scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string(),scope.connection_profile_id.to_string()],|r|r.get(0)).optional().map_err(sql_error)?.ok_or(WorkbenchError::Forbidden)?;
    serde_json::from_str(&encoded).map_err(|_| WorkbenchError::StoreUnavailable)
}
fn read_statements(
    c: &Connection,
    owner: &str,
    scope: &Scope,
    id: Uuid,
    start: i64,
    limit: usize,
) -> Result<Vec<StatementOutcome>, WorkbenchError> {
    let mut query=c.prepare("SELECT json_object('ordinal',ordinal,'digest',statement_digest,'outcome',outcome,'effectClass',effect_class,'effectEvidence',effect_evidence,'rowsMatched',matched_rows,'rowsChanged',changed_rows,'version',CAST(version AS TEXT),'errorCode',failure_code,'transactionId',transaction_operation_id) FROM statements WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4 AND ordinal>=?5 ORDER BY ordinal LIMIT ?6").map_err(sql_error)?;
    let rows = query
        .query_map(
            params![
                owner,
                scope.workspace_id.to_string(),
                scope.database_target_id.to_string(),
                id.to_string(),
                start,
                limit as i64
            ],
            |r| r.get::<_, String>(0),
        )
        .map_err(sql_error)?;
    rows.map(|row| {
        serde_json::from_str(&row.map_err(sql_error)?).map_err(|_| WorkbenchError::StoreUnavailable)
    })
    .collect()
}
fn is_readonly(
    c: &Connection,
    owner: &str,
    scope: &Scope,
    id: Uuid,
) -> Result<bool, WorkbenchError> {
    c.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4 AND kind='export_job') OR (EXISTS(SELECT 1 FROM statements WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4) AND NOT EXISTS(SELECT 1 FROM statements WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4 AND (effect_class<>'read' OR effect_evidence<>'read_only')))",params![owner,scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string()],|r|r.get(0)).map_err(sql_error)
}
fn effect_summary(
    c: &Connection,
    owner: &str,
    scope: &Scope,
    id: Uuid,
    operation: &Operation,
) -> Result<EffectSummary, WorkbenchError> {
    let (unknown,transactional,read,committed,rolled_back,transaction_scopes):(i64,i64,i64,i64,i64,i64)=c.query_row("SELECT COALESCE(SUM(outcome='outcome_unknown' OR effect_evidence='outcome_unknown' OR (effect_class='unknown' AND outcome<>'not_sent')),0),COALESCE(SUM(effect_evidence IN('pending_transaction','committed','rolled_back')),0),COALESCE(SUM(effect_evidence='read_only' AND outcome<>'not_sent'),0),COALESCE(SUM(effect_evidence='committed'),0),COALESCE(SUM(effect_evidence='rolled_back'),0),COUNT(DISTINCT COALESCE(transaction_operation_id,'none')) FROM statements WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4",params![owner,scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).map_err(sql_error)?;
    Ok(
        if unknown > 0
            || operation.state == OperationState::OutcomeUnknown
            || operation.transaction_outcome == TransactionOutcome::OutcomeUnknown
        {
            EffectSummary::Unknown
        } else if operation.state == OperationState::Partial
            || committed > 0 && rolled_back > 0
            || transaction_scopes > 1
        {
            EffectSummary::Partial
        } else if transactional > 0 {
            EffectSummary::Transactional
        } else if read > 0 {
            EffectSummary::Read
        } else {
            EffectSummary::None
        },
    )
}
fn settle_statements(
    c: &Connection,
    owner: &str,
    scope: &Scope,
    id: Uuid,
    kind: OperationKind,
    outcome: TransactionOutcome,
    now: i64,
) -> Result<(), WorkbenchError> {
    let evidence = match outcome {
        TransactionOutcome::Committed => "committed",
        TransactionOutcome::RolledBack => "rolled_back",
        TransactionOutcome::OutcomeUnknown => "outcome_unknown",
        _ => return Ok(()),
    };
    if kind == OperationKind::ManualTransaction {
        // Mixed scripts may have no aggregate transaction FK; use each statement
        // link so an implicit commit cannot be overwritten by another scope.
        c.execute("UPDATE operations SET transaction_outcome=CASE WHEN transaction_operation_id=?4 THEN ?5 ELSE transaction_outcome END,state=CASE WHEN ?5='outcome_unknown' THEN 'outcome_unknown' ELSE state END,terminal_at_ms=CASE WHEN ?5='outcome_unknown' THEN COALESCE(terminal_at_ms,?6) ELSE terminal_at_ms END,version=version+1,updated_at_ms=MAX(updated_at_ms,?6) WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id<>?4 AND (transaction_operation_id=?4 OR EXISTS(SELECT 1 FROM statements s WHERE s.owner_id=operations.owner_id AND s.workspace_id=operations.workspace_id AND s.target_id=operations.target_id AND s.operation_id=operations.operation_id AND s.transaction_operation_id=?4 AND s.effect_evidence='pending_transaction'))",params![owner,scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string(),evidence,now]).map_err(sql_error)?;
        c.execute("UPDATE statements SET effect_evidence=?5,version=version+1,updated_at_ms=MAX(updated_at_ms,?6) WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND transaction_operation_id=?4 AND effect_evidence='pending_transaction'",params![owner,scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string(),evidence,now]).map_err(sql_error)?;
    }
    c.execute("UPDATE statements SET effect_evidence=CASE WHEN effect_evidence='pending_transaction' THEN ?5 ELSE effect_evidence END,outcome=CASE WHEN ?5='outcome_unknown' AND outcome='sent' THEN 'outcome_unknown' ELSE outcome END,version=version+1,updated_at_ms=MAX(updated_at_ms,?6) WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4 AND (effect_evidence='pending_transaction' OR (?5='outcome_unknown' AND outcome='sent'))",params![owner,scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string(),evidence,now]).map_err(sql_error)?;
    Ok(())
}
