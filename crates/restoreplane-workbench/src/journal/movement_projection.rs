//! Owner-scoped bounded parent/child projections; polling never writes storage.
use super::*;
use restoreplane_domain::workbench_metadata::{Completeness, Count, Page, PageInput};
use restoreplane_domain::workbench_movement::{
    ImportBatchStatus, MovementMetadata, MovementProgress, MovementStatus, NextUnsent,
};

impl OperationJournal {
    pub fn movement_status(
        &self,
        ctx: &InvocationContext,
        scope: &Scope,
        id: Uuid,
        page: PageInput,
    ) -> Result<MovementStatus, WorkbenchError> {
        self.context(ctx)?;
        if page.limit == 0 || page.limit > 100 {
            return Err(WorkbenchError::Limit);
        }
        let mut operation = self.get(ctx, scope, id)?;
        if !matches!(
            operation.kind,
            OperationKind::ImportJob | OperationKind::ExportJob
        ) {
            return Err(WorkbenchError::InvalidInput);
        }
        let after = match page.cursor {
            Some(cursor) => {
                let prefix = format!("wbm1.{id}.");
                let value = cursor
                    .strip_prefix(&prefix)
                    .ok_or(WorkbenchError::CursorInvalid)?;
                let parsed = value
                    .parse::<u32>()
                    .map_err(|_| WorkbenchError::CursorInvalid)?;
                if value != parsed.to_string() {
                    return Err(WorkbenchError::CursorInvalid);
                }
                Some(parsed)
            }
            None => None,
        };
        let metadata = self.movement_metadata(ctx, scope, id)?;
        let (mut progress, file) = match metadata {
            Some(MovementMetadata::Import { progress, .. }) => (progress, None),
            Some(MovementMetadata::Export { progress, file, .. }) => (progress, Some(file)),
            None => (
                MovementProgress {
                    bytes_processed: 0.into(),
                    total_bytes: None,
                    rows_processed: 0.into(),
                    confirmed_committed_rows: 0.into(),
                    confirmed_committed_batches: 0.into(),
                    unresolved_batches: 0.into(),
                    next_batch_index: Some(0),
                },
                None,
            ),
        };
        let (mut items,child_count,committed_rows,committed_batches,unresolved,next)=self.store.read(|connection|{
            let mut statement=connection.prepare("SELECT operation_id,batch_index,state,version,source_byte_start,source_byte_end,source_row_start,source_row_end,failure_code FROM operations WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND parent_operation_id=?4 AND parent_kind='import_job' AND (?5 IS NULL OR batch_index>?5) ORDER BY batch_index LIMIT ?6").map_err(sql_error)?;
            let rows=statement.query_map(params![self.store.owner_id(),scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string(),after.map(i64::from),(page.limit+1)as i64],|row|Ok((row.get::<_,String>(0)?,row.get::<_,i64>(1)?,row.get::<_,String>(2)?,row.get::<_,i64>(3)?,row.get::<_,i64>(4)?,row.get::<_,i64>(5)?,row.get::<_,i64>(6)?,row.get::<_,i64>(7)?,row.get::<_,Option<String>>(8)?))).map_err(sql_error)?;
            let mut items=Vec::new();
            for row in rows {
                let(op,index,state,version,byte_start,byte_end,row_start,row_end,error)=row.map_err(sql_error)?;
                let state:OperationState=serde_json::from_value(Value::String(state)).map_err(|_|WorkbenchError::StoreUnavailable)?;
                let number=|value:i64|u64::try_from(value).map_err(|_|WorkbenchError::StoreUnavailable);
                let row_start=number(row_start)?;let row_end=number(row_end)?;
                if row_end<row_start{return Err(WorkbenchError::StoreUnavailable)}
                items.push(ImportBatchStatus{operation_id:uuid(&op)?,batch_index:u32::try_from(index).map_err(|_|WorkbenchError::StoreUnavailable)?,state,version:positive(version)?.into(),source_byte_start:number(byte_start)?.into(),source_byte_end:number(byte_end)?.into(),source_row_start:row_start.into(),source_row_end:row_end.into(),rows_committed:(state==OperationState::Committed).then(||(row_end-row_start).into()),error_code:error});
            }
            let(count,rows,batches,unknown):(i64,i64,i64,i64)=connection.query_row("SELECT COUNT(*),COALESCE(SUM(CASE WHEN state='committed' THEN source_row_end-source_row_start ELSE 0 END),0),COALESCE(SUM(state='committed'),0),COALESCE(SUM(state='outcome_unknown'),0) FROM operations WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND parent_operation_id=?4 AND parent_kind='import_job'",params![self.store.owner_id(),scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string()],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).map_err(sql_error)?;
            let last:Option<(i64,String)>=connection.query_row("SELECT batch_index,state FROM operations WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND parent_operation_id=?4 AND parent_kind='import_job' ORDER BY batch_index DESC LIMIT 1",params![self.store.owner_id(),scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string()],|row|Ok((row.get(0)?,row.get(1)?))).optional().map_err(sql_error)?;
            let next=match last{None=>Some(0),Some((index,state))if state=="committed"=>u32::try_from(index).ok().and_then(|index|index.checked_add(1)),_=>None};
            Ok((items,u64::try_from(count).map_err(|_|WorkbenchError::StoreUnavailable)?,u64::try_from(rows).map_err(|_|WorkbenchError::StoreUnavailable)?,u64::try_from(batches).map_err(|_|WorkbenchError::StoreUnavailable)?,u64::try_from(unknown).map_err(|_|WorkbenchError::StoreUnavailable)?,next))
        })?;
        if operation.kind == OperationKind::ImportJob {
            progress.confirmed_committed_rows = committed_rows.into();
            progress.confirmed_committed_batches = committed_batches.into();
            progress.unresolved_batches = unresolved.into();
            progress.rows_processed = progress.rows_processed.value().max(committed_rows).into();
            progress.next_batch_index = if operation.state == OperationState::Completed {
                None
            } else {
                next
            };
            if unresolved > 0 {
                operation.state = OperationState::OutcomeUnknown;
                operation.transaction_outcome = TransactionOutcome::OutcomeUnknown;
                operation.effect_summary = EffectSummary::Unknown;
            }
        }
        let has_more = items.len() > page.limit;
        items.truncate(page.limit);
        let next_cursor = has_more.then(|| {
            format!(
                "wbm1.{id}.{}",
                items.last().expect("bounded positive page").batch_index
            )
        });
        Ok(MovementStatus {
            operation,
            progress,
            children: Page {
                items,
                next_cursor,
                completeness: if has_more {
                    Completeness::Limited
                } else {
                    Completeness::Complete
                },
                count: Count::Exact {
                    value: child_count.into(),
                },
                as_of_ms: (self.clock.now_ms()? as u64).into(),
            },
            file,
        })
    }
    /// Acknowledgement never changes this refusal. Previously sent but rolled
    /// back ranges are not silently retried or skipped by continuation.
    pub fn import_next_unsent(
        &self,
        ctx: &InvocationContext,
        scope: &Scope,
        id: Uuid,
    ) -> Result<Option<NextUnsent>, WorkbenchError> {
        self.context(ctx)?;
        let operation = self.get(ctx, scope, id)?;
        if operation.kind != OperationKind::ImportJob {
            return Err(WorkbenchError::InvalidInput);
        }
        let Some(MovementMetadata::Import { total_rows, .. }) =
            self.movement_metadata(ctx, scope, id)?
        else {
            return Err(WorkbenchError::StoreUnavailable);
        };
        self.store.read(|connection|{
            let unknown:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND parent_operation_id=?4 AND state='outcome_unknown')",params![self.store.owner_id(),scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string()],|row|row.get(0)).map_err(sql_error)?;
            if unknown{return Err(WorkbenchError::OutcomeUnknown)}
            let prior:Option<(i64,String,i64,i64)>=connection.query_row("SELECT batch_index,state,source_byte_end,source_row_end FROM operations WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND parent_operation_id=?4 ORDER BY batch_index DESC LIMIT 1",params![self.store.owner_id(),scope.workspace_id.to_string(),scope.database_target_id.to_string(),id.to_string()],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).optional().map_err(sql_error)?;
            match prior{
                None=>Ok(Some(NextUnsent{batch_index:0,byte_start:0.into(),row_start:0.into()})),
                Some((index,state,bytes,rows))if state=="committed"=>{
                    let rows=u64::try_from(rows).map_err(|_|WorkbenchError::StoreUnavailable)?;
                    if rows==total_rows.value(){return Ok(None)}
                    if rows>total_rows.value(){return Err(WorkbenchError::StoreUnavailable)}
                    Ok(Some(NextUnsent{batch_index:u32::try_from(index).ok().and_then(|index|index.checked_add(1)).ok_or(WorkbenchError::Limit)?,byte_start:u64::try_from(bytes).map_err(|_|WorkbenchError::StoreUnavailable)?.into(),row_start:rows.into()}))
                },
                Some(_)=>Err(WorkbenchError::Unsupported),
            }
        })
    }
}
