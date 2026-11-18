import { useEffect, useRef, useState } from "react";
import { WorkbenchError, workbenchErrorText, type WorkbenchClient } from "../../lib/workbench";
import type { MovementStatus } from "../../lib/workbenchTypes";
import { OPERATION_LABEL, OperationPanel } from "./OperationPanel";

export function MovementProgressPanel({ client, operationId, kind, onContinue }: { client: WorkbenchClient; operationId: string; kind: "import" | "export"; onContinue?: (parentId: string) => void }) {
  const [status, setStatus] = useState<MovementStatus | null>(null), [cursor, setCursor] = useState<string | null>(null), [error, setError] = useState(""), [cancelStatus, setCancelStatus] = useState("");
  const [busy, setBusy] = useState(false), [removeConfirmed, setRemoveConfirmed] = useState(false);
  const current = useRef(operationId); current.current = operationId;
  useEffect(() => {
    let active = true, pending = false;
    async function refresh(): Promise<void> {
      if (pending) return; pending = true;
      try { const next = await client.request("movement.status", { operationId, page: { cursor, limit: 20 } }); if (next.operation.operationId !== operationId || next.children.items.length > 20) throw new WorkbenchError("WB_PAYLOAD_INVALID"); if (active) { setStatus(next); setError(""); } }
      catch (failure) { if (active) setError(workbenchErrorText(failure)); }
      finally { pending = false; }
    }
    void refresh(); const timer = window.setInterval(() => void refresh(), 2000); return () => { active = false; window.clearInterval(timer); };
  }, [client, operationId, cursor]);
  async function cancel(): Promise<void> {
    if (busy) return; const captured = operationId; setBusy(true);
    try {
      const reply = kind === "import" ? await client.request("import.cancel", { parentOperationId: operationId }) : await client.request("export.cancel", { operationId });
      if (current.current === captured) setCancelStatus(reply.cancelRequested ? "중지를 요청했습니다. 이미 확인된 커밋은 유지되며 진행 중인 배치의 롤백은 별도 증거가 필요합니다." : "중지 요청을 적용하지 않았습니다. 현재 작업 상태를 확인하세요.");
    } catch (failure) { if (current.current === captured) setError(workbenchErrorText(failure)); }
    finally { if (current.current === captured) setBusy(false); }
  }
  async function removePartial(): Promise<void> {
    if (!removeConfirmed || busy) return; setBusy(true); const captured = operationId;
    try { const reply = await client.request("movement.partial.remove", { operationId }); if (current.current === captured) { setCancelStatus(reply.removed ? "네이티브가 이 작업의 부분 파일 제거를 확인했습니다." : "부분 파일 제거를 확인하지 못했습니다."); setRemoveConfirmed(false); } }
    catch (failure) { if (current.current === captured) setError(workbenchErrorText(failure)); }
    finally { if (current.current === captured) setBusy(false); }
  }
  const file = status?.file;
  const completeFile = file?.state === "complete" && file.finalized;
  const unresolved = status ? BigInt(status.progress.unresolvedBatches) !== 0n : true;
  const activeOperation = !!status && ["accepted", "dispatching", "running", "commit_requested"].includes(status.operation.state);
  return <section className="wb-movement-progress" aria-label={kind === "import" ? "가져오기 배치 진행" : "내보내기 파일 진행"}><h3>{kind === "import" ? "가져오기 진행과 배치 증거" : "내보내기 파일 상태"}</h3>
    {status && <><p>{OPERATION_LABEL[status.operation.state]} · 처리 {status.progress.rowsProcessed}행 / {status.progress.bytesProcessed} bytes · 전체 바이트 {status.progress.totalBytes ?? "미확인"}</p>
      {kind === "import" && <><p>커밋 확인 {status.progress.confirmedCommittedRows}행 / {status.progress.confirmedCommittedBatches}배치 · 미확정 {status.progress.unresolvedBatches}배치</p>{unresolved && <p className="wb-error" role="alert">미확정 배치가 있어 후속 배치를 시작하지 않습니다. 사용자 인지 기록도 이 차단을 해제하지 않습니다.</p>}<div className="wb-statement-list">{status.children.items.map((child) => <div className="wb-statement" key={child.operationId}><strong>배치 {child.batchIndex} · {OPERATION_LABEL[child.state]}</strong><span>원본 바이트 [{child.sourceByteStart}, {child.sourceByteEnd}) · 행 [{child.sourceRowStart}, {child.sourceRowEnd})</span><span>커밋 확인 {child.rowsCommitted ?? "미확인"}행</span></div>)}</div><button className="button button-secondary" type="button" disabled={!status.children.nextCursor} onClick={() => setCursor(status.children.nextCursor)}>다음 배치 증거</button>{onContinue && <button className="button button-secondary" type="button" disabled={unresolved || activeOperation || status.progress.nextBatchIndex === null || busy} onClick={() => onContinue(operationId)}>새 파일 확인 후 미전송 구간 검토</button>}</>}
      {file && <div className="wb-file-artifact"><strong>{completeFile ? "선택한 내보내기 범위의 파일 완료 확인" : ({ not_started: "파일 작성 전", partial: "부분 파일", complete: "파일 완료 근거 확인 필요", unconfirmed: "파일 완료 미확인", removed: "부분 파일 제거됨" })[file.state]}</strong><p className="mono">{file.displayPath}</p>{file.partialDisplayPath && <p>부분 파일 위치 <code>{file.partialDisplayPath}</code></p>}<p>원래 조회 상태 {({ complete: "조회 완료", capped: "조회 상한 도달", cancelled: "취소 확인됨", interrupted: "수신 중단", partial: "일부 결과", failed: "조회 실패" })[file.sourceCompleteness]} · 기록 {file.rowsWritten}행 / {file.bytesWritten} bytes · 완전한 전체 데이터 여부는 원래 조회 범위와 별개입니다.</p>{file.state === "partial" && !activeOperation && <><label className="wb-check"><input type="checkbox" checked={removeConfirmed} disabled={busy} onChange={(event) => setRemoveConfirmed(event.currentTarget.checked)} />이 작업의 보존한 부분 파일만 제거합니다.</label><button className="button button-danger" type="button" disabled={busy || !removeConfirmed} onClick={() => void removePartial()}>확인하고 부분 파일 제거 요청</button></>}</div>}
    </>}
    <button className="button button-secondary" type="button" disabled={busy || !activeOperation} onClick={() => void cancel()}>이 파일 작업 중지 요청</button>
    {cancelStatus && <p role="status">{cancelStatus}</p>}{error && <p className="wb-error" role="alert">{error}</p>}
    <OperationPanel client={client} operationId={operationId} />
  </section>;
}
