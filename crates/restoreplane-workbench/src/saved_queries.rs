//! Explicit named saves only. SQL and names are encrypted before any SQLite bind.
use crate::store::ClientStore;
use crate::types::now_ms;
use crate::{InvocationContext, WorkbenchError, WorkbenchState};
use restoreplane_domain::workbench::{AuthorizePurpose, Scope};
use restoreplane_domain::workbench_metadata::{Completeness, Count, Page, PageInput};
use restoreplane_domain::workbench_query::{
    SavedQuery, SavedQueryPut, SavedQuerySummary, SavedQueryVersion,
};
use rusqlite::{params, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};
const MAX_CONTENT: usize = 200 * 1024;
const MAX_CIPHERTEXT: usize = 262_144;
struct Cursor {
    window: Uuid,
    scope: Scope,
    updated: i64,
    id: String,
    created: Instant,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Content {
    owner: String,
    scope: Scope,
    id: Uuid,
    name: String,
    sql: String,
}
impl Drop for Content {
    fn drop(&mut self) {
        self.name.zeroize();
        self.sql.zeroize();
    }
}
pub struct SavedQueries {
    runtime: Arc<WorkbenchState>,
    store: Arc<ClientStore>,
    cursors: Mutex<HashMap<Uuid, Cursor>>,
}
impl SavedQueries {
    pub fn new(runtime: Arc<WorkbenchState>, store: Arc<ClientStore>) -> Self {
        Self {
            runtime,
            store,
            cursors: Mutex::new(HashMap::new()),
        }
    }
    fn context(&self, context: &InvocationContext) -> Result<(), WorkbenchError> {
        context.principal().ensure_standard()?;
        if context.principal() != self.runtime.principal()
            || context.principal().owner_id() != self.store.owner_id()
        {
            return Err(WorkbenchError::Forbidden);
        }
        Ok(())
    }
    pub async fn put(
        self: &Arc<Self>,
        context: &InvocationContext,
        input: SavedQueryPut,
    ) -> Result<SavedQueryVersion, WorkbenchError> {
        self.context(context)?;
        if input.name.trim().is_empty()
            || input.name.len() > 256
            || input.sql.len() > MAX_CONTENT
            || input.query_id.is_some() != input.expected_version.is_some()
            || input.query_id.is_some_and(|id| id.is_nil())
        {
            return Err(WorkbenchError::InvalidInput);
        }
        let receipt = self
            .runtime
            .authorize_scope(context, &input.scope, AuthorizePurpose::Read)
            .await?;
        let this = self.clone();
        tokio::task::spawn_blocking(move||{
            let id=input.query_id.unwrap_or_else(Uuid::new_v4);
            let content=Content {owner:this.store.owner_id().into(),scope:input.scope.clone(),id,name:input.name,sql:input.sql};
            let plaintext=Zeroizing::new(serde_json::to_vec(&content).map_err(|_|WorkbenchError::InvalidInput)?);
            let encrypted=restoreplane_secrets::protect_current_user(&plaintext,&purpose(this.store.owner_id(),&input.scope,id)).map_err(|_|WorkbenchError::Io)?;
            if encrypted.len()>MAX_CIPHERTEXT {return Err(WorkbenchError::Limit);}
            let now=i64::try_from(now_ms()).map_err(|_|WorkbenchError::Limit)?;
            crate::journal::validate_receipt(&receipt,now)?;
            let expected=input.expected_version.map(|value|i64::try_from(value.value()).map_err(|_|WorkbenchError::InvalidInput)).transpose()?;
            if expected.is_some_and(|value|value<1||value==i64::MAX){return Err(WorkbenchError::InvalidInput);}
            this.store.write(|connection|{
                let tx=connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql_error)?;
                if input.query_id.is_none(){let count:i64=tx.query_row("SELECT COUNT(*) FROM saved_queries WHERE owner_id=?1",[this.store.owner_id()],|row|row.get(0)).map_err(sql_error)?;if count>=1000{return Err(WorkbenchError::Limit);}}
                let bytes=["client-state.sqlite3","client-state.sqlite3-wal"].into_iter().try_fold(0u64,|total,name|match std::fs::metadata(this.store.root().join(name)){Ok(meta)=>total.checked_add(meta.len()).ok_or(WorkbenchError::Limit),Err(error) if error.kind()==std::io::ErrorKind::NotFound=>Ok(total),Err(_)=>Err(WorkbenchError::StoreUnavailable)})?;
                if bytes.saturating_add((encrypted.len()*2+32*1024) as u64)>128*1024*1024{return Err(WorkbenchError::Limit);}
                let binding=crate::journal::ensure_binding(&tx,this.store.owner_id(),&receipt,None,now)?;
                let version=if let Some(expected)=expected {
                    let changed=tx.execute("UPDATE saved_queries SET encrypted_content=?6,binding_id=?7,version=version+1,updated_at_ms=MAX(updated_at_ms,?8) WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND query_id=?4 AND version=?5",params![this.store.owner_id(),input.scope.workspace_id.as_str(),input.scope.database_target_id.as_str(),id.to_string(),expected,encrypted,binding,now]).map_err(sql_error)?;
                    if changed!=1{return Err(WorkbenchError::IdempotencyConflict);}expected+1
                }else{
                    tx.execute("INSERT INTO saved_queries(owner_id,workspace_id,target_id,binding_id,query_id,encryption_format,encrypted_content,version,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?5,'current-user-dpapi-v1',?6,1,?7,?7)",params![this.store.owner_id(),input.scope.workspace_id.as_str(),input.scope.database_target_id.as_str(),binding,id.to_string(),encrypted,now]).map_err(sql_error)?;1
                };
                tx.commit().map_err(sql_error)?;Ok(SavedQueryVersion {query_id:id,version:(version as u64).into()})
            })
        }).await.map_err(|_|WorkbenchError::Io)?
    }
    pub async fn get(
        self: &Arc<Self>,
        context: &InvocationContext,
        id: Uuid,
    ) -> Result<SavedQuery, WorkbenchError> {
        self.context(context)?;
        let this = self.clone();
        let (scope, encrypted, version) = tokio::task::spawn_blocking(move || this.load(id))
            .await
            .map_err(|_| WorkbenchError::Io)??;
        self.runtime
            .authorize_scope(context, &scope, AuthorizePurpose::Read)
            .await?;
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let content = decrypt(this.store.owner_id(), &scope, id, &encrypted)?;
            Ok(SavedQuery {
                query_id: id,
                name: content.name.clone(),
                sql: content.sql.clone(),
                version: (version as u64).into(),
            })
        })
        .await
        .map_err(|_| WorkbenchError::Io)?
    }
    fn load(&self, id: Uuid) -> Result<(Scope, Vec<u8>, i64), WorkbenchError> {
        if id.is_nil() {
            return Err(WorkbenchError::InvalidInput);
        }
        self.store.read(|connection|{
            let mut query=connection.prepare("SELECT q.workspace_id,q.target_id,b.connection_profile_id,q.encrypted_content,q.version FROM saved_queries q JOIN target_bindings b USING(owner_id,workspace_id,target_id,binding_id) WHERE q.owner_id=?1 AND q.query_id=?2 LIMIT 2").map_err(sql_error)?;
            let mut rows=query.query(params![self.store.owner_id(),id.to_string()]).map_err(sql_error)?;
            let row=rows.next().map_err(sql_error)?.ok_or(WorkbenchError::ResultExpired)?;
            let workspace:String=row.get(0).map_err(sql_error)?;let target:String=row.get(1).map_err(sql_error)?;let profile:String=row.get(2).map_err(sql_error)?;
            let scope=Scope {workspace_id:workspace.parse().map_err(|_|WorkbenchError::StoreUnavailable)?,database_target_id:target.parse().map_err(|_|WorkbenchError::StoreUnavailable)?,connection_profile_id:profile.parse().map_err(|_|WorkbenchError::StoreUnavailable)?};
            let encrypted:Vec<u8>=row.get(3).map_err(sql_error)?;let version:i64=row.get(4).map_err(sql_error)?;
            if encrypted.len()>MAX_CIPHERTEXT||rows.next().map_err(sql_error)?.is_some(){return Err(WorkbenchError::Forbidden);}
            Ok((scope,encrypted,version))
        })
    }
    pub async fn delete(
        self: &Arc<Self>,
        context: &InvocationContext,
        id: Uuid,
        expected: u64,
    ) -> Result<bool, WorkbenchError> {
        self.context(context)?;
        let this = self.clone();
        let (scope, _, version) = tokio::task::spawn_blocking(move || this.load(id))
            .await
            .map_err(|_| WorkbenchError::Io)??;
        if u64::try_from(version).ok() != Some(expected) {
            return Err(WorkbenchError::IdempotencyConflict);
        }
        self.runtime
            .authorize_scope(context, &scope, AuthorizePurpose::Read)
            .await?;
        let this = self.clone();
        tokio::task::spawn_blocking(move||this.store.write(|connection|{let changed=connection.execute("DELETE FROM saved_queries WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND query_id=?4 AND version=?5",params![this.store.owner_id(),scope.workspace_id.as_str(),scope.database_target_id.as_str(),id.to_string(),version]).map_err(sql_error)?;if changed!=1{return Err(WorkbenchError::IdempotencyConflict);}Ok(true)})).await.map_err(|_|WorkbenchError::Io)?
    }
    pub async fn list(
        self: &Arc<Self>,
        context: &InvocationContext,
        scope: Scope,
        page: PageInput,
    ) -> Result<Page<SavedQuerySummary>, WorkbenchError> {
        self.context(context)?;
        self.runtime
            .authorize_scope(context, &scope, AuthorizePurpose::Read)
            .await?;
        let limit = if page.limit == 0 { 100 } else { page.limit };
        if limit > 200 {
            return Err(WorkbenchError::Limit);
        }
        let (updated, id) = if let Some(cursor) = page.cursor {
            let id = Uuid::parse_str(&cursor).map_err(|_| WorkbenchError::CursorInvalid)?;
            let cursors = self.cursors.lock().map_err(|_| WorkbenchError::Io)?;
            let cursor = cursors.get(&id).ok_or(WorkbenchError::CursorInvalid)?;
            if cursor.window != context.window_instance_id()
                || cursor.scope != scope
                || cursor.created.elapsed() > Duration::from_secs(120)
            {
                return Err(WorkbenchError::CursorInvalid);
            }
            (cursor.updated, cursor.id.clone())
        } else {
            (i64::MAX, String::new())
        };
        let this = self.clone();
        let native_scope = scope.clone();
        let (items,last,more)=tokio::task::spawn_blocking(move||this.store.read(|connection|{
            let mut query=connection.prepare("SELECT query_id,encrypted_content,updated_at_ms FROM saved_queries WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND (updated_at_ms<?4 OR (updated_at_ms=?4 AND query_id>?5)) ORDER BY updated_at_ms DESC,query_id ASC LIMIT ?6").map_err(sql_error)?;
            let mut rows=query.query(params![this.store.owner_id(),native_scope.workspace_id.as_str(),native_scope.database_target_id.as_str(),updated,id,(limit+1) as i64]).map_err(sql_error)?;
            let mut items=Vec::with_capacity(limit);let mut last=None;let mut more=false;
            while let Some(row)=rows.next().map_err(sql_error)? {if items.len()==limit {more=true;break;}let id:String=row.get(0).map_err(sql_error)?;let encrypted:Vec<u8>=row.get(1).map_err(sql_error)?;let updated:i64=row.get(2).map_err(sql_error)?;let uuid=Uuid::parse_str(&id).map_err(|_|WorkbenchError::StoreUnavailable)?;let content=decrypt(this.store.owner_id(),&native_scope,uuid,&encrypted)?;items.push(SavedQuerySummary {query_id:uuid,name:content.name.clone()});last=Some((updated,id));}
            Ok((items,last,more))
        })).await.map_err(|_|WorkbenchError::Io)??;
        let next = if more {
            let (updated, id) = last.ok_or(WorkbenchError::CursorInvalid)?;
            let mut cursors = self.cursors.lock().map_err(|_| WorkbenchError::Io)?;
            cursors.retain(|_, value| value.created.elapsed() < Duration::from_secs(120));
            if cursors.len() >= 1024 {
                return Err(WorkbenchError::Limit);
            }
            let token = Uuid::new_v4();
            cursors.insert(
                token,
                Cursor {
                    window: context.window_instance_id(),
                    scope,
                    updated,
                    id,
                    created: Instant::now(),
                },
            );
            Some(token.to_string())
        } else {
            None
        };
        Ok(Page {
            items,
            next_cursor: next,
            completeness: if more {
                Completeness::Limited
            } else {
                Completeness::Complete
            },
            count: Count::Unknown,
            as_of_ms: now_ms().into(),
        })
    }
    pub fn retire_window(&self, context: &InvocationContext) -> Result<(), WorkbenchError> {
        self.context(context)?;
        self.cursors
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .retain(|_, cursor| cursor.window != context.window_instance_id());
        Ok(())
    }
}
fn purpose(owner: &str, scope: &Scope, id: Uuid) -> String {
    format!(
        "restoreplane.workbench.saved-query.v1.{}",
        blake3::hash(
            format!(
                "{owner}:{}:{}:{}:{id}",
                scope.workspace_id, scope.connection_profile_id, scope.database_target_id
            )
            .as_bytes()
        )
        .to_hex()
    )
}
fn decrypt(
    owner: &str,
    scope: &Scope,
    id: Uuid,
    encrypted: &[u8],
) -> Result<Content, WorkbenchError> {
    if encrypted.is_empty() || encrypted.len() > MAX_CIPHERTEXT {
        return Err(WorkbenchError::StoreUnavailable);
    }
    let plaintext =
        restoreplane_secrets::unprotect_current_user(encrypted, &purpose(owner, scope, id))
            .map_err(|_| WorkbenchError::Forbidden)?;
    let content: Content =
        serde_json::from_slice(&plaintext).map_err(|_| WorkbenchError::StoreUnavailable)?;
    if content.owner != owner
        || content.scope != *scope
        || content.id != id
        || content.name.len() > 256
        || content.sql.len() > MAX_CONTENT
    {
        return Err(WorkbenchError::Forbidden);
    }
    Ok(content)
}
fn sql_error(_: rusqlite::Error) -> WorkbenchError {
    WorkbenchError::StoreUnavailable
}
