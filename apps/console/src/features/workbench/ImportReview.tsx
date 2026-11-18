import { useEffect, useRef, useState } from "react";
import { Dialog } from "../../components/Dialog";
import { WorkbenchError, workbenchErrorText, type WorkbenchClient } from "../../lib/workbench";
import type { CatalogItem, Column, ImportContinuationReview, ImportPreview, MovementFormat, MovementSource, Session, Transaction } from "../../lib/workbenchTypes";
import { warningText } from "./QueryEditor";
import { VirtualResultGrid, type SelectionAnchor } from "./VirtualResultGrid";
import { RowInspector } from "./RowInspector";
import { MODE_LABEL, transactionLabel } from "./TransactionBar";

export function ImportReview({ client, session, object, columns, transaction, parentOperationId = null, suspended, onAdmitted, onClose }: {
  client: WorkbenchClient; session: Session; object: CatalogItem; columns: Column[]; transaction: Transaction | null | undefined;
  parentOperationId?: string | null; suspended: boolean; onAdmitted: (operationId: string) => void; onClose: () => void;
}) {
  const [format, setFormat] = useState<MovementFormat>("csv"), [source, setSource] = useState<MovementSource | null>(null), [mapping, setMapping] = useState<Record<string, string>>({});
  const [sourceKey, setSourceKey] = useState(""), [columnId, setColumnId] = useState(columns[0]?.columnId ?? "");
  const [preview, setPreview] = useState<ImportPreview | null>(null), [continuation, setContinuation] = useState<ImportContinuationReview | null>(null), [selection, setSelection] = useState<SelectionAnchor | null>(null);
  const [warnings, setWarnings] = useState<string[]>([]), [confirmed, setConfirmed] = useState(false), [busy, setBusy] = useState(false), [dispatched, setDispatched] = useState(false), [error, setError] = useState("");
  const [clock, setClock] = useState(Date.now);
  const ref = { sessionId: session.sessionId, connectionGeneration: session.connectionGeneration }, binding = JSON.stringify([ref, object.object, session.mode]);
  const current = useRef(binding), held = useRef<string | null>(null), claimed = useRef(false); current.current = binding;
  function release(): void { const handle = held.current; held.current = null; if (handle && !claimed.current) void client.request("movement.file.release", { handle }).catch(() => undefined); }
  useEffect(() => { current.current = binding; return () => { current.current = ""; release(); }; }, [client]);
  useEffect(() => { release(); setSource(null); setPreview(null); setContinuation(null); setConfirmed(false); setWarnings([]); }, [binding]);
  useEffect(() => { if (!continuation) return; const timer = window.setInterval(() => setClock(Date.now()), 1000); return () => window.clearInterval(timer); }, [continuation]);
  const locked = busy || dispatched || suspended;
  const manual = !!transaction && !["committed", "rolled_back"].includes(transaction.state);
  const reason = session.mode === "read_only" ? "가져오기는 데이터 변경 모드 또는 전체 SQL 모드가 필요합니다." : transaction === undefined ? "트랜잭션 상태를 확인해야 합니다." : manual ? "수동 트랜잭션을 먼저 명시적으로 정리하세요." : session.state !== "ready" ? "현재 연결이 실행 준비 상태가 아닙니다." : object.kind !== "table" ? "검증한 일반 기본 테이블만 가져오기 대상으로 사용할 수 있습니다." : "";
  function invalidate(): void { setPreview(null); setContinuation(null); setWarnings([]); setConfirmed(false); setError(""); }
  async function pick(): Promise<void> {
    if (locked) return; const captured = binding; setBusy(true); setError(""); release(); setSource(null); invalidate(); claimed.current = false;
    try {
      const next = await client.request("movement.source.pick", { session: ref, format });
      if (current.current !== captured) { if (next.source) void client.request("movement.file.release", { handle: next.source.sourceHandle }).catch(() => undefined); return; }
      if (next.source && (next.source.columns.length > 512 || new Set(next.source.columns.map((column) => column.sourceKey)).size !== next.source.columns.length || next.source.columns.some((column) => !Number.isSafeInteger(column.ordinal) || column.ordinal < 0 || column.ordinal >= 512))) throw new WorkbenchError("WB_PAYLOAD_INVALID");
      held.current = next.source?.sourceHandle ?? null; setSource(next.source); setMapping({}); setSourceKey(next.source?.columns[0]?.sourceKey ?? "");
    } catch (failure) { if (current.current === captured) setError(workbenchErrorText(failure)); }
    finally { if (current.current === captured) setBusy(false); }
  }
  function mapColumn(): void {
    if (locked || !sourceKey || !columnId || !source?.columns.some((column) => column.sourceKey === sourceKey) || !columns.some((column) => column.columnId === columnId)) return;
    if (Object.entries(mapping).some(([key, value]) => key !== sourceKey && value === columnId)) { setError("같은 대상 열에 두 원본 열을 연결할 수 없습니다."); return; }
    invalidate(); setMapping({ ...mapping, [sourceKey]: columnId });
  }
  async function inspect(cursor: string | null = null): Promise<void> {
    if (!source || locked || Object.keys(mapping).length === 0) return;
    const captured = binding; setBusy(true); setError(""); setConfirmed(false); setWarnings([]); setContinuation(null);
    try {
      const next = await client.request("import.preview", { session: ref, object: object.object, sourceHandle: source.sourceHandle, format, mapping, page: { cursor, limit: 100 } });
      if (current.current !== captured) return;
      if (next.columns.length > 512 || next.rows.items.length > 100 || next.rows.items.some((row) => row.cells.length !== next.columns.length)) throw new WorkbenchError("WB_PAYLOAD_INVALID");
      setPreview(next); setSelection(null);
    } catch (failure) { if (current.current === captured) setError(workbenchErrorText(failure)); }
    finally { if (current.current === captured) setBusy(false); }
  }
  async function reviewContinuation(): Promise<void> {
    if (!preview || !parentOperationId || locked) return; const captured = binding; setBusy(true); setError(""); setConfirmed(false); setWarnings([]);
    try { const next = await client.request("import.continue.review", { session: ref, parentOperationId, previewId: preview.previewId }); if (next.parentOperationId !== parentOperationId) throw new WorkbenchError("WB_PAYLOAD_INVALID"); if (current.current === captured) { setContinuation(next); setClock(Date.now()); } }
    catch (failure) { if (current.current === captured) setError(workbenchErrorText(failure)); }
    finally { if (current.current === captured) setBusy(false); }
  }
  const allWarnings = [...new Set([...(preview?.warnings ?? []), ...(continuation?.warnings ?? [])])];
  const continueBlocked = !!parentOperationId && (!continuation?.nextUnsent || BigInt(continuation.unresolvedBatches) !== 0n || BigInt(continuation.expiresAtMs) <= BigInt(clock));
  async function start(): Promise<void> {
    if (locked || !!reason || !preview || !confirmed || continueBlocked || !allWarnings.every((warning) => warnings.includes(warning))) return;
    const captured = binding; setBusy(true); setError("");
    const intent = parentOperationId && continuation ? { command: "import.continue" as const, payload: { session: ref, parentOperationId, reviewId: continuation.reviewId } } : { command: "import.start" as const, payload: { session: ref, previewId: preview.previewId, confirmedWarnings: warnings } };
    try {
      const attempt = await client.request("operation.prepare", { intent });
      if (current.current !== captured) return;
      if (BigInt(attempt.expiresAtMs) <= BigInt(Date.now())) throw new WorkbenchError("WB_ATTEMPT_EXPIRED");
      setDispatched(true); claimed.current = true;
      const result = intent.command === "import.start" ? await client.request("import.start", intent.payload, undefined, attempt.attemptToken) : await client.request("import.continue", intent.payload, undefined, attempt.attemptToken);
      if (current.current === captured) { onAdmitted(parentOperationId ?? result.operationId); onClose(); }
    } catch (failure) { if (current.current === captured) { setError(workbenchErrorText(failure)); if (failure instanceof WorkbenchError && failure.operationId) onAdmitted(parentOperationId ?? failure.operationId); } }
    finally { if (current.current === captured) setBusy(false); }
  }
  if (suspended) return null;
  return <Dialog title={parentOperationId ? "미전송 구간을 새로 검토" : "파일 가져오기 검토"} description="파일은 네이티브에서 스트리밍으로 읽습니다. 검토한 매핑만 바인딩 INSERT로 요청하고 각 배치를 독립 트랜잭션으로 실행합니다." onClose={() => { if (!busy) onClose(); }}><div className="wb-movement-dialog">
    <p className="mono">{session.actualContext.endpoint} / {session.actualContext.database} · {session.actualContext.role || "파일 권한"} · {session.actualContext.purpose === "restored" ? "복원 대상" : "원본"} · {object.name}</p>
    <p>모드 {MODE_LABEL[session.mode]} · {transactionLabel(transaction)}</p>
    <div className="wb-query-actions"><label>파일 형식<select value={format} disabled={locked} onChange={(event) => { release(); setSource(null); setMapping({}); setFormat(event.currentTarget.value as MovementFormat); invalidate(); }}><option value="csv">CSV</option><option value="tsv">TSV</option><option value="json">형식 포함 JSON</option></select></label><button className="button button-secondary" type="button" disabled={locked} onClick={() => void pick()}>원본 파일 선택</button></div>
    <p>UTF-8 · {format === "json" ? "JSON은 NULL 태그·정밀 숫자·바이트·시간 형식을 보존합니다." : `${format === "csv" ? "쉼표" : "탭"} 구분 · SQL NULL은 \\N으로 빈 문자열과 구분하며 텍스트의 앞쪽 역슬래시는 별도로 이스케이프합니다.`}</p>
    {source && <><p>{source.displayName} · {source.byteLength} bytes · 원본 열 {source.columns.length}개</p><div className="wb-query-actions"><label>원본 열<select value={sourceKey} disabled={locked} onChange={(event) => setSourceKey(event.currentTarget.value)}>{source.columns.map((column) => <option key={column.sourceKey} value={column.sourceKey}>{column.ordinal + 1} · {column.name}{column.typeHint ? ` (${column.typeHint})` : ""}</option>)}</select></label><label>대상 열<select value={columnId} disabled={locked} onChange={(event) => setColumnId(event.currentTarget.value)}>{columns.map((column) => <option key={column.columnId} value={column.columnId} disabled={column.readonlyReason === "generated_column"}>{column.name} · {column.typeName}</option>)}</select></label><button className="button button-secondary" type="button" disabled={locked} onClick={mapColumn}>이 열 연결</button></div><details><summary>확인할 열 매핑 {Object.keys(mapping).length}개</summary><ul>{Object.entries(mapping).map(([key, value]) => <li key={key}>{source.columns.find((column) => column.sourceKey === key)?.name} → {columns.find((column) => column.columnId === value)?.name}<button className="wb-text-button" type="button" disabled={locked} onClick={() => { const next = { ...mapping }; delete next[key]; setMapping(next); invalidate(); }}>매핑 제거</button></li>)}</ul></details><button className="button button-secondary" type="button" disabled={locked || Object.keys(mapping).length === 0} onClick={() => void inspect()}>형식과 매핑 미리보기</button></>}
    {preview && <><VirtualResultGrid resultSetId={preview.previewId} columns={preview.columns} rows={preview.rows.items} selection={selection} onSelect={setSelection} /><details><summary>미리보기 선택 행의 원문과 형식</summary><RowInspector client={client} identity={null} row={preview.rows.items.find((row) => row.rowHandle === selection?.rowHandle)} columns={preview.columns} selection={selection} /></details><p>현재 미리보기 {preview.rows.items.length}행 · 전체 수 {preview.rows.count.kind === "unknown" ? "미확인" : preview.rows.count.value} · 파일 전체는 창에 적재하지 않습니다.</p><button className="button button-secondary" type="button" disabled={locked || !preview.rows.nextCursor} onClick={() => void inspect(preview.rows.nextCursor)}>다음 미리보기</button>{parentOperationId && <button className="button button-secondary" type="button" disabled={locked} onClick={() => void reviewContinuation()}>다음 미전송 구간의 새 검토 받기</button>}</>}
    {continuation && <p>커밋 확인 {continuation.confirmedCommittedRows}행 · 미확정 {continuation.unresolvedBatches}배치 · 다음 미전송 배치 {continuation.nextUnsent?.batchIndex ?? "없음"}{continuation.nextUnsent && ` / 시작 바이트 ${continuation.nextUnsent.byteStart} / 시작 행 ${continuation.nextUnsent.rowStart}`}</p>}
    {allWarnings.map((warning) => <label className="wb-check" key={warning}><input type="checkbox" checked={warnings.includes(warning)} disabled={locked} onChange={(event) => setWarnings(event.currentTarget.checked ? [...warnings, warning] : warnings.filter((item) => item !== warning))} />{warningText(warning)}</label>)}
    {reason && <p className="wb-notice" role="status">{reason}</p>}
    <label className="wb-check"><input type="checkbox" checked={confirmed} disabled={locked || !preview || !!reason || continueBlocked} onChange={(event) => setConfirmed(event.currentTarget.checked)} />실제 대상·매핑·값 형식을 확인했습니다. 앞서 확인된 커밋은 후속 실패·취소로 되돌아가지 않으며 미확정 배치 뒤에는 계속하지 않습니다.</label>
    <button className="button button-primary" type="button" disabled={locked || !preview || !!reason || !confirmed || continueBlocked || !allWarnings.every((warning) => warnings.includes(warning))} onClick={() => void start()}>검토한 가져오기 요청</button>
    {dispatched && <p role="status">요청을 다시 보내지 않습니다. 작업 기록에서 상태를 확인하세요.</p>}{error && <p className="wb-error" role="alert">{error}</p>}
  </div></Dialog>;
}
