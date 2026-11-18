import { useEffect, useRef, useState } from "react";
import { Dialog } from "../../components/Dialog";
import { WorkbenchError, readonlyReasonText, workbenchErrorText, type WorkbenchClient } from "../../lib/workbench";
import type { CatalogItem, Column, EditChange, EditableCell, EditReviewData, EditValues, ResultRow, Session, Transaction, WorkbenchOperation } from "../../lib/workbenchTypes";
import { EditCellInput } from "./EditCellInput";
import { EditReview } from "./EditReview";
import { ConflictPanel } from "./ConflictPanel";
import { OperationPanel } from "./OperationPanel";

export function EditDraft({ client, session, object, resultSetId, columns, selectedRow, transaction, onDirty, onRefresh, suspended }: {
  client: WorkbenchClient; session: Session; object: CatalogItem; resultSetId: string; columns: Column[]; selectedRow: ResultRow | undefined;
  transaction: Transaction | null | undefined; onDirty: (dirty: boolean) => void; onRefresh: () => void; suspended: boolean;
}) {
  const binding = JSON.stringify([session.sessionId, session.connectionGeneration, session.mode, object.object, resultSetId]);
  const generation = JSON.stringify([session.sessionId, session.connectionGeneration]);
  const current = useRef(binding); current.current = binding;
  const [changes, setChanges] = useState<EditChange[]>([]), [basis, setBasis] = useState(binding), [insert, setInsert] = useState<EditValues>([]);
  const [entry, setEntry] = useState<"update" | "insert">("update"), [columnId, setColumnId] = useState(columns[0]?.columnId ?? "");
  const [review, setReview] = useState<EditReviewData | null>(null), [frozen, setFrozen] = useState<EditReviewData | null>(null), [operationId, setOperationId] = useState<string | null>(null);
  const [operationGeneration, setOperationGeneration] = useState(generation);
  const [busy, setBusy] = useState(false), [error, setError] = useState(""), [conflict, setConflict] = useState(false), [conflictId, setConflictId] = useState<string | null>(null);
  const [sent, setSent] = useState(false), [quarantined, setQuarantined] = useState(false), [discardDialog, setDiscardDialog] = useState(false), [discard, setDiscard] = useState(false);
  const dirty = changes.length > 0 || insert.length > 0, stale = dirty && basis !== binding;
  useEffect(() => { onDirty(dirty); }, [dirty, onDirty]);
  useEffect(() => { setReview(null); setBusy(false); }, [binding]);
  useEffect(() => { setQuarantined(false); setSent(false); }, [generation]);
  const capability = session.capabilities.verified_editing;
  const manualTransaction = transaction && !["committed", "rolled_back"].includes(transaction.state);
  const reason = !capability?.supported ? readonlyReasonText(capability?.reason) : session.mode === "read_only" ? "읽기 전용 모드에서는 변경 초안을 추가하지 않습니다." : object.kind !== "table" ? "일반 기본 테이블의 출처가 확인되지 않아 편집할 수 없습니다." : transaction === undefined ? "트랜잭션 상태를 확인해야 합니다." : manualTransaction ? "수동 트랜잭션을 명시적으로 정리한 뒤 별도 변경을 검토하세요." : session.state !== "ready" ? "연결이 실행 준비 상태가 아닙니다." : "";
  const disabled = !!reason || busy || stale || sent || quarantined || suspended;
  const column = columns.find((item) => item.columnId === columnId) ?? columns[0];
  const original = selectedRow?.cells[columns.findIndex((item) => item.columnId === column?.columnId)];
  const editable = original?.kind !== "large" && original?.kind !== "unsupported" ? original : undefined;
  const sessionRef = { sessionId: session.sessionId, connectionGeneration: session.connectionGeneration };
  function invalidate(): void { setReview(null); setError(""); if (!dirty) setBasis(binding); }
  function put(next: EditChange[]): boolean {
    if (next.length > 100 || new TextEncoder().encode(JSON.stringify(next)).byteLength > 512 * 1024) { setError("변경 초안은 100행·512KiB까지 추가할 수 있습니다. 원자적 변경을 자동 분할하지 않습니다."); return false; }
    invalidate(); setChanges(next); return true;
  }
  function addValue(value: EditableCell): void {
    if (disabled || !column?.editable) return;
    if (entry === "insert") {
      const values = [...insert.filter((item) => item.columnId !== column.columnId), { columnId: column.columnId, value }];
      if (new TextEncoder().encode(JSON.stringify([changes, values])).byteLength > 512 * 1024) { setError("변경 초안의 512KiB 상한에 도달했습니다."); return; }
      invalidate(); setInsert(values); return;
    }
    if (!selectedRow || !editable) return;
    const existing = changes.find((change) => change.kind !== "insert" && change.rowHandle === selectedRow.rowHandle);
    if (existing?.kind === "delete") { setError("이 행의 삭제 초안을 먼저 제거해 주세요."); return; }
    const values = [...(existing?.kind === "update" ? existing.values.filter((item) => item.columnId !== column.columnId) : []), { columnId: column.columnId, value }];
    const next: EditChange = { kind: "update", rowHandle: selectedRow.rowHandle, values };
    put([...changes.filter((change) => change.kind === "insert" || change.rowHandle !== selectedRow.rowHandle), next]);
  }
  function addDelete(): void {
    if (disabled || !selectedRow) return;
    if (changes.some((change) => change.kind !== "insert" && change.rowHandle === selectedRow.rowHandle)) { setError("이 행에 이미 있는 초안을 제거한 뒤 삭제 초안을 추가하세요."); return; }
    put([...changes, { kind: "delete", rowHandle: selectedRow.rowHandle }]);
  }
  async function prepare(): Promise<void> {
    if (disabled || changes.length === 0 || insert.length > 0) return;
    const captured = binding; setBusy(true); setError(""); setReview(null);
    try {
      const next = await client.request("edit.prepare", { session: sessionRef, object: object.object, changes });
      if (current.current !== captured) return;
      if (next.target.workspaceId !== session.workspaceId || next.target.databaseTargetId !== session.databaseTargetId || next.target.connectionProfileId !== session.connectionProfileId || next.object.catalogId !== object.object.catalogId || next.object.schemaVersion !== object.object.schemaVersion || next.target.actualEndpoint !== session.actualContext.endpoint || next.target.databaseName !== session.actualContext.database || next.target.databaseRole !== session.actualContext.role || next.target.purpose !== session.actualContext.purpose || next.changes.length > 100 || new TextEncoder().encode(JSON.stringify(next)).byteLength > 1024 * 1024) throw new WorkbenchError("WB_PAYLOAD_INVALID");
      setReview(next); setFrozen(next); setConflict(false); setConflictId(null);
    } catch (failure) { if (current.current === captured) setError(workbenchErrorText(failure)); }
    finally { if (current.current === captured) setBusy(false); }
  }
  async function apply(warnings: string[]): Promise<void> {
    if (!review || disabled) return; const captured = binding, approved = review; setBusy(true); setReview(null); setError("");
    const payload = { session: sessionRef, draftId: approved.draftId, reviewId: approved.reviewId, confirmedWarnings: warnings }; let dispatchStarted = false;
    try {
      const attempt = await client.request("operation.prepare", { intent: { command: "edit.apply", payload } });
      if (current.current !== captured) return;
      if (BigInt(attempt.expiresAtMs) <= BigInt(Date.now()) || BigInt(approved.expiresAtMs) <= BigInt(Date.now())) throw new WorkbenchError("WB_ATTEMPT_EXPIRED");
      setSent(true); dispatchStarted = true;
      const admission = await client.request("edit.apply", payload, undefined, attempt.attemptToken);
      if (current.current === captured) { setOperationGeneration(generation); setOperationId(admission.operationId); }
    } catch (failure) {
      if (current.current === captured) {
        setError(workbenchErrorText(failure));
        if (failure instanceof WorkbenchError) { if (failure.operationId) { setOperationGeneration(generation); setOperationId(failure.operationId); } if (failure.code === "WB_ROW_CONFLICT") { setConflict(true); setConflictId(failure.conflictId); } if (dispatchStarted && (failure.code === "WB_OUTCOME_UNKNOWN" || failure.code === "WB_TRANSPORT_UNAVAILABLE")) setQuarantined(true); }
      }
    } finally { if (current.current === captured) setBusy(false); }
  }
  async function discardDraft(): Promise<void> {
    if (!discard || busy) return; setBusy(true); setError("");
    try {
      if (frozen && !sent) { const reply = await client.request("edit.discard", { draftId: frozen.draftId }); if (!reply.discarded) throw new WorkbenchError("WB_IO"); }
      setChanges([]); setInsert([]); setBasis(binding); setReview(null); setFrozen(null); setDiscardDialog(false); setDiscard(false); setConflict(false); if (!quarantined) setSent(false);
    } catch (failure) { setError(workbenchErrorText(failure)); }
    finally { setBusy(false); }
  }
  function observed(operation: WorkbenchOperation): void { if (operationGeneration === generation && operation.state === "outcome_unknown") setQuarantined(true); }
  return <section className="wb-edit-draft" aria-label="검토할 데이터 변경 초안"><h3>데이터 변경 초안</h3>
    {reason && <p className="wb-notice" role="status">{reason}</p>}{stale && <p className="wb-notice" role="status">대상·구조·결과 또는 연결 세대가 바뀌었습니다. 이전 초안을 보존하며 그대로 적용하지 않습니다.</p>}
    {quarantined && <p className="wb-error" role="alert">이 변경의 결과를 확인하지 못했습니다. 현재 연결에서 재적용하지 않습니다. 인지 기록도 원래 결과를 바꾸지 않습니다.</p>}
    <div className="wb-query-actions"><label>초안 작업<select value={entry} disabled={disabled} onChange={(event) => setEntry(event.currentTarget.value as "update" | "insert")}><option value="update">선택 행의 값 변경</option><option value="insert">새 행 추가</option></select></label><label>열<select value={column?.columnId ?? ""} disabled={disabled} onChange={(event) => setColumnId(event.currentTarget.value)}>{columns.map((item) => <option key={item.columnId} value={item.columnId}>{item.name}</option>)}</select></label><button className="button button-secondary" type="button" disabled={disabled || !selectedRow} onClick={addDelete}>선택 행 삭제 초안 추가</button></div>
    {column && <>{(!column.editable || entry === "update" && !editable) && <p>{readonlyReasonText(column.readonlyReason)}</p>}<EditCellInput column={column} basis={entry === "update" ? editable : undefined} disabled={disabled || !column.editable || entry === "update" && (!selectedRow || !editable)} onValue={addValue} /></>}
    {entry === "insert" && <><p>새 행에 지정한 값 {insert.length}개 · 지정하지 않은 열은 검토에서 서버 기본값·생성값으로 구분합니다. SQL NULL은 별도 값입니다.</p><button className="button button-secondary" type="button" disabled={disabled} onClick={() => { if (put([...changes, { kind: "insert", values: insert }])) setInsert([]); }}>이 새 행을 변경 초안에 등록</button></>}
    <p>등록한 변경 {changes.length}건 / 100건 · 값과 초안은 현재 창에만 유지합니다.</p>
    <ol className="wb-edit-changes">{changes.map((change, index) => <li key={index}><span>{({ insert: "행 추가", update: "행 변경", delete: "행 삭제" })[change.kind]} · {change.kind === "insert" ? `지정 값 ${change.values.length}개` : change.rowHandle}</span><button type="button" className="wb-text-button" disabled={busy || sent || quarantined} onClick={() => { invalidate(); setChanges(changes.filter((_, ordinal) => ordinal !== index)); }}>이 초안 제거</button></li>)}</ol>
    <div className="wb-query-actions"><button className="button button-primary" type="button" disabled={disabled || changes.length === 0 || insert.length > 0} onClick={() => void prepare()}>생성한 변경 검토</button><button className="button button-secondary" type="button" disabled={!dirty || busy} onClick={() => { setDiscard(false); setDiscardDialog(true); }}>변경 초안 버리기</button></div>
    {sent && <p role="status">이 검토의 적용 요청은 다시 보내지 않습니다. 완료 결과는 작업 상태에서 확인하세요.</p>}
    {error && <p className="wb-error" role="alert">{error}</p>}
    {review && <EditReview key={review.reviewId} review={review} busy={busy} disabled={disabled} onApply={(warnings) => void apply(warnings)} onClose={() => setReview(null)} />}
    {conflict && frozen && <ConflictPanel key={`${frozen.reviewId}:${conflictId}`} client={client} session={sessionRef} review={frozen} conflictId={conflictId} onRefresh={onRefresh} />}
    {operationId && <OperationPanel key={operationId} client={client} operationId={operationId} onOperation={observed} />}
    {discardDialog && !suspended && <Dialog title="변경 초안을 버릴까요?" description="창에 있는 초안만 정리합니다. 대상 데이터의 롤백이나 미확인 작업의 삭제를 의미하지 않습니다." onClose={() => { if (!busy) setDiscardDialog(false); }}><label className="wb-check"><input type="checkbox" checked={discard} disabled={busy} onChange={(event) => setDiscard(event.currentTarget.checked)} />보존한 변경 초안을 버립니다.</label><div className="dialog-actions"><button className="button button-secondary" type="button" disabled={busy} onClick={() => setDiscardDialog(false)}>초안 유지</button><button className="button button-danger" type="button" disabled={busy || !discard} onClick={() => void discardDraft()}>확인하고 초안 버리기</button></div></Dialog>}
  </section>;
}
