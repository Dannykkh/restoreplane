//! Metadata-only parent projections for native movement coordinators.
use super::*;
use restoreplane_domain::workbench_movement::{
    FileArtifactState, MovementMetadata, MovementProgress, PersistedExportSource,
};
use restoreplane_domain::workbench_stream::Completion;

impl OperationJournal {
    pub fn movement_metadata(
        &self,
        context: &InvocationContext,
        scope: &Scope,
        id: Uuid,
    ) -> Result<Option<MovementMetadata>, WorkbenchError> {
        self.context(context)?;
        let result=self.store.read(|c|{
            let raw:Option<String>=c.query_row("SELECT movement_metadata_json FROM operations WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4 AND kind IN('import_job','export_job')",params![self.store.owner_id(),scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string()],|r|r.get(0)).optional().map_err(sql_error)?.ok_or(WorkbenchError::InvalidInput)?;
            raw.map(|raw|{if raw.len()>64*1024{return Err(WorkbenchError::StoreUnavailable)}let metadata:MovementMetadata=serde_json::from_str(&raw).map_err(|_|WorkbenchError::StoreUnavailable)?;validate_metadata(&metadata,scope).map_err(|_|WorkbenchError::StoreUnavailable)?;Ok(metadata)}).transpose()
        });
        if matches!(
            result,
            Err(WorkbenchError::StoreUnavailable | WorkbenchError::Io)
        ) {
            self.disabled.store(true, Ordering::SeqCst);
        }
        let mut metadata = result?;
        if !self.write_enabled() {
            if let Some(MovementMetadata::Export { file, .. }) = &mut metadata {
                if !matches!(
                    file.state,
                    FileArtifactState::Removed | FileArtifactState::NotStarted
                ) {
                    file.state = FileArtifactState::Unconfirmed;
                    file.finalized = false;
                }
            }
        }
        Ok(metadata)
    }
    /// Initial metadata is durable while Accepted/dispatch0. Later updates keep
    /// immutable native source/destination basis; final file evidence and the
    /// operation state/version share this one local transaction.
    pub fn update_movement(
        &self,
        context: &InvocationContext,
        scope: &Scope,
        id: Uuid,
        expected: u64,
        metadata: MovementMetadata,
        next: Option<Transition>,
    ) -> Result<Operation, WorkbenchError> {
        self.context(context)?;
        validate_metadata(&metadata, scope)?;
        if matches!(&metadata,MovementMetadata::Export{file,..} if file.state==FileArtifactState::Complete)
            && !matches!(next, Some(Transition::JobCompleted))
        {
            return Err(WorkbenchError::InvalidInput);
        }
        let encoded = serde_json::to_string(&metadata).map_err(|_| WorkbenchError::InvalidInput)?;
        if encoded.len() > 64 * 1024 {
            return Err(WorkbenchError::Limit);
        }
        let now = self.clock.now_ms()?;
        let continuation = self.movement_grant(context, scope, id)?;
        self.write(|c|{
            let tx=c.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql_error)?;
            let old=read_operation(&tx,self.store.owner_id(),scope,id)?;
            if old.version.value()!=expected{return Err(WorkbenchError::IdempotencyConflict)}
            if !matches!((&metadata,old.kind),(MovementMetadata::Import{..},OperationKind::ImportJob)|(MovementMetadata::Export{..},OperationKind::ExportJob)){return Err(WorkbenchError::InvalidInput)}
            let (raw,window,boot,dispatch):(Option<String>,String,String,i64)=tx.query_row("SELECT movement_metadata_json,window_instance_id,process_boot_epoch,dispatch_count FROM operations WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4",params![self.store.owner_id(),scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).map_err(sql_error)?;
            if window!=context.window_instance_id().to_string()||boot!=self.boot.to_string(){
                if let Some(grant)=&continuation{grant.durable(&tx,self.store.owner_id())?;}else{return Err(WorkbenchError::Forbidden)}
            }
            if let Some(raw)=raw{
                let previous:MovementMetadata=serde_json::from_str(&raw).map_err(|_|WorkbenchError::StoreUnavailable)?;
                validate_metadata(&previous,scope).map_err(|_|WorkbenchError::StoreUnavailable)?;
                if immutable(&previous)?!=immutable(&metadata)?||!progress_advances(progress(&previous),progress(&metadata)){return Err(WorkbenchError::IdempotencyConflict)}
                if let (MovementMetadata::Export{file:before,file_identity:before_id,partial_path:before_path,..},MovementMetadata::Export{file:after,file_identity:after_id,partial_path:after_path,..})=(&previous,&metadata){
                    if after.bytes_written.value()<before.bytes_written.value()||after.rows_written.value()<before.rows_written.value()||before_id.as_ref().is_some_and(|id|after_id.as_ref()!=Some(id))||before_path.as_ref().zip(after_path.as_ref()).is_some_and(|(before,after)|before!=after){return Err(WorkbenchError::IdempotencyConflict)}
                }
            }else if old.state!=OperationState::Accepted||dispatch!=0{return Err(WorkbenchError::InvalidInput)}
            let unknown:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND parent_operation_id=?4 AND state='outcome_unknown')",params![self.store.owner_id(),scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string()],|r|r.get(0)).map_err(sql_error)?;
            let running=matches!(old.state,OperationState::Running|OperationState::Dispatching);
            let (state,outcome,completeness)=match next {
                None if matches!(old.state,OperationState::Accepted|OperationState::Dispatching|OperationState::Running)=>(old.state,old.transaction_outcome,old.result_completeness),
                Some(Transition::Running) if old.state==OperationState::Dispatching=>(OperationState::Running,TransactionOutcome::None,old.result_completeness),
                Some(Transition::JobCompleted) if old.state==OperationState::Running=>{
                    if unknown{return Err(WorkbenchError::OutcomeUnknown)}
                    match &metadata{
                        MovementMetadata::Export{file,source,..}=>{
                            if file.state!=FileArtifactState::Complete||!file.finalized{return Err(WorkbenchError::InvalidInput)}
                            match source{PersistedExportSource::Shown{range,..} if file.rows_written.value()==u64::from(range.row_count)=>{},PersistedExportSource::Fresh{..} if file.source_completeness==Completion::Complete=>{},_=>return Err(WorkbenchError::OutcomeUnknown)}
                        },
                        MovementMetadata::Import{total_rows,progress,..}=>{
                            let (unfinished,rows):(bool,i64)=tx.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND parent_operation_id=?4 AND state<>'committed' AND NOT(dispatch_count=0 AND state IN('cancelled','failed'))),COALESCE((SELECT SUM(source_row_end-source_row_start) FROM operations WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND parent_operation_id=?4 AND state='committed'),0)",params![self.store.owner_id(),scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string()],|r|Ok((r.get(0)?,r.get(1)?))).map_err(sql_error)?;
                            if unfinished||u64::try_from(rows).ok()!=Some(total_rows.value())||progress.confirmed_committed_rows!=*total_rows||progress.rows_processed!=*total_rows||progress.unresolved_batches.value()!=0{return Err(WorkbenchError::OutcomeUnknown)}
                        },
                    }
                    (OperationState::Completed,TransactionOutcome::None,ResultCompleteness::Complete)
                },
                Some(Transition::Partial) if running=>if unknown{(OperationState::OutcomeUnknown,TransactionOutcome::OutcomeUnknown,ResultCompleteness::Partial)}else{(OperationState::Partial,TransactionOutcome::None,ResultCompleteness::Partial)},
                Some(Transition::ReadInterrupted|Transition::EffectsUnknown) if running&&old.kind==OperationKind::ExportJob=>{
                    let MovementMetadata::Export{file,..}=&metadata else{return Err(WorkbenchError::InvalidInput)};
                    if file.finalized||file.state==FileArtifactState::Complete{return Err(WorkbenchError::InvalidInput)}
                    (match file.source_completeness{Completion::Cancelled=>OperationState::Cancelled,Completion::Failed=>OperationState::Failed,_=>OperationState::Interrupted},TransactionOutcome::None,if file.rows_written.value()>0{ResultCompleteness::Partial}else{ResultCompleteness::Interrupted})
                },
                Some(Transition::EffectsUnknown) if running&&old.kind==OperationKind::ImportJob&&unknown=>(OperationState::OutcomeUnknown,TransactionOutcome::OutcomeUnknown,ResultCompleteness::Partial),
                Some(Transition::ReadInterrupted) if running&&old.kind==OperationKind::ImportJob&&!unknown=>(OperationState::Interrupted,TransactionOutcome::None,ResultCompleteness::Interrupted),
                Some(Transition::CancelledBeforeSend) if old.state==OperationState::Accepted&&dispatch==0=>(OperationState::Cancelled,TransactionOutcome::None,ResultCompleteness::Cancelled),
                Some(Transition::FailedBeforeSend) if old.state==OperationState::Accepted&&dispatch==0=>(OperationState::Failed,TransactionOutcome::None,ResultCompleteness::None),
                _=>return Err(WorkbenchError::InvalidInput),
            };
            let changed=tx.execute("UPDATE operations SET movement_metadata_json=?6,state=?7,transaction_outcome=?8,result_completeness=?9,version=version+1,updated_at_ms=MAX(updated_at_ms,?10),terminal_at_ms=CASE WHEN ?11 THEN COALESCE(terminal_at_ms,MAX(created_at_ms,?10)) ELSE terminal_at_ms END WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4 AND version=?5",params![self.store.owner_id(),scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string(),signed(expected)?,encoded,state.as_str(),outcome.as_str(),completeness.as_str(),now,state.is_terminal()]).map_err(sql_error)?;
            if changed!=1{return Err(WorkbenchError::IdempotencyConflict)}tx.commit().map_err(sql_error)
        })?;
        self.get(context, scope, id)
    }
    /// Real movement handlers use this barrier before file creation/child wire.
    pub fn mark_movement_dispatch(
        &self,
        context: &InvocationContext,
        scope: &Scope,
        id: Uuid,
        expected: u64,
        epoch: u64,
        fresh: &BindingReceipt,
    ) -> Result<DispatchPermit, WorkbenchError> {
        let operation = self.get(context, scope, id)?;
        if operation.version.value() != expected
            || !matches!(
                operation.kind,
                OperationKind::ImportJob | OperationKind::ExportJob
            )
        {
            return Err(WorkbenchError::InvalidInput);
        }
        if self.movement_metadata(context, scope, id)?.is_none() {
            return Err(WorkbenchError::InvalidInput);
        }
        self.mark_dispatch(context, scope, id, expected, epoch, fresh)
    }
}
fn progress(metadata: &MovementMetadata) -> &MovementProgress {
    match metadata {
        MovementMetadata::Import { progress, .. } | MovementMetadata::Export { progress, .. } => {
            progress
        }
    }
}
fn progress_advances(before: &MovementProgress, after: &MovementProgress) -> bool {
    before.total_bytes == after.total_bytes
        && before.bytes_processed.value() <= after.bytes_processed.value()
        && before.rows_processed.value() <= after.rows_processed.value()
        && before.confirmed_committed_rows.value() <= after.confirmed_committed_rows.value()
        && before.confirmed_committed_batches.value() <= after.confirmed_committed_batches.value()
}
fn immutable(metadata: &MovementMetadata) -> Result<Value, WorkbenchError> {
    let mut value = serde_json::to_value(metadata).map_err(|_| WorkbenchError::InvalidInput)?;
    let object = value.as_object_mut().ok_or(WorkbenchError::InvalidInput)?;
    object.remove("progress");
    if matches!(metadata, MovementMetadata::Export { .. }) {
        for key in ["file", "partialPath", "fileIdentity"] {
            object.remove(key);
        }
    }
    Ok(value)
}
fn text_bound(value: &str, limit: usize) -> bool {
    !value.is_empty() && value.len() <= limit && !value.contains(['\0', '\r', '\n'])
}
fn validate_metadata(metadata: &MovementMetadata, scope: &Scope) -> Result<(), WorkbenchError> {
    let progress = progress(metadata);
    if progress
        .total_bytes
        .is_some_and(|total| progress.bytes_processed.value() > total.value())
        || progress.confirmed_committed_rows.value() > progress.rows_processed.value()
    {
        return Err(WorkbenchError::InvalidInput);
    }
    match metadata {
        MovementMetadata::Import {
            signature_version,
            source_digest,
            object,
            target_basis,
            mapping,
            total_bytes,
            total_rows,
            progress,
            ..
        } => {
            if *signature_version != 1
                || !digest_valid(source_digest)
                || !digest_valid(&object.schema_version)
                || !digest_valid(&target_basis.schema_version)
                || object.schema_version != target_basis.schema_version
                || !digest_valid(&target_basis.column_basis_digest)
                || !text_bound(&target_basis.schema_name, 4096)
                || !text_bound(&target_basis.table_name, 4096)
                || mapping.is_empty()
                || mapping.len() > 512
                || progress.total_bytes != Some(*total_bytes)
                || progress.rows_processed.value() > total_rows.value()
            {
                return Err(WorkbenchError::InvalidInput);
            }
            let mut sources = std::collections::BTreeSet::new();
            let mut targets = std::collections::BTreeSet::new();
            for item in mapping {
                if !text_bound(&item.source_key, 64)
                    || item.source_ordinal >= 512
                    || item.target_ordinal >= 512
                    || item.column_id.is_nil()
                    || !sources.insert(item.source_ordinal)
                    || !targets.insert(item.target_ordinal)
                {
                    return Err(WorkbenchError::InvalidInput);
                }
            }
        }
        MovementMetadata::Export {
            signature_version,
            format,
            final_path,
            partial_path,
            file_identity,
            source_digest,
            source,
            file,
            ..
        } => {
            if *signature_version != 1
                || !digest_valid(source_digest)
                || !text_bound(final_path, 32768)
                || !std::path::Path::new(final_path).is_absolute()
                || partial_path.as_ref().is_some_and(|path| {
                    !text_bound(path, 32768) || !std::path::Path::new(path).is_absolute()
                })
                || file_identity
                    .as_ref()
                    .is_some_and(|id| !text_bound(id, 512))
                || file.display_path != *final_path
                || file.partial_display_path != *partial_path
                || file.format != *format
                || file.finalized != (file.state == FileArtifactState::Complete)
            {
                return Err(WorkbenchError::InvalidInput);
            }
            if file.state == FileArtifactState::Complete
                && (file_identity.is_none() || partial_path.is_some())
            {
                return Err(WorkbenchError::InvalidInput);
            }
            match source {
                PersistedExportSource::Shown { identity, range }
                    if identity.workspace_id.as_str() == scope.workspace_id.as_str()
                        && identity.database_target_id.as_str()
                            == scope.database_target_id.as_str()
                        && range.row_count <= 200 => {}
                PersistedExportSource::Fresh { review_digest, .. }
                    if digest_valid(review_digest) => {}
                _ => return Err(WorkbenchError::InvalidInput),
            }
        }
    }
    Ok(())
}
