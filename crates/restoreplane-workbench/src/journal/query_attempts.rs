//! Query-specific compact MAC claims. Only hashes and native context are bound;
//! SQL/review caches are neither persisted nor required to read an admitted run.
use super::*;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};

struct QueryClaims {
    scope: Scope,
    nonce: Uuid,
    expires: i64,
    resolved: String,
    public: String,
    header: String,
    mac: blake3::Hash,
}
impl QueryClaims {
    fn parse(token: &str) -> Result<Self, WorkbenchError> {
        if token.len() > 256 {
            return Err(WorkbenchError::AttemptExpired);
        }
        let (header, mac) = token
            .rsplit_once('.')
            .ok_or(WorkbenchError::AttemptExpired)?;
        let fields = header.split('.').collect::<Vec<_>>();
        if fields.len() != 6 || fields[0] != "q1" {
            return Err(WorkbenchError::AttemptExpired);
        }
        let scope = decode_scope(fields[1])?;
        let nonce = Uuid::from_bytes(decode_fixed::<16>(fields[2])?);
        let expires = i64::from_be_bytes(decode_fixed::<8>(fields[3])?);
        if nonce.is_nil() || expires < 0 {
            return Err(WorkbenchError::AttemptExpired);
        }
        Ok(Self {
            scope,
            nonce,
            expires,
            resolved: blake3::Hash::from_bytes(decode_fixed::<32>(fields[4])?)
                .to_hex()
                .to_string(),
            public: blake3::Hash::from_bytes(decode_fixed::<32>(fields[5])?)
                .to_hex()
                .to_string(),
            header: header.into(),
            mac: blake3::Hash::from_bytes(decode_fixed::<32>(mac)?),
        })
    }
    fn time(&self, now: i64) -> Result<(), WorkbenchError> {
        let issued = self
            .expires
            .checked_sub(TOKEN_MS)
            .ok_or(WorkbenchError::AttemptExpired)?;
        if issued < 0 || now < issued || now >= self.expires {
            return Err(WorkbenchError::AttemptExpired);
        }
        Ok(())
    }
}
fn decode_fixed<const N: usize>(value: &str) -> Result<[u8; N], WorkbenchError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| WorkbenchError::AttemptExpired)?;
    if URL_SAFE_NO_PAD.encode(&bytes) != value {
        return Err(WorkbenchError::AttemptExpired);
    }
    bytes.try_into().map_err(|_| WorkbenchError::AttemptExpired)
}
fn encode_scope(scope: &Scope) -> Result<String, WorkbenchError> {
    let mut bytes = Vec::with_capacity(48);
    for value in [
        scope.workspace_id.as_str(),
        scope.connection_profile_id.as_str(),
        scope.database_target_id.as_str(),
    ] {
        bytes.extend_from_slice(uuid(value)?.as_bytes());
    }
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}
fn decode_scope(value: &str) -> Result<Scope, WorkbenchError> {
    let bytes = decode_fixed::<48>(value)?;
    let part = |start: usize| {
        Uuid::from_slice(&bytes[start..start + 16])
            .map(|id| id.to_string())
            .map_err(|_| WorkbenchError::AttemptExpired)
    };
    Ok(Scope {
        workspace_id: part(0)?
            .parse()
            .map_err(|_| WorkbenchError::AttemptExpired)?,
        connection_profile_id: part(16)?
            .parse()
            .map_err(|_| WorkbenchError::AttemptExpired)?,
        database_target_id: part(32)?
            .parse()
            .map_err(|_| WorkbenchError::AttemptExpired)?,
    })
}
struct QueryBasis {
    scope: Scope,
    revision: String,
    authority: String,
    epoch: String,
    session: SessionRef,
}
impl OperationJournal {
    fn query_mac(
        &self,
        context: &InvocationContext,
        basis: &QueryBasis,
        header: &str,
        resolved: &str,
    ) -> Result<blake3::Hash, WorkbenchError> {
        let digest = payload_digest(
            "native.query.mac.v1",
            &json!({"header":header,"owner":context.principal().owner_id(),"logon":context.principal().logon_session_id().to_string(),"window":context.window_instance_id(),"boot":self.boot,"journalEpoch":basis.epoch,"scope":basis.scope,"revision":basis.revision,"authority":basis.authority,"session":basis.session,"resolvedDigest":resolved}),
        )?;
        Ok(blake3::keyed_hash(&self.commit_mac_key, digest.as_bytes()))
    }
    /// A fixed 234-byte opaque token fits both the existing 256-byte durable key
    /// and the 512-byte wire cap. Expanded native intent remains the row digest.
    pub fn prepare_query_attempt(
        &self,
        context: &InvocationContext,
        receipt: &BindingReceipt,
        session: SessionRef,
        resolved: &Value,
        public: &Value,
    ) -> Result<PreparedAttempt, WorkbenchError> {
        let prepared = self.prepare(context, receipt, Some(session), "query.run", resolved)?;
        let nonce = uuid(&prepared.attempt_token)?;
        let attempt = self
            .attempts
            .lock()
            .map_err(|_| WorkbenchError::StoreUnavailable)?
            .get(&nonce)
            .cloned()
            .ok_or(WorkbenchError::AttemptExpired)?;
        let wire = payload_digest("query.run", public)?;
        let resolved_hash = blake3::Hash::from_hex(&prepared.payload_digest)
            .map_err(|_| WorkbenchError::InvalidInput)?;
        let public_hash =
            blake3::Hash::from_hex(&wire).map_err(|_| WorkbenchError::InvalidInput)?;
        let header = format!(
            "q1.{}.{}.{}.{}.{}",
            encode_scope(&attempt.scope)?,
            URL_SAFE_NO_PAD.encode(nonce.as_bytes()),
            URL_SAFE_NO_PAD.encode(attempt.expires.to_be_bytes()),
            URL_SAFE_NO_PAD.encode(resolved_hash.as_bytes()),
            URL_SAFE_NO_PAD.encode(public_hash.as_bytes())
        );
        let basis = QueryBasis {
            scope: attempt.scope,
            revision: attempt.descriptor_revision,
            authority: attempt.authority_epoch,
            epoch: attempt.journal_epoch,
            session,
        };
        let mac = self.query_mac(context, &basis, &header, &prepared.payload_digest)?;
        let token = format!("{header}.{}", URL_SAFE_NO_PAD.encode(mac.as_bytes()));
        if token.len() > 256 {
            return Err(WorkbenchError::Limit);
        }
        Ok(PreparedAttempt {
            attempt_token: token,
            ..prepared
        })
    }
    pub(super) fn query_admission_nonce(
        &self,
        context: &InvocationContext,
        receipt: &BindingReceipt,
        session: SessionRef,
        resolved: &str,
        token: &str,
    ) -> Result<Uuid, WorkbenchError> {
        let claims = QueryClaims::parse(token)?;
        claims.time(self.clock.now_ms()?)?;
        if claims.scope != receipt.scope {
            return Err(WorkbenchError::Forbidden);
        }
        if claims.resolved != resolved {
            return Err(WorkbenchError::IdempotencyConflict);
        }
        let epoch = self
            .store
            .read(|c| current_epoch(c, self.store.owner_id()))?;
        let basis = QueryBasis {
            scope: receipt.scope.clone(),
            revision: receipt.descriptor_revision.clone(),
            authority: receipt.authority_epoch.clone(),
            epoch,
            session,
        };
        if self.query_mac(context, &basis, &claims.header, &claims.resolved)? != claims.mac {
            return Err(WorkbenchError::IdempotencyConflict);
        }
        Ok(claims.nonce)
    }
    /// Status-only duplicate resolution after transient review/run cache loss.
    /// A missing durable row still needs a live native issued pending attempt.
    pub fn query_status(
        &self,
        context: &InvocationContext,
        token: &str,
        public: &Value,
        session: SessionRef,
    ) -> Result<Option<(Admission, Scope, u64)>, WorkbenchError> {
        self.context(context)?;
        let claims = QueryClaims::parse(token)?;
        claims.time(self.clock.now_ms()?)?;
        if payload_digest("query.run", public)? != claims.public {
            return Err(WorkbenchError::IdempotencyConflict);
        }
        for _ in 0..2 {
            if let Some(row) = self
                .store
                .read(|c| read_query_row(c, self.store.owner_id(), &claims.scope, token))?
            {
                let epoch = self
                    .store
                    .read(|c| current_epoch(c, self.store.owner_id()))?;
                if row.basis.scope != claims.scope
                    || row.window != context.window_instance_id().to_string()
                    || row.boot != self.boot.to_string()
                    || row.basis.epoch != epoch
                    || row.basis.session != session
                    || row.expires != claims.expires
                {
                    return Err(WorkbenchError::Forbidden);
                }
                if row.digest != claims.resolved
                    || self.query_mac(context, &row.basis, &claims.header, &claims.resolved)?
                        != claims.mac
                {
                    return Err(WorkbenchError::IdempotencyConflict);
                }
                let operation = self.get(context, &row.basis.scope, row.id)?;
                return Ok(Some((
                    Admission {
                        operation_id: row.id,
                        version: operation.version,
                        deduplicated: true,
                    },
                    row.basis.scope,
                    row.execution_epoch,
                )));
            }
            let pending = self
                .attempts
                .lock()
                .map_err(|_| WorkbenchError::StoreUnavailable)?
                .get(&claims.nonce)
                .cloned();
            if let Some(attempt) = pending {
                let epoch = self
                    .store
                    .read(|c| current_epoch(c, self.store.owner_id()))?;
                if attempt.scope != claims.scope
                    || attempt.owner != self.store.owner_id()
                    || attempt.window != context.window_instance_id()
                    || attempt.logon_session_id != context.principal().logon_session_id()
                    || attempt.session != Some(session)
                    || attempt.journal_epoch != epoch
                {
                    return Err(WorkbenchError::Forbidden);
                }
                if attempt.command != "query.run"
                    || attempt.digest != claims.resolved
                    || attempt.expires != claims.expires
                {
                    return Err(WorkbenchError::IdempotencyConflict);
                }
                let basis = QueryBasis {
                    scope: attempt.scope,
                    revision: attempt.descriptor_revision,
                    authority: attempt.authority_epoch,
                    epoch,
                    session,
                };
                if self.query_mac(context, &basis, &claims.header, &claims.resolved)? != claims.mac
                {
                    return Err(WorkbenchError::IdempotencyConflict);
                }
                return Ok(None);
            }
            // Another caller may have committed then evicted pending memory
            // between our first read and memory lookup; retry the row once.
        }
        Err(WorkbenchError::AttemptExpired)
    }
    pub fn lookup_admitted(
        &self,
        context: &InvocationContext,
        token: &str,
        command: &str,
        payload: &Value,
        receipt: &BindingReceipt,
        spec: &AdmissionSpec,
    ) -> Result<Option<Admission>, WorkbenchError> {
        self.context(context)?;
        validate_spec(command, spec, self.limits.statements_per_operation)?;
        let digest = payload_digest(command, payload)?;
        let now = self.clock.now_ms()?;
        if command == "query.run" && token.starts_with("q1.") {
            self.query_admission_nonce(
                context,
                receipt,
                spec.session.ok_or(WorkbenchError::InvalidInput)?,
                &digest,
                token,
            )?;
        }
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
        self.store.read(|c| existing_admission(c, &lookup))
    }
    pub fn execution_epoch(
        &self,
        context: &InvocationContext,
        scope: &Scope,
        id: Uuid,
    ) -> Result<u64, WorkbenchError> {
        self.context(context)?;
        self.store.read(|c|{
            let value:Option<i64>=c.query_row("SELECT execution_epoch FROM operations WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4 AND window_instance_id=?5 AND process_boot_epoch=?6",params![self.store.owner_id(),scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string(),context.window_instance_id().to_string(),self.boot.to_string()],|r|r.get(0)).optional().map_err(sql_error)?;
            u64::try_from(value.ok_or(WorkbenchError::Forbidden)?).map_err(|_|WorkbenchError::StoreUnavailable)
        })
    }
    pub fn bind_execution_epoch(
        &self,
        context: &InvocationContext,
        scope: &Scope,
        id: Uuid,
        expected: u64,
        epoch: u64,
    ) -> Result<Operation, WorkbenchError> {
        self.context(context)?;
        if epoch == 0 {
            return Err(WorkbenchError::InvalidInput);
        }
        let now = self.clock.now_ms()?;
        self.write(|c|{
            let tx=c.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql_error)?;
            let operation=read_operation(&tx,self.store.owner_id(),scope,id)?;
            if operation.kind!=OperationKind::Query||operation.state!=OperationState::Accepted||operation.version.value()!=expected{return Err(WorkbenchError::IdempotencyConflict)}
            let bound:Option<i64>=tx.query_row("SELECT execution_epoch FROM operations WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4 AND dispatch_count=0 AND window_instance_id=?5 AND process_boot_epoch=?6",params![self.store.owner_id(),scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string(),context.window_instance_id().to_string(),self.boot.to_string()],|r|r.get(0)).optional().map_err(sql_error)?;
            let old=u64::try_from(bound.ok_or(WorkbenchError::Forbidden)?).map_err(|_|WorkbenchError::StoreUnavailable)?;
            if old!=0&&old!=epoch{return Err(WorkbenchError::IdempotencyConflict)}
            if old==0 {let affected=tx.execute("UPDATE operations SET execution_epoch=?6,version=version+1,updated_at_ms=MAX(updated_at_ms,?7) WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4 AND version=?5 AND dispatch_count=0 AND execution_epoch=0",params![self.store.owner_id(),scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string(),signed(expected)?,signed(epoch)?,now]).map_err(sql_error)?;if affected!=1{return Err(WorkbenchError::IdempotencyConflict)}}
            tx.commit().map_err(sql_error)
        })?;
        self.get(context, scope, id)
    }
}
struct QueryRow {
    basis: QueryBasis,
    window: String,
    boot: String,
    expires: i64,
    digest: String,
    id: Uuid,
    execution_epoch: u64,
}
struct RawQueryRow {
    workspace: String,
    target: String,
    profile: String,
    revision: String,
    authority: String,
    epoch: String,
    boot: String,
    window: String,
    session: String,
    generation: i64,
    expires: i64,
    digest: String,
    id: String,
    execution_epoch: i64,
}
fn read_query_row(
    c: &Connection,
    owner: &str,
    scope: &Scope,
    token: &str,
) -> Result<Option<QueryRow>, WorkbenchError> {
    let mut statement=c.prepare("SELECT o.workspace_id,o.target_id,b.connection_profile_id,b.descriptor_revision,b.authority_epoch,o.journal_epoch,o.process_boot_epoch,o.window_instance_id,o.session_id,o.connection_generation,o.token_expires_at_ms,o.payload_digest,o.operation_id,o.execution_epoch FROM operations o JOIN target_bindings b USING(owner_id,workspace_id,target_id,binding_id) WHERE o.owner_id=?1 AND o.workspace_id=?2 AND o.target_id=?3 AND o.kind='query' AND o.idempotency_key=?4 LIMIT 2").map_err(sql_error)?;
    let rows = statement
        .query_map(
            params![
                owner,
                scope.workspace_id.to_string(),
                scope.database_target_id.to_string(),
                token
            ],
            |r| {
                Ok(RawQueryRow {
                    workspace: r.get(0)?,
                    target: r.get(1)?,
                    profile: r.get(2)?,
                    revision: r.get(3)?,
                    authority: r.get(4)?,
                    epoch: r.get(5)?,
                    boot: r.get(6)?,
                    window: r.get(7)?,
                    session: r.get(8)?,
                    generation: r.get(9)?,
                    expires: r.get(10)?,
                    digest: r.get(11)?,
                    id: r.get(12)?,
                    execution_epoch: r.get(13)?,
                })
            },
        )
        .map_err(sql_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(sql_error)?;
    if rows.len() > 1 {
        return Err(WorkbenchError::IdempotencyConflict);
    }
    let Some(row) = rows.into_iter().next() else {
        return Ok(None);
    };
    let scope = Scope {
        workspace_id: row
            .workspace
            .parse()
            .map_err(|_| WorkbenchError::StoreUnavailable)?,
        connection_profile_id: row
            .profile
            .parse()
            .map_err(|_| WorkbenchError::StoreUnavailable)?,
        database_target_id: row
            .target
            .parse()
            .map_err(|_| WorkbenchError::StoreUnavailable)?,
    };
    Ok(Some(QueryRow {
        basis: QueryBasis {
            scope,
            revision: row.revision,
            authority: row.authority,
            epoch: row.epoch,
            session: SessionRef {
                session_id: uuid(&row.session)?,
                connection_generation: positive(row.generation)?.into(),
            },
        },
        window: row.window,
        boot: row.boot,
        expires: row.expires,
        digest: row.digest,
        id: uuid(&row.id)?,
        execution_epoch: u64::try_from(row.execution_epoch)
            .map_err(|_| WorkbenchError::StoreUnavailable)?,
    }))
}
