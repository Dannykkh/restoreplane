//! Explicit bounded continuation authority. Historical parent ownership columns
//! are immutable; one durable metadata marker distinguishes retained requests.
use super::*;
use restoreplane_domain::workbench_movement::{
    MovementContinuationMarker, MovementMetadata, NextUnsent,
};
use std::sync::Weak;
fn continuation_token(token: &str) -> Result<(Uuid, Uuid), WorkbenchError> {
    let parts = token.split('.').collect::<Vec<_>>();
    if token.len() > 256 || parts.len() != 3 || parts[0] != "c1" {
        return Err(WorkbenchError::AttemptExpired);
    }
    let parse = |value: &str| {
        Uuid::parse_str(value)
            .ok()
            .filter(|id| !id.is_nil() && id.simple().to_string() == value)
            .ok_or(WorkbenchError::AttemptExpired)
    };
    Ok((parse(parts[1])?, parse(parts[2])?))
}

pub struct NativeContinuationGrant {
    marker: MovementContinuationMarker,
    owner: String,
    logon: u32,
    parent: Uuid,
    scope: Scope,
}
impl NativeContinuationGrant {
    pub fn next(&self) -> NextUnsent {
        self.marker.next_unsent.clone()
    }
    fn context(
        &self,
        journal: &OperationJournal,
        ctx: &InvocationContext,
        scope: &Scope,
        session: SessionRef,
        receipt: &BindingReceipt,
    ) -> Result<(), WorkbenchError> {
        if self.owner != ctx.principal().owner_id()
            || self.logon != ctx.principal().logon_session_id()
            || self.marker.window_instance_id != ctx.window_instance_id()
            || self.scope != *scope
            || self.marker.session != session
        {
            return Err(WorkbenchError::Forbidden);
        }
        if self.marker.process_boot_epoch != journal.boot
            || self.marker.descriptor_revision != receipt.descriptor_revision
            || self.marker.authority_epoch != receipt.authority_epoch
        {
            return Err(WorkbenchError::BindingStale);
        }
        if self.marker.expires_at_ms.value() <= journal.clock.now_ms()? as u64 {
            return Err(WorkbenchError::AttemptExpired);
        }
        Ok(())
    }
    pub(super) fn durable(&self, c: &Connection, owner: &str) -> Result<(), WorkbenchError> {
        let raw:Option<String>=c.query_row("SELECT movement_metadata_json FROM operations WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4 AND kind='import_job'",params![owner,self.scope.workspace_id.to_string(),self.scope.database_target_id.to_string(),self.parent.to_string()],|row|row.get(0)).map_err(sql_error)?;
        let Some(MovementMetadata::Import {
            latest_continuation: Some(marker),
            ..
        }) = raw
            .map(|raw| serde_json::from_str(&raw).map_err(|_| WorkbenchError::StoreUnavailable))
            .transpose()?
        else {
            return Err(WorkbenchError::AttemptExpired);
        };
        if marker != self.marker || current_epoch(c, owner)? != self.marker.journal_epoch {
            return Err(WorkbenchError::AttemptExpired);
        }
        Ok(())
    }
}
impl OperationJournal {
    pub fn prepare_continuation_attempt(
        &self,
        ctx: &InvocationContext,
        receipt: &BindingReceipt,
        session: SessionRef,
        parent: Uuid,
        resolved: &Value,
    ) -> Result<PreparedAttempt, WorkbenchError> {
        let prepared = self.prepare(ctx, receipt, Some(session), "import.continue", resolved)?;
        let nonce = uuid(&prepared.attempt_token)?;
        Ok(PreparedAttempt {
            attempt_token: format!("c1.{}.{}", parent.simple(), nonce.simple()),
            ..prepared
        })
    }
    pub(super) fn active_continuation(
        &self,
        ctx: &InvocationContext,
        scope: &Scope,
        parent: Uuid,
        session: SessionRef,
        receipt: &BindingReceipt,
    ) -> Result<Option<Arc<NativeContinuationGrant>>, WorkbenchError> {
        let mut grants = self
            .continuations
            .lock()
            .map_err(|_| WorkbenchError::StoreUnavailable)?;
        grants.retain(|_, grant| grant.strong_count() > 0);
        let Some(grant) = grants.get(&parent).and_then(Weak::upgrade) else {
            return Ok(None);
        };
        grant.context(self, ctx, scope, session, receipt)?;
        Ok(Some(grant))
    }
    pub(super) fn movement_grant(
        &self,
        ctx: &InvocationContext,
        scope: &Scope,
        parent: Uuid,
    ) -> Result<Option<Arc<NativeContinuationGrant>>, WorkbenchError> {
        let grant = self
            .continuations
            .lock()
            .map_err(|_| WorkbenchError::StoreUnavailable)?
            .get(&parent)
            .and_then(Weak::upgrade);
        let Some(grant) = grant else { return Ok(None) };
        if grant.owner != ctx.principal().owner_id()
            || grant.logon != ctx.principal().logon_session_id()
            || grant.marker.window_instance_id != ctx.window_instance_id()
            || grant.scope != *scope
            || grant.marker.process_boot_epoch != self.boot
            || grant.marker.expires_at_ms.value() <= self.clock.now_ms()? as u64
        {
            return Err(WorkbenchError::Forbidden);
        }
        Ok(Some(grant))
    }
    /// Status-only retained-marker lookup. No native review/cache is required
    /// for an exact duplicate, and no child or grant is created by this read.
    pub fn continuation_status(
        &self,
        ctx: &InvocationContext,
        token: &str,
        public: &Value,
        session: SessionRef,
    ) -> Result<Option<Admission>, WorkbenchError> {
        self.context(ctx)?;
        let (bound_parent, _) = continuation_token(token)?;
        let digest = payload_digest("import.continue", public)?;
        let id = public
            .get("parentOperationId")
            .and_then(Value::as_str)
            .and_then(|value| Uuid::parse_str(value).ok())
            .ok_or(WorkbenchError::InvalidInput)?;
        if id != bound_parent {
            return Err(WorkbenchError::IdempotencyConflict);
        }
        let scope = self.scope_for_operation(ctx, id)?;
        let operation = self.get(ctx, &scope, id)?;
        let Some(MovementMetadata::Import {
            latest_continuation: Some(marker),
            ..
        }) = self.movement_metadata(ctx, &scope, id)?
        else {
            return Ok(None);
        };
        if marker.token_hash != blake3::hash(token.as_bytes()).to_hex().to_string() {
            return Ok(None);
        }
        if marker.public_digest != digest {
            return Err(WorkbenchError::IdempotencyConflict);
        }
        if marker.window_instance_id != ctx.window_instance_id() || marker.session != session {
            return Err(WorkbenchError::Forbidden);
        }
        if marker.process_boot_epoch != self.boot
            || marker.expires_at_ms.value() <= self.clock.now_ms()? as u64
            || self
                .store
                .read(|c| current_epoch(c, self.store.owner_id()))?
                != marker.journal_epoch
        {
            return Err(WorkbenchError::AttemptExpired);
        }
        Ok(Some(Admission {
            operation_id: id,
            version: operation.version,
            deduplicated: true,
        }))
    }
    #[allow(
        clippy::too_many_arguments,
        reason = "exact native continuation claims bind reviewed source, parent range, session and fresh authority"
    )]
    pub fn admit_continuation(
        &self,
        ctx: &InvocationContext,
        token: &str,
        public: &Value,
        resolved: &Value,
        receipt: &BindingReceipt,
        session: SessionRef,
        expected: u64,
        new_basis: &MovementMetadata,
    ) -> Result<(Admission, Option<Arc<NativeContinuationGrant>>), WorkbenchError> {
        if let Some(admission) = self.continuation_status(ctx, token, public, session)? {
            return Ok((admission, None));
        }
        let (_, nonce) = continuation_token(token)?;
        let digest = payload_digest("import.continue", resolved)?;
        let public_digest = payload_digest("import.continue", public)?;
        let attempt = self
            .attempts
            .lock()
            .map_err(|_| WorkbenchError::StoreUnavailable)?
            .get(&nonce)
            .cloned()
            .ok_or(WorkbenchError::AttemptExpired)?;
        let now = self.clock.now_ms()?;
        validate_receipt(receipt, now)?;
        if attempt.owner != self.store.owner_id()
            || attempt.logon_session_id != ctx.principal().logon_session_id()
            || attempt.window != ctx.window_instance_id()
            || attempt.scope != receipt.scope
            || attempt.session != Some(session)
        {
            return Err(WorkbenchError::Forbidden);
        }
        if attempt.expires <= now {
            return Err(WorkbenchError::AttemptExpired);
        }
        if attempt.command != "import.continue" || attempt.digest != digest {
            return Err(WorkbenchError::IdempotencyConflict);
        }
        if attempt.descriptor_revision != receipt.descriptor_revision
            || attempt.authority_epoch != receipt.authority_epoch
        {
            return Err(WorkbenchError::BindingStale);
        }
        let parent = public
            .get("parentOperationId")
            .and_then(Value::as_str)
            .and_then(|value| Uuid::parse_str(value).ok())
            .ok_or(WorkbenchError::InvalidInput)?;
        let scope = &receipt.scope;
        let next = self
            .import_next_unsent(ctx, scope, parent)?
            .ok_or(WorkbenchError::InvalidInput)?;
        let marker = MovementContinuationMarker {
            token_hash: blake3::hash(token.as_bytes()).to_hex().to_string(),
            public_digest,
            resolved_digest: digest,
            window_instance_id: ctx.window_instance_id(),
            session,
            descriptor_revision: receipt.descriptor_revision.clone(),
            authority_epoch: receipt.authority_epoch.clone(),
            journal_epoch: attempt.journal_epoch.clone(),
            process_boot_epoch: self.boot,
            expires_at_ms: (attempt.expires as u64).into(),
            next_unsent: next,
        };
        let mut grants = self
            .continuations
            .lock()
            .map_err(|_| WorkbenchError::StoreUnavailable)?;
        grants.retain(|_, grant| grant.strong_count() > 0);
        if grants.len() >= 4 && !grants.contains_key(&parent) {
            return Err(WorkbenchError::Limit);
        }
        let version=self.write(|c|{
            let tx=c.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql_error)?;
            if current_epoch(&tx,self.store.owner_id())?!=attempt.journal_epoch{return Err(WorkbenchError::AttemptExpired)}
            let operation=read_operation(&tx,self.store.owner_id(),scope,parent)?;
            if operation.version.value()!=expected{return Err(WorkbenchError::IdempotencyConflict)}
            if !matches!(operation.state,OperationState::Partial|OperationState::Interrupted|OperationState::Cancelled|OperationState::OutcomeUnknown){return Err(WorkbenchError::TargetBusy)}
            let unsafe_child:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND parent_operation_id=?4 AND state IN('accepted','dispatching','running','commit_requested','outcome_unknown'))",params![self.store.owner_id(),scope.workspace_id.to_string(),scope.database_target_id.to_string(),parent.to_string()],|row|row.get(0)).map_err(sql_error)?;
            if unsafe_child{return Err(WorkbenchError::OutcomeUnknown)}
            let raw:Option<String>=tx.query_row("SELECT movement_metadata_json FROM operations WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4",params![self.store.owner_id(),scope.workspace_id.to_string(),scope.database_target_id.to_string(),parent.to_string()],|row|row.get(0)).map_err(sql_error)?;
            let mut metadata:MovementMetadata=serde_json::from_str(raw.as_deref().ok_or(WorkbenchError::StoreUnavailable)?).map_err(|_|WorkbenchError::StoreUnavailable)?;
            if !continuation_basis_matches(&metadata,new_basis){return Err(WorkbenchError::BindingStale)}
            let MovementMetadata::Import{latest_continuation,progress,..}=&mut metadata else{return Err(WorkbenchError::InvalidInput)};
            *latest_continuation=Some(marker.clone());progress.next_batch_index=Some(marker.next_unsent.batch_index);
            let encoded=serde_json::to_string(&metadata).map_err(|_|WorkbenchError::InvalidInput)?;if encoded.len()>64*1024{return Err(WorkbenchError::Limit)}
            let changed=tx.execute("UPDATE operations SET movement_metadata_json=?6,state='running',transaction_outcome='none',result_completeness='partial',terminal_at_ms=NULL,version=version+1,updated_at_ms=MAX(updated_at_ms,?7) WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4 AND version=?5",params![self.store.owner_id(),scope.workspace_id.to_string(),scope.database_target_id.to_string(),parent.to_string(),signed(expected)?,encoded,now]).map_err(sql_error)?;
            if changed!=1{return Err(WorkbenchError::IdempotencyConflict)}tx.commit().map_err(sql_error)?;expected.checked_add(1).ok_or(WorkbenchError::Limit)
        })?;
        let grant = Arc::new(NativeContinuationGrant {
            marker,
            owner: self.store.owner_id().into(),
            logon: ctx.principal().logon_session_id(),
            parent,
            scope: scope.clone(),
        });
        grants.insert(parent, Arc::downgrade(&grant));
        drop(grants);
        self.attempts
            .lock()
            .map_err(|_| WorkbenchError::StoreUnavailable)?
            .remove(&nonce);
        Ok((
            Admission {
                operation_id: parent,
                version: version.into(),
                deduplicated: false,
            },
            Some(grant),
        ))
    }
}
pub(crate) fn continuation_basis_matches(old: &MovementMetadata, new: &MovementMetadata) -> bool {
    let (
        MovementMetadata::Import {
            source_digest: a,
            format: af,
            target_basis: ab,
            mapping: am,
            total_bytes: abytes,
            total_rows: arows,
            ..
        },
        MovementMetadata::Import {
            source_digest: b,
            format: bf,
            target_basis: bb,
            mapping: bm,
            total_bytes: bbytes,
            total_rows: brows,
            ..
        },
    ) = (old, new)
    else {
        return false;
    };
    let identity = match (&ab.actual_database_identity, &bb.actual_database_identity) {
        (Some(a), Some(b)) if a == b && a.starts_with("sqlite-file-object-id:") => true,
        (Some(a), Some(b)) if a == b && a.starts_with("postgresql-database-oid:") => {
            ab.server_identity.is_some() && ab.server_identity == bb.server_identity
        }
        _ => false,
    };
    identity
        && a == b
        && af == bf
        && abytes == bbytes
        && arows == brows
        && ab.schema_name == bb.schema_name
        && ab.table_name == bb.table_name
        && ab.schema_version == bb.schema_version
        && ab.column_basis_digest == bb.column_basis_digest
        && am.len() == bm.len()
        && am.iter().zip(bm).all(|(a, b)| {
            a.source_key == b.source_key
                && a.source_ordinal == b.source_ordinal
                && a.target_ordinal == b.target_ordinal
        })
}
