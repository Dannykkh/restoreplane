import { useEffect, useRef, useState } from "react";
import { WorkbenchError, workbenchErrorText, type WorkbenchClient } from "../../lib/workbench";
import type { CatalogItem, QueryRunReceipt, Session, Transaction, WorkbenchOperation } from "../../lib/workbenchTypes";
import { QueryTabs } from "./QueryTabs";
import { TransactionBar } from "./TransactionBar";
import { OperationPanel } from "./OperationPanel";
import { QueryResults, type QueryContext } from "./QueryResults";
import type { ReviewedQuery } from "./QueryEditor";
import { TransactionFence } from "../../lib/transactionFence";

export const SQL_LIMITS = { maxRows: 100_000, maxBytes: 64 * 1024 * 1024, statementTimeoutMs: 30_000, lockWaitMs: 2_000 };
export function SqlWorkspace({ client, session, engine, transaction, onSession, onTransaction, onDirty, onContext, onRunning, suspended, transactionFence, schema = null, relation = null, visible = true, renderTransactionControls = true }: {
  client: WorkbenchClient; session: Session; engine: string; transaction: Transaction | null | undefined;
  onSession: (session: Session) => void; onTransaction: (transaction: Transaction | null) => void;
  onDirty: (dirty: boolean) => void; onContext: (context: QueryContext) => void; onRunning: (running: boolean) => void; suspended: boolean;
  transactionFence?: TransactionFence;
  schema?: CatalogItem | null; relation?: CatalogItem | null; visible?: boolean; renderTransactionControls?: boolean;
}) {
  const [receipt, setReceipt] = useState<QueryRunReceipt | null>(null), [operationId, setOperationId] = useState<string | null>(null), [running, setRunning] = useState(false);
  const [stopping, setStopping] = useState(false), [stopStatus, setStopStatus] = useState(""), [error, setError] = useState("");
  const [splitFocus, setSplitFocus] = useState<"split" | "editor" | "results">("split");
  const key = JSON.stringify([session.sessionId, session.connectionGeneration, session.mode]);
  const current = useRef(key), runRef = useRef<QueryRunReceipt | null>(null); current.current = key;
  const fallbackFence = useRef(new TransactionFence()), fence = transactionFence ?? fallbackFence.current;
  fence.bind(JSON.stringify({ sessionId: session.sessionId, connectionGeneration: session.connectionGeneration }));
  const observedVersion = useRef("");
  useEffect(() => { onRunning(running); }, [running, onRunning]);
  useEffect(() => { setReceipt(null); runRef.current = null; setOperationId(null); setRunning(false); setStopStatus(""); setStopping(false); setError(""); onContext({ consistency: null, count: { kind: "unknown" }, completeness: null, receiving: false }); }, [key]);
  async function execute(reviewed: ReviewedQuery, warnings: string[]): Promise<QueryRunReceipt> {
    if (running || suspended) throw new WorkbenchError("WB_BUSY");
    const captured = key; fence.invalidate(); setRunning(true); setError(""); setStopStatus(""); setReceipt(null); runRef.current = null; setOperationId(null);
    const payload = { session: { sessionId: session.sessionId, connectionGeneration: session.connectionGeneration }, reviewId: reviewed.review.reviewId, confirmedWarnings: warnings, limits: SQL_LIMITS };
    try {
      const prepared = await client.request("operation.prepare", { intent: { command: "query.run", payload } });
      if (current.current !== captured) throw new WorkbenchError("WB_SESSION_STALE");
      if (BigInt(prepared.expiresAtMs) <= BigInt(Date.now())) throw new WorkbenchError("WB_ATTEMPT_EXPIRED");
      const next = await client.request("query.run", payload, undefined, prepared.attemptToken);
      if (current.current !== captured) throw new WorkbenchError("WB_SESSION_STALE");
      runRef.current = next; setReceipt(next); setOperationId(next.operationId); return next;
    } catch (failure) {
      if (current.current === captured) { setRunning(false); if (failure instanceof WorkbenchError && failure.operationId) setOperationId(failure.operationId); }
      throw failure;
    } finally { fence.invalidate(); }
  }
  async function stop(): Promise<void> {
    const active = runRef.current; if (!active || stopping) return;
    const captured = key; setStopping(true); setError("");
    try {
      const outcome = await client.request("query.cancel", { session: { sessionId: session.sessionId, connectionGeneration: session.connectionGeneration }, queryRunId: active.queryRunId, executionEpoch: active.executionEpoch });
      if (current.current !== captured || runRef.current?.queryRunId !== active.queryRunId || runRef.current.executionEpoch !== active.executionEpoch) return;
      setStopStatus(outcome.settled ? "중지 요청 처리가 끝났습니다. 실행 결과와 트랜잭션 효과는 아래 작업 상태에서 확인하세요." : outcome.cancelRequested ? "중지를 요청했습니다. 취소·롤백 완료는 아직 확인하지 못했습니다." : "중지 요청을 적용하지 않았습니다. 현재 작업 상태를 확인하세요.");
    } catch (failure) { if (current.current === captured) setError(workbenchErrorText(failure)); }
    finally { if (current.current === captured) setStopping(false); }
  }
  function observed(operation: WorkbenchOperation): void { if (!["accepted", "dispatching", "running", "commit_requested"].includes(operation.state)) { const version = `${operation.operationId}:${operation.version}`; if (observedVersion.current !== version) { observedVersion.current = version; fence.invalidate(); } setRunning(false); } }
  const settledTransaction = transaction === null || !!transaction && ["committed", "rolled_back"].includes(transaction.state);
  const editorVisible = visible && splitFocus !== "results", resultsVisible = visible && splitFocus !== "editor";
  const completionEnabled = editorVisible && client.available && session.state === "ready" && !running && !suspended && settledTransaction;
  return <section className="wb-sql-workspace" data-visible={visible} aria-label="SQL 작업 공간">
    {renderTransactionControls && <TransactionBar client={client} session={session} engine={engine} transaction={transaction} onSession={onSession} onTransaction={onTransaction} suspended={suspended} transactionFence={fence} executionBlocked={running || suspended} />}
    <div className="wb-sql-status">
      {(visible || running) && <div className="wb-query-actions wb-sql-tools"><span>최대 {SQL_LIMITS.maxRows}행 · 64MiB · 30초 · 잠금 2초</span><button className="button button-secondary" type="button" disabled={!receipt || !running || stopping} onClick={() => void stop()}>현재 실행 중지</button><div className="wb-split-actions" hidden={!visible} aria-label="SQL 작업 영역 표시"><button className="wb-text-button" type="button" aria-pressed={splitFocus === "split"} onClick={() => setSplitFocus("split")}>편집기와 결과 함께</button><button className="wb-text-button" type="button" aria-pressed={splitFocus === "editor"} onClick={() => setSplitFocus("editor")}>편집기 집중</button><button className="wb-text-button" type="button" aria-pressed={splitFocus === "results"} onClick={() => setSplitFocus("results")}>결과 집중</button></div></div>}
      {stopStatus && <p role="status">{stopStatus}</p>}{error && <p className="wb-error" role="alert">{error}</p>}
      {operationId && <OperationPanel key={operationId} client={client} operationId={operationId} onOperation={observed} compact />}
    </div>
    <div className="wb-sql-split" data-focus={splitFocus} hidden={!visible}>
      <div className="wb-sql-editor-region" hidden={!editorVisible}>
        <QueryTabs client={client} session={session} engine={engine} transaction={transaction} onTransaction={onTransaction} onDirtyChange={onDirty} onExecute={execute} executionBlocked={running || suspended} suspended={suspended} transactionFence={fence} schema={schema} relation={relation} completionEnabled={completionEnabled} />
      </div>
      <div className="wb-sql-results-region" hidden={!resultsVisible}>
        {receipt ? <QueryResults key={`${key}:${receipt.queryRunId}:${receipt.executionEpoch}`} client={client} session={session} receipt={receipt} onContext={onContext} visible={resultsVisible} /> : <p className="wb-empty-result">SQL 원문을 검토하고 실행하면 결과가 여기에 표시됩니다.</p>}
      </div>
    </div>
  </section>;
}
