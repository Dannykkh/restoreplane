import { useEffect, useRef, useState } from "react";
import { TransactionFence } from "../../lib/transactionFence";
import { Dialog } from "../../components/Dialog";
import { workbenchErrorText, type WorkbenchClient } from "../../lib/workbench";
import type { CatalogItem, NativeCounterU64, QueryPurpose, QueryRunReceipt, SavedQuery, Session, Transaction } from "../../lib/workbenchTypes";
import { MAX_SQL_BYTES, QueryEditor, sqlBytes, type ReviewedQuery } from "./QueryEditor";
import { SavedQueries } from "./SavedQueries";
import { useSqlCompletions } from "./useSqlCompletions";

export interface QueryDraft { id: string; name: string; sql: string; baseline: string; binding: string; savedQueryId: string | null; savedVersion: NativeCounterU64 | null; reviewed: ReviewedQuery | null; warnings: string[] }
export function queryBinding(session: Session): string { return JSON.stringify([session.workspaceId, session.connectionProfileId, session.databaseTargetId, session.sessionId, session.connectionGeneration, session.descriptorRevision, session.mode]); }
function draft(binding: string, index: number): QueryDraft { return { id: crypto.randomUUID(), name: `쿼리 ${index}`, sql: "", baseline: "", binding, savedQueryId: null, savedVersion: null, reviewed: null, warnings: [] }; }
export function QueryTabs({ client, session, engine, transaction, onTransaction, onDirtyChange, onExecute, executionBlocked = false, suspended = false, transactionFence, schema = null, relation = null, completionEnabled = false }: {
  client: WorkbenchClient; session: Session; engine: string; transaction: Transaction | null | undefined;
  onTransaction: (transaction: Transaction) => void; onDirtyChange?: (dirty: boolean) => void;
  transactionFence?: TransactionFence;
  onExecute?: (reviewed: ReviewedQuery, warnings: string[]) => Promise<QueryRunReceipt>;
  executionBlocked?: boolean;
  suspended?: boolean;
  schema?: CatalogItem | null;
  relation?: CatalogItem | null;
  completionEnabled?: boolean;
}) {
  const binding = queryBinding(session), bindingRef = useRef(binding); bindingRef.current = binding;
  const [tabs, setTabs] = useState<QueryDraft[]>(() => [draft(binding, 1)]), [activeId, setActiveId] = useState("");
  const active = tabs.find((tab) => tab.id === activeId) ?? tabs[0];
  const [busy, setBusy] = useState(false), [error, setError] = useState(""), [accepted, setAccepted] = useState<QueryRunReceipt | null>(null);
  const [closingState, setClosing] = useState<QueryDraft | null>(null), [discard, setDiscard] = useState(false), [disposition, setDisposition] = useState<"keep_open" | "rollback_request">("keep_open");
  const [savedDialogState, setSavedDialog] = useState<"load" | "save" | null>(null);
  const closing = suspended ? null : closingState, savedDialog = suspended ? null : savedDialogState;
  const revisions = useRef(new Map<string, number>()), request = useRef(0);
  const dirty = tabs.some((tab) => tab.sql !== tab.baseline);
  const completionAllowed = completionEnabled && client.available && session.state === "ready" && !busy && !executionBlocked && !suspended && active?.binding === binding && (transaction === null || transaction !== undefined && ["committed", "rolled_back"].includes(transaction.state));
  const completionProvider = useSqlCompletions({ client, session, engine, schema, relation, enabled: completionAllowed });
  useEffect(() => { onDirtyChange?.(dirty); }, [dirty, onDirtyChange]);
  useEffect(() => {
    request.current++; setBusy(false); setAccepted(null); setError("");
    setTabs((current) => current.map((tab) => ({ ...tab, binding: tab.sql ? tab.binding : binding, reviewed: null, warnings: [] })));
  }, [binding]);
  function update(id: string, change: Partial<QueryDraft>): void { setTabs((current) => current.map((tab) => tab.id === id ? { ...tab, ...change } : tab)); }
  // Pure pre-admission; CodeMirror rejects without changing document/selection or native review.
  function canChangeSql(sql: string): boolean { return !!active && tabs.length <= 8 && tabs.reduce((total, tab) => total + sqlBytes(tab.id === active.id ? sql : tab.sql), 0) <= MAX_SQL_BYTES; }
  function changeSql(sql: string): void {
    if (!active) return;
    if (tabs.reduce((total, tab) => total + sqlBytes(tab.id === active.id ? sql : tab.sql), 0) > MAX_SQL_BYTES) { setError("열린 SQL 초안은 합쳐서 UTF-8 1MiB까지 보유합니다. 다른 초안을 닫거나 별도로 저장해 주세요."); return; }
    revisions.current.set(active.id, (revisions.current.get(active.id) ?? 0) + 1);
    update(active.id, { sql, reviewed: null, warnings: [] }); setAccepted(null); setError("");
  }
  function add(query?: SavedQuery): void {
    if (tabs.length >= 8) { setError("SQL 탭은 현재 창에서 최대 8개까지 열 수 있습니다."); return; }
    if (query && tabs.reduce((total, tab) => total + sqlBytes(tab.sql), sqlBytes(query.sql)) > MAX_SQL_BYTES) { setError("저장한 SQL을 열기 전에 다른 초안을 닫아 주세요. 열린 초안 합계 상한은 1MiB입니다."); return; }
    const next = draft(binding, tabs.length + 1);
    if (query) Object.assign(next, { name: query.name, sql: query.sql, baseline: query.sql, savedQueryId: query.queryId, savedVersion: query.version });
    setTabs((current) => [...current, next]); setActiveId(next.id); setError("");
  }
  async function review(purpose: QueryPurpose, wireSql: string): Promise<void> {
    if (!active || busy || suspended || active.binding !== binding || sqlBytes(wireSql) > MAX_SQL_BYTES) return;
    const id = active.id, source = active.sql, revision = revisions.current.get(id) ?? 0, capturedBinding = binding;
    const requestId = ++request.current; setBusy(true); setError(""); update(id, { reviewed: null, warnings: [] });
    try {
      const result = await client.request("query.review", { session: { sessionId: session.sessionId, connectionGeneration: session.connectionGeneration }, sql: wireSql, purpose });
      if (request.current !== requestId || bindingRef.current !== capturedBinding || (revisions.current.get(id) ?? 0) !== revision) return;
      update(id, { reviewed: { sourceSql: source, wireSql, purpose, review: result, binding: capturedBinding }, warnings: [] });
    } catch (failure) { if (request.current === requestId) setError(workbenchErrorText(failure)); }
    finally { if (request.current === requestId) setBusy(false); }
  }
  async function run(): Promise<void> {
    if (!active?.reviewed || !onExecute || busy || suspended || executionBlocked || active.binding !== binding || active.reviewed.binding !== binding || active.reviewed.sourceSql !== active.sql) return;
    if (BigInt(active.reviewed.review.expiresAtMs) <= BigInt(Date.now())) { update(active.id, { reviewed: null }); setError("검토 기한이 지났습니다. 원문을 다시 검토해 주세요."); return; }
    if (!active.reviewed.review.warnings.every((warning) => active.warnings.includes(warning))) return;
    const reviewed = active.reviewed, warnings = active.warnings;
    const capturedBinding = binding, requestId = ++request.current; setBusy(true); setError("");
    // Retire the review before dispatch; an ambiguous response never re-enables one-click resend.
    update(active.id, { reviewed: null, warnings: [] });
    try { const receipt = await onExecute(reviewed, warnings); if (bindingRef.current === capturedBinding && request.current === requestId) setAccepted(receipt); }
    catch (failure) { if (request.current === requestId) setError(workbenchErrorText(failure)); }
    finally { if (request.current === requestId) setBusy(false); }
  }
  const liveTransaction = transaction && !["committed", "rolled_back"].includes(transaction.state) ? transaction : null;
  function remove(id: string): void {
    const next = tabs.find((tab) => tab.id !== id);
    setTabs((current) => current.filter((tab) => tab.id !== id)); revisions.current.delete(id);
    if (active?.id === id) { setActiveId(next?.id ?? ""); requestAnimationFrame(() => next ? document.getElementById(`sql-tab-${next.id}`)?.focus() : document.querySelector<HTMLButtonElement>(".wb-query-tabs .wb-new-query")?.focus()); }
  }
  function requestClose(tab: QueryDraft): void {
    if (busy) return;
    if (tab.sql !== tab.baseline || liveTransaction) { setClosing(tab); setDiscard(false); setDisposition("keep_open"); }
    else remove(tab.id);
  }
  async function closeTab(): Promise<void> {
    if (!closing || busy || (closing.sql !== closing.baseline && !discard)) return;
    if (disposition === "rollback_request" && liveTransaction) {
      const ticket = transactionFence?.mutate(); if (transactionFence && !ticket) return;
      const capturedBinding = binding; setBusy(true); setError("");
      try {
        const next = await client.request("transaction.rollback", { session: { sessionId: session.sessionId, connectionGeneration: session.connectionGeneration }, transactionId: liveTransaction.transactionId });
        if (bindingRef.current !== capturedBinding || ticket && !transactionFence!.acceptsMutation(ticket)) return; onTransaction(next);
        if (next.state !== "rolled_back") { setError("롤백 완료를 확인하지 못했습니다. 트랜잭션과 초안을 유지합니다."); return; }
      } catch (failure) { setError(workbenchErrorText(failure)); return; }
      finally { if (ticket) transactionFence!.finish(ticket); setBusy(false); }
    }
    remove(closing.id); setClosing(null);
  }
  return <section className="wb-query-tabs" aria-label="SQL 작업">
    <div className="wb-query-actions"><div role="tablist" aria-label="SQL 초안 탭">{tabs.map((tab) => <div className="wb-query-tab" key={tab.id}><button type="button" role="tab" id={`sql-tab-${tab.id}`} aria-controls={`sql-panel-${tab.id}`} aria-selected={active?.id === tab.id} tabIndex={active?.id === tab.id ? 0 : -1} onClick={() => setActiveId(tab.id)} onKeyDown={(event) => {
      if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key) || event.nativeEvent.isComposing) return;
      event.preventDefault(); const index = tabs.findIndex((item) => item.id === tab.id);
      const target = event.key === "Home" ? tabs[0] : event.key === "End" ? tabs[tabs.length - 1] : tabs[(index + (event.key === "ArrowRight" ? 1 : tabs.length - 1)) % tabs.length];
      setActiveId(target.id); requestAnimationFrame(() => document.getElementById(`sql-tab-${target.id}`)?.focus());
    }}>{tab.name}{tab.sql !== tab.baseline ? " · 미저장" : ""}{tab.binding !== binding ? " · 연결 변경됨" : ""}</button></div>)}</div>{active && <button type="button" className="wb-text-button" aria-label={`${active.name} 탭 닫기`} disabled={busy} onClick={() => requestClose(active)}>현재 탭 닫기</button>}<button className="button button-secondary wb-new-query" type="button" disabled={tabs.length >= 8 || busy} onClick={() => add()}>새 SQL 탭</button><button className="button button-secondary" type="button" disabled={busy} onClick={() => setSavedDialog("load")}>저장한 SQL 열기</button><details className="wb-draft-context"><summary>초안 정보</summary><p className="wb-muted">{session.actualContext.endpoint} / {session.actualContext.database} · 현재 연결에 고정된 초안 · 최대 8개 / 합계 1MiB</p></details></div>
    {active ? <div role="tabpanel" id={`sql-panel-${active.id}`} aria-labelledby={`sql-tab-${active.id}`}>
      {active.binding !== binding && <button className="button button-secondary" type="button" disabled={busy} onClick={() => update(active.id, { binding, reviewed: null, warnings: [] })}>실제 대상 확인하고 이 초안 다시 연결</button>}
      <QueryEditor key={active.id} sql={active.sql} engine={engine} mode={session.mode} reviewed={active.reviewed} confirmedWarnings={active.warnings} busy={busy} canRun={!!onExecute && !executionBlocked && session.state === "ready"} staleBinding={active.binding !== binding} onChange={changeSql} canChange={canChangeSql} completionProvider={completionProvider} onReview={(purpose, wireSql) => void review(purpose, wireSql)} onWarnings={(warnings) => update(active.id, { warnings })} onRun={() => void run()} onSave={() => setSavedDialog("save")} suspended={suspended} />
    </div> : <p>열린 SQL 탭이 없습니다. 새 SQL 탭을 열어 원문을 입력하세요.</p>}
    {tabs.filter((tab) => tab.id !== active?.id).map((tab) => <div key={tab.id} role="tabpanel" id={`sql-panel-${tab.id}`} aria-labelledby={`sql-tab-${tab.id}`} hidden />)}
    {error && <p className="wb-error" role="alert">{error}</p>}
    {accepted && <p role="status">SQL 실행 요청 접수 · 작업 {accepted.operationId} · 실행 세대 {accepted.executionEpoch} · 완료 상태는 별도로 확인합니다.</p>}
    {savedDialog && (savedDialog === "load" || active) && <SavedQueries client={client} scope={{ workspaceId: session.workspaceId, connectionProfileId: session.connectionProfileId, databaseTargetId: session.databaseTargetId }} mode={savedDialog} sql={active?.sql ?? ""} queryId={active?.savedQueryId ?? null} version={active?.savedVersion ?? null} name={active?.savedQueryId ? active.name : ""} onLoaded={add} onSaved={(query) => active && update(active.id, { name: query.name, savedQueryId: query.queryId, savedVersion: query.version, baseline: query.sql })} onDeleted={() => active && update(active.id, { savedQueryId: null, savedVersion: null, baseline: "" })} onClose={() => setSavedDialog(null)} />}
    {closing && <Dialog title="SQL 탭을 닫을까요?" description="초안을 버리는 동작과 트랜잭션 정리는 별개입니다. 트랜잭션을 유지하면 현재 연결은 열린 상태로 남습니다." onClose={() => { if (!busy) setClosing(null); }}>
      {closing.sql !== closing.baseline && <label className="wb-check"><input type="checkbox" checked={discard} onChange={(event) => setDiscard(event.currentTarget.checked)} />이 탭의 저장하지 않은 SQL 초안을 버립니다.</label>}
      {liveTransaction && <fieldset><legend>현재 연결의 트랜잭션</legend><label className="wb-check"><input type="radio" name="sql-tab-disposition" checked={disposition === "keep_open"} onChange={() => setDisposition("keep_open")} />트랜잭션과 연결 유지</label><label className="wb-check"><input type="radio" name="sql-tab-disposition" checked={disposition === "rollback_request"} onChange={() => setDisposition("rollback_request")} />롤백 완료를 확인한 뒤 탭 닫기</label></fieldset>}
      <div className="dialog-actions"><button className="button button-secondary" type="button" disabled={busy} onClick={() => setClosing(null)}>계속 편집</button><button className="button button-danger" type="button" disabled={busy || (closing.sql !== closing.baseline && !discard)} onClick={() => void closeTab()}>선택한 처리로 탭 닫기</button></div>
    </Dialog>}
  </section>;
}
