import { useEffect, useState } from "react";
import { Dialog } from "../../components/Dialog";
import { nativeFreshness, workbenchErrorText, type WorkbenchClient } from "../../lib/workbench";
import type { Scope, WorkbenchOperation, WorkbenchPage } from "../../lib/workbenchTypes";
import { OPERATION_LABEL, OperationPanel } from "./OperationPanel";
import { MovementProgressPanel } from "./MovementProgressPanel";

const KIND: Record<WorkbenchOperation["kind"], string> = { query: "SQL 실행", manual_transaction: "수동 트랜잭션", verified_apply: "검토한 변경", import_job: "파일 가져오기", import_batch: "가져오기 배치", export_job: "파일 내보내기", protection_link: "백업 보호 연결" };
export function OperationHistory({ client, scope, suspended, onContinueImport, onClose }: { client: WorkbenchClient; scope: Scope; suspended: boolean; onContinueImport?: (parentId: string) => void; onClose: () => void }) {
  const [state, setState] = useState<string | null>("outcome_unknown"), [cursor, setCursor] = useState<string | null>(null);
  const [page, setPage] = useState<WorkbenchPage<WorkbenchOperation> | null>(null), [selected, setSelected] = useState<string | null>(null), [error, setError] = useState(""), [busy, setBusy] = useState(false), [revision, setRevision] = useState(0);
  const scopeKey = JSON.stringify(scope);
  useEffect(() => { setCursor(null); setSelected(null); }, [scopeKey]);
  useEffect(() => {
    let active = true; setPage(null); setBusy(true); setError("");
    void client.request("operation.list", { scope, state, page: { cursor, limit: 20 } }).then((value) => { if (active) setPage(value); }).catch((failure: unknown) => { if (active) setError(workbenchErrorText(failure)); }).finally(() => { if (active) setBusy(false); });
    return () => { active = false; };
  }, [client, scopeKey, state, cursor, revision]);
  if (suspended) return null;
  return <Dialog title="이 대상의 작업 기록" description="사용자 장부의 상태와 효과 증거를 조회합니다. 이전 SQL을 다시 실행하거나 과거 실행의 중지 권한을 복원하지 않습니다." onClose={onClose}>
    <div className="wb-query-actions"><label>조회할 상태<select value={state ?? "all"} disabled={busy} onChange={(event) => { setState(event.currentTarget.value === "all" ? null : event.currentTarget.value); setCursor(null); setSelected(null); }}><option value="outcome_unknown">결과 미확인</option><option value="all">모든 상태</option></select></label><button className="button button-secondary" type="button" disabled={busy} onClick={() => { setCursor(null); setRevision(revision + 1); }}>최신 기록 조회</button></div>
    {busy && <p role="status">작업 기록을 읽는 중입니다.</p>}
    {page && page.items.length === 0 && <p role="status">선택한 범위의 작업 기록이 없습니다.</p>}
    <div className="wb-operation-history">{page?.items.map((operation) => <button className="wb-saved-item" type="button" key={operation.operationId} aria-pressed={selected === operation.operationId} onClick={() => setSelected(operation.operationId)}><strong>{KIND[operation.kind]} · {OPERATION_LABEL[operation.state]}</strong><span className="mono">{operation.operationId}</span><small>{nativeFreshness(operation.updatedAtMs)}</small></button>)}</div>
    <button className="button button-secondary" type="button" disabled={busy || !page?.nextCursor} onClick={() => { setCursor(page?.nextCursor ?? null); setSelected(null); }}>다음 작업 기록</button>
    {selected && (page?.items.find((operation) => operation.operationId === selected)?.kind === "import_job" ? <MovementProgressPanel key={selected} client={client} operationId={selected} kind="import" onContinue={onContinueImport} /> : page?.items.find((operation) => operation.operationId === selected)?.kind === "export_job" ? <MovementProgressPanel key={selected} client={client} operationId={selected} kind="export" /> : <OperationPanel key={selected} client={client} operationId={selected} />)}
    {error && <p className="wb-error" role="alert">{error}</p>}
  </Dialog>;
}
