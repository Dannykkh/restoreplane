//! Compact native import-parent claims. A cold retry can read an admitted
//! parent, but cannot revive an expired preview or dispatch another batch.
use super::*;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};

struct ImportClaims {
    scope: Scope,
    nonce: Uuid,
    expires: i64,
    public: String,
    header: String,
    mac: blake3::Hash,
}
fn decoded<const N: usize>(value: &str) -> Result<[u8; N], WorkbenchError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| WorkbenchError::AttemptExpired)?;
    if URL_SAFE_NO_PAD.encode(&bytes) != value {
        return Err(WorkbenchError::AttemptExpired);
    }
    bytes.try_into().map_err(|_| WorkbenchError::AttemptExpired)
}
fn scope_text(scope: &Scope) -> String {
    let mut bytes = Vec::with_capacity(48);
    bytes.extend_from_slice(scope.workspace_id.as_uuid().as_bytes());
    bytes.extend_from_slice(scope.connection_profile_id.as_uuid().as_bytes());
    bytes.extend_from_slice(scope.database_target_id.as_uuid().as_bytes());
    URL_SAFE_NO_PAD.encode(bytes)
}
impl ImportClaims {
    fn parse(token: &str) -> Result<Self, WorkbenchError> {
        if token.len() > 256 {
            return Err(WorkbenchError::AttemptExpired);
        }
        let (header, mac) = token
            .rsplit_once('.')
            .ok_or(WorkbenchError::AttemptExpired)?;
        let fields = header.split('.').collect::<Vec<_>>();
        if fields.len() != 5 || fields[0] != "i1" {
            return Err(WorkbenchError::AttemptExpired);
        }
        let bytes = decoded::<48>(fields[1])?;
        let part = |start: usize| {
            Uuid::from_slice(&bytes[start..start + 16])
                .map(|id| id.to_string())
                .map_err(|_| WorkbenchError::AttemptExpired)
        };
        let scope = Scope {
            workspace_id: part(0)?
                .parse()
                .map_err(|_| WorkbenchError::AttemptExpired)?,
            connection_profile_id: part(16)?
                .parse()
                .map_err(|_| WorkbenchError::AttemptExpired)?,
            database_target_id: part(32)?
                .parse()
                .map_err(|_| WorkbenchError::AttemptExpired)?,
        };
        let nonce = Uuid::from_bytes(decoded::<16>(fields[2])?);
        let expires = i64::from_be_bytes(decoded::<8>(fields[3])?);
        if nonce.is_nil() || expires < 0 {
            return Err(WorkbenchError::AttemptExpired);
        }
        Ok(Self {
            scope,
            nonce,
            expires,
            public: blake3::Hash::from_bytes(decoded::<32>(fields[4])?)
                .to_hex()
                .to_string(),
            header: header.into(),
            mac: blake3::Hash::from_bytes(decoded::<32>(mac)?),
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
struct ImportBasis<'a> {
    scope: &'a Scope,
    revision: &'a str,
    authority: &'a str,
    epoch: &'a str,
    session: SessionRef,
}
impl OperationJournal {
    fn import_mac(
        &self,
        ctx: &InvocationContext,
        basis: ImportBasis<'_>,
        header: &str,
        resolved: &str,
    ) -> Result<blake3::Hash, WorkbenchError> {
        let digest = payload_digest(
            "native.import.parent.mac.v1",
            &json!({"header":header,"owner":ctx.principal().owner_id(),"logon":ctx.principal().logon_session_id().to_string(),"window":ctx.window_instance_id(),"boot":self.boot,"journalEpoch":basis.epoch,"scope":basis.scope,"revision":basis.revision,"authority":basis.authority,"session":basis.session,"resolvedDigest":resolved}),
        )?;
        Ok(blake3::keyed_hash(&self.commit_mac_key, digest.as_bytes()))
    }
    pub fn prepare_import_attempt(
        &self,
        ctx: &InvocationContext,
        receipt: &BindingReceipt,
        session: SessionRef,
        resolved: &Value,
        public: &Value,
    ) -> Result<PreparedAttempt, WorkbenchError> {
        let prepared = self.prepare(ctx, receipt, Some(session), "import.start", resolved)?;
        let nonce = uuid(&prepared.attempt_token)?;
        let attempt = self
            .attempts
            .lock()
            .map_err(|_| WorkbenchError::StoreUnavailable)?
            .get(&nonce)
            .cloned()
            .ok_or(WorkbenchError::AttemptExpired)?;
        let public = payload_digest("import.start", public)?;
        let public = blake3::Hash::from_hex(public).map_err(|_| WorkbenchError::InvalidInput)?;
        let header = format!(
            "i1.{}.{}.{}.{}",
            scope_text(&receipt.scope),
            URL_SAFE_NO_PAD.encode(nonce.as_bytes()),
            URL_SAFE_NO_PAD.encode(attempt.expires.to_be_bytes()),
            URL_SAFE_NO_PAD.encode(public.as_bytes())
        );
        let mac = self.import_mac(
            ctx,
            ImportBasis {
                scope: &receipt.scope,
                revision: &receipt.descriptor_revision,
                authority: &receipt.authority_epoch,
                epoch: &attempt.journal_epoch,
                session,
            },
            &header,
            &prepared.payload_digest,
        )?;
        let token = format!("{header}.{}", URL_SAFE_NO_PAD.encode(mac.as_bytes()));
        if token.len() > 256 {
            return Err(WorkbenchError::Limit);
        }
        Ok(PreparedAttempt {
            attempt_token: token,
            ..prepared
        })
    }
    pub(super) fn import_admission_nonce(
        &self,
        ctx: &InvocationContext,
        receipt: &BindingReceipt,
        session: SessionRef,
        resolved: &str,
        token: &str,
    ) -> Result<Uuid, WorkbenchError> {
        let claims = ImportClaims::parse(token)?;
        claims.time(self.clock.now_ms()?)?;
        if claims.scope != receipt.scope {
            return Err(WorkbenchError::Forbidden);
        }
        let epoch = self
            .store
            .read(|c| current_epoch(c, self.store.owner_id()))?;
        let expected = self.import_mac(
            ctx,
            ImportBasis {
                scope: &receipt.scope,
                revision: &receipt.descriptor_revision,
                authority: &receipt.authority_epoch,
                epoch: &epoch,
                session,
            },
            &claims.header,
            resolved,
        )?;
        if expected != claims.mac {
            return Err(WorkbenchError::IdempotencyConflict);
        }
        Ok(claims.nonce)
    }
    pub fn import_start_status(
        &self,
        ctx: &InvocationContext,
        token: &str,
        public: &Value,
        session: SessionRef,
    ) -> Result<Option<Admission>, WorkbenchError> {
        self.context(ctx)?;
        let claims = ImportClaims::parse(token)?;
        claims.time(self.clock.now_ms()?)?;
        if payload_digest("import.start", public)? != claims.public {
            return Err(WorkbenchError::IdempotencyConflict);
        }
        let scope = &claims.scope;
        let existing=self.store.read(|c|{
            let row=c.query_row("SELECT o.operation_id,o.payload_digest,o.version,o.window_instance_id,o.process_boot_epoch,o.journal_epoch,o.token_expires_at_ms,o.session_id,o.connection_generation,b.connection_profile_id,b.descriptor_revision,b.authority_epoch,h.journal_epoch FROM operations o JOIN target_bindings b USING(owner_id,workspace_id,target_id,binding_id) JOIN journal_header h ON h.owner_id=o.owner_id AND h.singleton=1 WHERE o.owner_id=?1 AND o.workspace_id=?2 AND o.target_id=?3 AND o.kind='import_job' AND o.idempotency_key=?4",params![self.store.owner_id(),scope.workspace_id.to_string(),scope.database_target_id.to_string(),token],|row|Ok(DurableAttempt{id:row.get(0)?,digest:row.get(1)?,version:row.get(2)?,window:row.get(3)?,boot:row.get(4)?,epoch:row.get(5)?,expires:row.get(6)?,session:row.get(7)?,generation:row.get(8)?,profile:row.get(9)?,revision:row.get(10)?,authority:row.get(11)?,current_epoch:row.get(12)?})).optional().map_err(sql_error)?;Ok(row)
        })?;
        let Some(row) = existing else { return Ok(None) };
        if row.window != ctx.window_instance_id().to_string()
            || row.profile != scope.connection_profile_id.to_string()
            || row.session.as_deref() != Some(session.session_id.to_string().as_str())
            || u64::try_from(row.generation).ok() != Some(session.connection_generation.value())
        {
            return Err(WorkbenchError::Forbidden);
        }
        if row.boot != self.boot.to_string()
            || row.epoch != row.current_epoch
            || row.expires != claims.expires
        {
            return Err(WorkbenchError::AttemptExpired);
        }
        let expected = self.import_mac(
            ctx,
            ImportBasis {
                scope,
                revision: &row.revision,
                authority: &row.authority,
                epoch: &row.epoch,
                session,
            },
            &claims.header,
            &row.digest,
        )?;
        if expected != claims.mac {
            return Err(WorkbenchError::IdempotencyConflict);
        }
        Ok(Some(Admission {
            operation_id: uuid(&row.id)?,
            version: positive(row.version)?.into(),
            deduplicated: true,
        }))
    }
}
