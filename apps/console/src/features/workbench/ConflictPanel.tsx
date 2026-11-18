import { useRef, useState } from "react";
import { WorkbenchError, nativeFreshness, readonlyReasonText, workbenchErrorText, type WorkbenchClient } from "../../lib/workbench";
import type { ConflictObservation, EditReviewData, SessionRef } from "../../lib/workbenchTypes";

export function ConflictPanel({ client, session, review, conflictId, onRefresh }: { client: WorkbenchClient; session: SessionRef; review: EditReviewData; conflictId: string | null; onRefresh: () => void }) {
  const [observation, setObservation] = useState<ConflictObservation | null>(null), [index, setIndex] = useState(0), [fieldPage, setFieldPage] = useState(0), [busy, setBusy] = useState(false), [error, setError] = useState("");
  const binding = JSON.stringify([session, review.draftId, review.reviewId, conflictId]), current = useRef(binding); current.current = binding;
  async function observe(): Promise<void> {
    if (!conflictId || busy) return; const captured = binding; setBusy(true); setError("");
    try {
      const next = await client.request("edit.conflict.get", { session, conflictId });
      if (current.current !== captured) return;
      if (next.conflictId !== conflictId || next.draftId !== review.draftId || next.reviewId !== review.reviewId || next.basisObject.catalogId !== review.object.catalogId || next.basisObject.schemaVersion !== review.object.schemaVersion || next.rows.length > 100) throw new WorkbenchError("WB_PAYLOAD_INVALID");
      if (new Set(next.rows.map((row) => row.changeIndex)).size !== next.rows.length || next.rows.some((row) => !Number.isSafeInteger(row.changeIndex) || row.changeIndex < 0 || row.changeIndex >= review.changes.length || review.changes[row.changeIndex].rowHandle !== row.rowHandle)) throw new WorkbenchError("WB_PAYLOAD_INVALID");
      setObservation(next);
    } catch (failure) { if (current.current === captured) setError(workbenchErrorText(failure)); }
    finally { if (current.current === captured) setBusy(false); }
  }
  const row = observation?.rows[index];
  const basis = row ? review.changes[row.changeIndex] : undefined;
  return <section className="wb-conflict" aria-label="충돌 관측과 보존한 초안"><h3>변경 기준이 충돌했습니다.</h3><p>원본과 초안을 유지합니다. 아래 값은 실패한 적용 중 실제로 관측한 값이며 실시간 현재값을 보장하지 않습니다. 롤백 성공 여부는 작업 상태와 별개로 확인하세요.</p>
    <div className="wb-query-actions"><button className="button button-secondary" type="button" disabled={!conflictId || busy} onClick={() => void observe()}>실패 시점의 관측값 읽기</button><button className="button button-secondary" type="button" disabled={busy} onClick={onRefresh}>초안 유지하고 원본 별도 조회</button></div>
    {!observation ? <p>현재 관측값 미조회 · 초안은 검토 기준에 남아 있습니다.</p> : <><p>{nativeFreshness(observation.observedAtMs)} · 관측 시각 {observation.observedAtMs} · 관측 구조 버전 {observation.currentSchemaVersion ?? "미확인"}</p><label>관측한 행<select value={index} onChange={(event) => { setIndex(Number(event.currentTarget.value)); setFieldPage(0); }}>{observation.rows.map((item, ordinal) => <option key={ordinal} value={ordinal}>{ordinal + 1} · {item.rowHandle ?? "새 행 관련 관측"}</option>)}</select></label>
      <div className="wb-edit-comparison"><div><strong>보존한 원본·초안</strong>{basis ? <><pre>{JSON.stringify(basis.original, null, 2)}</pre><pre>{JSON.stringify(basis.draft, null, 2)}</pre></> : <p>이 관측을 특정 원본 행에 자동 대응시키지 않습니다. 변경 검토의 초안을 확인하세요.</p>}</div><div><strong>실패 시점의 관측값</strong>{row?.current.kind === "present" ? <>{row.current.values.slice(fieldPage * 50, (fieldPage + 1) * 50).map(({ columnId, value }) => <pre key={columnId}>{columnId} · {JSON.stringify(value)}</pre>)}<button type="button" className="wb-text-button" disabled={fieldPage === 0} onClick={() => setFieldPage(fieldPage - 1)}>이전 관측 열</button><button type="button" className="wb-text-button" disabled={(fieldPage + 1) * 50 >= row.current.values.length} onClick={() => setFieldPage(fieldPage + 1)}>다음 관측 열</button></> : row?.current.kind === "missing" ? <p>관측 시점에 행이 없었습니다.</p> : <p>관측값을 확보하지 못했습니다. {row?.current.kind === "unavailable" ? readonlyReasonText(row.current.reason) : "행 관측 없음"}</p>}</div></div></>}
    {error && <p className="wb-error" role="alert">{error}</p>}
  </section>;
}
