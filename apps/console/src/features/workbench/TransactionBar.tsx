import { useEffect, useRef, useState } from "react";
import { Dialog } from "../../components/Dialog";
import { WorkbenchError, workbenchErrorText, type WorkbenchClient } from "../../lib/workbench";
import type { Mode, Session, SessionRef, Transaction } from "../../lib/workbenchTypes";
import { TransactionFence } from "../../lib/transactionFence";

export const MODE_LABEL: Record<Mode, string> = { read_only: "읽기 전용", read_write: "데이터 변경 모드", full: "전체 SQL 모드" };
export function transactionLabel(transaction: Transaction | null | undefined): string {
  if (transaction === undefined) return "트랜잭션 상태 미확인";
  if (!transaction) return "열린 트랜잭션 없음";
  return { opening: "열기 확인 중", active: "트랜잭션 진행 중", failed_needs_rollback: "롤백 필요", commit_requested: "COMMIT 확인 중", committed: "COMMIT 확인됨", rollback_requested: "롤백 확인 중", rolled_back: "롤백 확인됨", outcome_unknown: "결과 미확인" }[transaction.state];
}
export function TransactionBar({ client, session, engine, transaction, onSession, onTransaction, suspended = false, transactionFence, compact = false, executionBlocked = false }: { client: WorkbenchClient; session: Session; engine: string; transaction: Transaction | null | undefined; onSession: (session: Session) => void; onTransaction: (transaction: Transaction | null) => void; suspended?: boolean; transactionFence?: TransactionFence; compact?: boolean; executionBlocked?: boolean }) {
  const [busy, setBusy] = useState(false), [error, setError] = useState(""), [modeState, setMode] = useState<Mode | null>(null);
  const [commitDialogState, setCommitDialog] = useState(false), [confirmed, setConfirmed] = useState(false);
  const mode = suspended ? null : modeState, commitDialog = !suspended && commitDialogState;
  const [isolation, setIsolation] = useState(engine === "sqlite" ? "serializable" : engine === "postgresql" ? "read_committed" : "repeatable_read");
  const [readOnly, setReadOnly] = useState(true);
  const ref: SessionRef = { sessionId: session.sessionId, connectionGeneration: session.connectionGeneration };
  const key = JSON.stringify(ref), current = useRef(key); current.current = key;
  const fallbackFence = useRef(new TransactionFence()), fence = transactionFence ?? fallbackFence.current;
  fence.bind(key);
  async function refresh(): Promise<void> {
    const captured = key, ticket = fence.read(); if (!ticket) return;
    try { const value = await client.request("transaction.get", { session: ref }); if (current.current === captured && fence.acceptsRead(ticket)) onTransaction(value); }
    catch (failure) { if (current.current === captured && fence.acceptsRead(ticket)) setError(workbenchErrorText(failure)); }
  }
  useEffect(() => {
    current.current = key;
    setBusy(false); setError(""); setMode(null); setCommitDialog(false); setConfirmed(false);
    void refresh(); const timer = window.setInterval(() => void refresh(), 2000);
    return () => { window.clearInterval(timer); current.current = ""; fence.invalidate(); };
  }, [client, key, fence]);
  async function changeMode(): Promise<void> {
    if (!mode || busy || executionBlocked || session.actualContext.purpose === "restored") return;
    const captured = key, ticket = fence.mutate(); if (!ticket) return; setBusy(true); setError("");
    try {
      const next = await client.request("session.mode.set", { session: ref, mode });
      if (current.current !== captured || !fence.acceptsMutation(ticket)) return;
      if (next.workspaceId !== session.workspaceId || next.databaseTargetId !== session.databaseTargetId || next.connectionProfileId !== session.connectionProfileId || next.mode !== mode) throw new WorkbenchError("WB_SESSION_STALE");
      onSession(next); setMode(null);
    } catch (failure) { if (current.current === captured && fence.acceptsMutation(ticket)) setError(workbenchErrorText(failure)); }
    finally { fence.finish(ticket); if (current.current === captured) setBusy(false); }
  }
  async function begin(): Promise<void> {
    if (busy || executionBlocked) return; const captured = key, ticket = fence.mutate(); if (!ticket) return; setBusy(true); setError("");
    try { const next = await client.request("transaction.begin", { session: ref, isolation, readOnly: readOnly || session.mode === "read_only" }); if (current.current === captured && fence.acceptsMutation(ticket)) onTransaction(next); }
    catch (failure) { if (current.current === captured && fence.acceptsMutation(ticket)) setError(workbenchErrorText(failure)); }
    finally { fence.finish(ticket); if (current.current === captured) setBusy(false); }
  }
  async function rollback(): Promise<void> {
    if (!transaction || busy) return; const captured = key, ticket = fence.mutate(); if (!ticket) return; setBusy(true); setError("");
    try { const next = await client.request("transaction.rollback", { session: ref, transactionId: transaction.transactionId }); if (current.current === captured && fence.acceptsMutation(ticket)) onTransaction(next); }
    catch (failure) { if (current.current === captured && fence.acceptsMutation(ticket)) setError(workbenchErrorText(failure)); }
    finally { fence.finish(ticket); if (current.current === captured) setBusy(false); }
  }
  async function commit(): Promise<void> {
    if (!transaction || !confirmed || busy) return; const captured = key, ticket = fence.mutate(); if (!ticket) return; setBusy(true); setError("");
    try {
      const payload = { session: ref, transactionId: transaction.transactionId };
      const prepared = await client.request("operation.prepare", { intent: { command: "transaction.commit", payload } });
      if (current.current !== captured || !fence.acceptsMutation(ticket)) return;
      if (BigInt(prepared.expiresAtMs) <= BigInt(Date.now())) throw new WorkbenchError("WB_ATTEMPT_EXPIRED");
      await client.request("transaction.commit", payload, undefined, prepared.attemptToken);
      if (current.current === captured && fence.acceptsMutation(ticket)) { fence.finish(ticket); setCommitDialog(false); setConfirmed(false); await refresh(); }
    } catch (failure) { if (current.current === captured && fence.acceptsMutation(ticket)) setError(workbenchErrorText(failure)); }
    finally { fence.finish(ticket); if (current.current === captured) setBusy(false); }
  }
  const live = transaction && !["committed", "rolled_back"].includes(transaction.state);
  const beginControls = <><label>격리 수준<select value={isolation} disabled={busy} onChange={(event) => setIsolation(event.currentTarget.value)}>{engine !== "sqlite" && <option value="read_committed">READ COMMITTED</option>}<option value={engine === "sqlite" ? "serializable" : "repeatable_read"}>{engine === "sqlite" ? "SERIALIZABLE" : "REPEATABLE READ"}</option>{engine === "postgresql" && <option value="serializable">SERIALIZABLE</option>}</select></label><label className="wb-check"><input type="checkbox" checked={readOnly || session.mode === "read_only"} disabled={busy || session.mode === "read_only"} onChange={(event) => setReadOnly(event.currentTarget.checked)} />읽기 전용 트랜잭션</label><button className="button button-secondary" type="button" disabled={busy || executionBlocked || transaction === undefined || session.state !== "ready"} onClick={() => void begin()}>트랜잭션 시작</button></>;
  return <section className={`wb-transaction${compact ? " is-compact" : ""}`} aria-label="SQL 모드와 트랜잭션">
    <div className="wb-query-actions"><label>SQL 모드<select value={session.mode} disabled={busy || executionBlocked || session.state !== "ready" || !!live || session.actualContext.purpose === "restored"} onChange={(event) => { if (!executionBlocked) setMode(event.currentTarget.value as Mode); }}><option value="read_only">읽기 전용</option><option value="read_write">데이터 변경 모드</option><option value="full">전체 SQL 모드</option></select></label>
      <strong>{transactionLabel(transaction)}</strong><button type="button" className="wb-text-button" disabled={busy} onClick={() => void refresh()}>상태 확인</button>
      {live ? <><button className="button button-secondary" type="button" disabled={busy || transaction.state !== "active"} onClick={() => { setCommitDialog(true); setConfirmed(false); }}>COMMIT 검토</button><button className="button button-secondary" type="button" disabled={busy} onClick={() => void rollback()}>롤백 요청</button><span className="mono">{transaction.isolation} · {transaction.readOnly ? "읽기 전용" : "변경 가능"}</span></> : compact ? <details className="wb-transaction-setup"><summary>트랜잭션 시작 설정</summary><div className="wb-query-actions">{beginControls}</div></details> : beginControls}
    </div>
    {transaction?.state === "outcome_unknown" && <p className="wb-error" role="alert">결과를 확인하지 못했습니다. COMMIT·롤백 성공으로 표시하거나 같은 변경을 다시 보내지 않습니다.</p>}
    {error && <p className="wb-error" role="alert">{error}</p>}
    {mode && <Dialog title={`${MODE_LABEL[mode]}로 전환할까요?`} description="이 설정은 앱의 SQL 검토 정책입니다. 데이터베이스 사용자의 실제 권한을 추가하지 않습니다. 연결 또는 세대가 바뀌면 기존 SQL 검토는 무효화됩니다." onClose={() => { if (!busy) setMode(null); }}><p className="mono">{session.actualContext.endpoint} / {session.actualContext.database}</p><div className="dialog-actions"><button type="button" className="button button-secondary" disabled={busy} onClick={() => setMode(null)}>취소</button><button type="button" className="button button-primary" disabled={busy || executionBlocked} onClick={() => void changeMode()}>이 대상의 모드 전환</button></div></Dialog>}
    {commitDialog && <Dialog title="이 트랜잭션에 COMMIT을 요청할까요?" description="최신 대상 권한을 다시 확인합니다. 요청 접수는 COMMIT 완료 확인과 다릅니다." onClose={() => { if (!busy) setCommitDialog(false); }}><p className="mono">{session.actualContext.endpoint} / {session.actualContext.database}</p><p className="mono">{transaction?.transactionId}</p><label className="wb-check"><input type="checkbox" checked={confirmed} disabled={busy} onChange={(event) => setConfirmed(event.currentTarget.checked)} />위 대상의 이 트랜잭션에 COMMIT을 요청합니다.</label><div className="dialog-actions"><button type="button" className="button button-secondary" disabled={busy} onClick={() => setCommitDialog(false)}>취소</button><button type="button" className="button button-danger" disabled={busy || !confirmed} onClick={() => void commit()}>확인하고 COMMIT 요청</button></div></Dialog>}
  </section>;
}
