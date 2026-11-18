//! Restart reconciliation uses only durable dispatch/effect evidence.
use rusqlite::{params, Connection, TransactionBehavior};

#[derive(Clone, Debug, Default)]
pub struct RecoveryReport {
    pub cancelled: u64,
    pub interrupted: u64,
    pub outcome_unknown: u64,
}

pub(crate) fn old_boot(
    connection: &mut Connection,
    owner: &str,
    boot: &str,
    now: i64,
) -> rusqlite::Result<RecoveryReport> {
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let pending = {
        let mut query=tx.prepare("SELECT workspace_id,target_id,operation_id,state,kind,dispatch_count FROM operations WHERE owner_id=?1 AND process_boot_epoch<>?2 AND state IN('accepted','dispatching','running','commit_requested') ORDER BY kind='import_job',created_at_ms,operation_id")?;
        let rows = query.query_map(params![owner, boot], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut report = RecoveryReport::default();
    for (workspace, target, id, state, kind, dispatch) in pending {
        let readonly:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM statements WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4) AND NOT EXISTS(SELECT 1 FROM statements WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4 AND (effect_class<>'read' OR effect_evidence<>'read_only'))",params![owner,workspace,target,id],|row|row.get(0))?;
        let (next, outcome, completion) = if state == "accepted" && dispatch == 0 {
            report.cancelled += 1;
            ("cancelled", "none", "cancelled")
        } else if kind == "export_job" || readonly && kind != "manual_transaction" {
            report.interrupted += 1;
            ("interrupted", "none", "interrupted")
        } else if kind == "import_job" {
            let unknown:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND parent_operation_id=?4 AND state='outcome_unknown')",params![owner,workspace,target,id],|row|row.get(0))?;
            if unknown {
                report.outcome_unknown += 1;
                ("outcome_unknown", "outcome_unknown", "partial")
            } else {
                report.interrupted += 1;
                ("interrupted", "none", "partial")
            }
        } else {
            report.outcome_unknown += 1;
            ("outcome_unknown", "outcome_unknown", "interrupted")
        };
        tx.execute("UPDATE operations SET state=?5,transaction_outcome=?6,result_completeness=?7,version=version+1,terminal_at_ms=?8,updated_at_ms=MAX(updated_at_ms,?8),failure_code='WB_RECOVERED_OLD_BOOT' WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4",params![owner,workspace,target,id,next,outcome,completion,now])?;
        if dispatch != 0 {
            tx.execute("UPDATE statements SET outcome=CASE WHEN outcome='sent' THEN ?5 ELSE outcome END,effect_evidence=CASE WHEN effect_evidence='pending_transaction' THEN 'outcome_unknown' ELSE effect_evidence END,version=version+1,updated_at_ms=MAX(updated_at_ms,?6) WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id=?4 AND (outcome='sent' OR effect_evidence='pending_transaction')",params![owner,workspace,target,id,if readonly{"cancelled"}else{"outcome_unknown"},now])?;
        }
        if kind == "manual_transaction" && next == "outcome_unknown" {
            // A mixed script has no aggregate transaction FK. Its statement
            // links still make pending effects uncertain after this loss.
            tx.execute("UPDATE operations SET state='outcome_unknown',transaction_outcome=CASE WHEN transaction_operation_id=?4 THEN 'outcome_unknown' ELSE transaction_outcome END,version=version+1,terminal_at_ms=COALESCE(terminal_at_ms,?5),updated_at_ms=MAX(updated_at_ms,?5) WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND operation_id<>?4 AND (transaction_operation_id=?4 OR EXISTS(SELECT 1 FROM statements s WHERE s.owner_id=operations.owner_id AND s.workspace_id=operations.workspace_id AND s.target_id=operations.target_id AND s.operation_id=operations.operation_id AND s.transaction_operation_id=?4 AND s.effect_evidence='pending_transaction'))",params![owner,workspace,target,id,now])?;
            tx.execute("UPDATE statements SET effect_evidence='outcome_unknown',version=version+1,updated_at_ms=MAX(updated_at_ms,?5) WHERE owner_id=?1 AND workspace_id=?2 AND target_id=?3 AND transaction_operation_id=?4 AND effect_evidence='pending_transaction'",params![owner,workspace,target,id,now])?;
        }
    }
    tx.commit()?;
    Ok(report)
}
