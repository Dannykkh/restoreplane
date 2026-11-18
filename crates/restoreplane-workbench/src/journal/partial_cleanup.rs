//! Explicit owner-bound historical partial cleanup. File existence/name alone
//! is never completion or deletion proof; native handle deletion precedes CAS.
use super::*;
use restoreplane_domain::workbench_movement::{FileArtifactState, MovementMetadata};
pub struct RecordedPartial {
    pub path: std::path::PathBuf,
    pub file_identity: String,
    pub version: u64,
}
impl OperationJournal {
    pub fn recorded_partial(
        &self,
        ctx: &InvocationContext,
        scope: &Scope,
        id: Uuid,
    ) -> Result<Option<RecordedPartial>, WorkbenchError> {
        self.context(ctx)?;
        if !self.write_enabled() {
            return Err(WorkbenchError::StoreUnavailable);
        }
        let operation = self.get(ctx, scope, id)?;
        if operation.kind != OperationKind::ExportJob {
            return Err(WorkbenchError::InvalidInput);
        }
        if !operation.state.is_terminal() {
            return Err(WorkbenchError::TargetBusy);
        }
        let Some(MovementMetadata::Export {
            partial_path,
            file_identity,
            file,
            ..
        }) = self.movement_metadata(ctx, scope, id)?
        else {
            return Ok(None);
        };
        if file.finalized || file.state == FileArtifactState::Complete {
            return Err(WorkbenchError::Forbidden);
        }
        if matches!(
            file.state,
            FileArtifactState::NotStarted | FileArtifactState::Removed
        ) {
            return Ok(None);
        }
        let (Some(path), Some(file_identity)) = (partial_path, file_identity) else {
            return Err(WorkbenchError::Unsupported);
        };
        Ok(Some(RecordedPartial {
            path: path.into(),
            file_identity,
            version: operation.version.value(),
        }))
    }
    /// Call only after remove_recorded_partial returned true for this exact
    /// identity. This method never removes a file, and never resets write gates.
    pub fn mark_partial_removed(
        &self,
        ctx: &InvocationContext,
        scope: &Scope,
        id: Uuid,
        expected: u64,
        identity: &str,
    ) -> Result<Operation, WorkbenchError> {
        self.context(ctx)?;
        let now = self.clock.now_ms()?;
        self.write(|connection|{
            let tx=connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql_error)?;
            let operation=read_operation(&tx,self.store.owner_id(),scope,id)?;
            if operation.kind!=OperationKind::ExportJob||!operation.state.is_terminal(){return Err(WorkbenchError::TargetBusy)}
            if operation.version.value()!=expected{return Err(WorkbenchError::IdempotencyConflict)}
            let raw:Option<String>=tx.query_row("SELECT movement_metadata_json FROM operations WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4",params![self.store.owner_id(),scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string()],|row|row.get(0)).map_err(sql_error)?;
            let mut metadata:MovementMetadata=serde_json::from_str(raw.as_deref().ok_or(WorkbenchError::StoreUnavailable)?).map_err(|_|WorkbenchError::StoreUnavailable)?;
            let MovementMetadata::Export{partial_path,file_identity,file,..}=&mut metadata else{return Err(WorkbenchError::InvalidInput)};
            if file_identity.as_deref()!=Some(identity)||file.finalized||file.state==FileArtifactState::Complete{return Err(WorkbenchError::Forbidden)}
            file.state=FileArtifactState::Removed;file.finalized=false;file.partial_display_path=None;*partial_path=None;
            let encoded=serde_json::to_string(&metadata).map_err(|_|WorkbenchError::InvalidInput)?;if encoded.len()>64*1024{return Err(WorkbenchError::Limit)}
            let changed=tx.execute("UPDATE operations SET movement_metadata_json=?6,version=version+1,updated_at_ms=MAX(updated_at_ms,?7) WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4 AND version=?5",params![self.store.owner_id(),scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string(),signed(expected)?,encoded,now]).map_err(sql_error)?;
            if changed!=1{return Err(WorkbenchError::IdempotencyConflict)}tx.commit().map_err(sql_error)
        })?;
        self.get(ctx, scope, id)
    }
}
