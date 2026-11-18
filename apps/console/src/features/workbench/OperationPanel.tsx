import { useEffect, useRef, useState } from "react";
import { WorkbenchError, applyOperationSnapshot, workbenchErrorText, type WorkbenchClient } from "../../lib/workbench";
import type { StatementOutcome, WorkbenchOperation, WorkbenchPage } from "../../lib/workbenchTypes";

export const OPERATION_LABEL: Record<WorkbenchOperation["state"], string> = { accepted: "요청 접수", dispatching: "전송 준비", running: "실행 중", commit_requested: "COMMIT 확인 중", committed: "COMMIT 확인됨", rolled_back: "롤백 확인됨", failed: "실행 실패", outcome_unknown: "결과 미확인", completed: "실행 완료", cancelled: "취소 확인됨", interrupted: "수신 중단", partial: "일부 결과" };
const EFFECT_LABEL: Record<StatementOutcome["effectEvidence"], string> = { none: "효과 증거 없음", read_only: "조회 효과", pending_transaction: "트랜잭션 안에서 미확정", committed: "COMMIT 확인됨", rolled_back: "롤백 확인됨", outcome_unknown: "효과 미확인" };
const STATEMENT_LABEL: Record<StatementOutcome["outcome"], string> = { not_sent: "미전송", sent: "전송됨", succeeded: "문장 완료", failed: "문장 실패", cancelled: "취소됨", outcome_unknown: "결과 미확인" };
export function OperationPanel({ client, operationId, onOperation, compact = false }: { client: WorkbenchClient; operationId: string; onOperation?: (operation: WorkbenchOperation) => void; compact?: boolean }) {
  const [operation, setOperation] = useState<WorkbenchOperation | null>(null), [error, setError] = useState("");
  const [page, setPage] = useState<WorkbenchPage<StatementOutcome> | null>(null), [cursor, setCursor] = useState<string | null>(null), [expanded, setExpanded] = useState(false);
  const [understood, setUnderstood] = useState(false), [busy, setBusy] = useState(false);
  const current = useRef<WorkbenchOperation | null>(null), identity = useRef(operationId), pulling = useRef(false), callback = useRef(onOperation); callback.current = onOperation;
  identity.current = operationId;
  async function refresh(): Promise<void> {
    if (pulling.current) return;
    const captured = operationId; pulling.current = true;
    try {
      const snapshot = await client.request("operation.get", { operationId, knownVersion: current.current?.version ?? null });
      if (identity.current !== captured) return;
      if (!snapshot.unchanged && snapshot.operation.operationId !== operationId) throw new WorkbenchError("WB_PAYLOAD_INVALID");
      const next = applyOperationSnapshot(current.current, snapshot);
      current.current = next; setOperation(next); if (next) callback.current?.(next); setError("");
    } catch (failure) { if (identity.current === captured) setError(workbenchErrorText(failure)); }
    finally { pulling.current = false; }
  }
  useEffect(() => {
    identity.current = operationId;
    current.current = null; setOperation(null); setPage(null); setExpanded(false); setCursor(null); setUnderstood(false);
    void refresh(); const timer = window.setInterval(() => void refresh(), 2000);
    return () => { window.clearInterval(timer); identity.current = ""; };
  }, [client, operationId]);
  useEffect(() => {
    if (!expanded) return;
    let active = true; setPage(null);
    void client.request("operation.statements", { operationId, page: { cursor, limit: 100 } }).then((value) => { if (active) setPage(value); }).catch((failure: unknown) => { if (active) setError(workbenchErrorText(failure)); });
    return () => { active = false; };
  }, [client, operationId, cursor, expanded]);
  async function acknowledge(): Promise<void> {
    if (!operation || !understood || busy) return; const captured = operationId; setBusy(true);
    try {
      const next = await client.request("operation.acknowledge_unknown", { operationId, expectedVersion: operation.version, acknowledgment: "uncertainty_understood" });
      if (identity.current !== captured) return;
      current.current = applyOperationSnapshot(current.current, { unchanged: false, operation: next }); setOperation(current.current); setUnderstood(false);
    } catch (failure) { if (identity.current === captured) setError(workbenchErrorText(failure)); }
    finally { if (identity.current === captured) setBusy(false); }
  }
  const statements = expanded ? page?.items ?? [] : operation?.statements ?? [];
  const statementDetails = <><div className="wb-statement-list">{statements.map((statement) => <div className="wb-statement" key={statement.ordinal}><strong>{statement.ordinal + 1}번째 문장 · {STATEMENT_LABEL[statement.outcome]}</strong><span>{EFFECT_LABEL[statement.effectEvidence]}</span><span className="mono">일치 {statement.rowsMatched ?? "미확인"} / 변경 {statement.rowsChanged ?? "미확인"}</span>{statement.errorCode && <span>진단 코드: {statement.errorCode}</span>}</div>)}</div><div className="wb-query-actions"><button className="button button-secondary" type="button" onClick={() => { setExpanded(true); setCursor(null); }}>문장별 증거 조회</button>{expanded && <button className="button button-secondary" type="button" disabled={!page?.nextCursor} onClick={() => setCursor(page?.nextCursor ?? null)}>다음 문장 목록</button>}</div></>;
  return <section className={`wb-operation${compact ? " is-compact" : ""}`} aria-label="실행 결과와 효과 증거">
    <div className="wb-query-actions"><h3>{operation ? OPERATION_LABEL[operation.state] : "작업 상태 확인 중"}</h3><span className="mono">{operationId}</span><button className="button button-secondary" type="button" onClick={() => void refresh()}>작업 상태 확인</button></div>
    {operation?.state === "outcome_unknown" && <div className="wb-notice" role="alert"><p>대상 데이터베이스의 결과를 확인하지 못했습니다. 성공·롤백으로 간주하거나 같은 변경을 자동으로 다시 보내지 않습니다.</p><label className="wb-check"><input type="checkbox" checked={understood} disabled={busy || operation.acknowledgedAtMs !== null} onChange={(event) => setUnderstood(event.currentTarget.checked)} />결과가 미확인 상태임을 이해했습니다.</label><button className="button button-secondary" type="button" disabled={!understood || busy || operation.acknowledgedAtMs !== null} onClick={() => void acknowledge()}>미확인 상태를 인지했음을 기록</button>{operation.acknowledgedAtMs !== null && <p>인지 기록이 있습니다. 원래 결과는 계속 미확인입니다.</p>}</div>}
    {compact ? <details className="wb-operation-details"><summary>문장별 효과 증거</summary>{statementDetails}</details> : statementDetails}
    {error && <p className="wb-error" role="alert">{error}</p>}
  </section>;
}
