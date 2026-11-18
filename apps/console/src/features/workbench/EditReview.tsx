import { useEffect, useState } from "react";
import { cellText } from "../../lib/workbench";
import type { EditReviewData } from "../../lib/workbenchTypes";
import { warningText } from "./QueryEditor";

export function EditReview({ review, busy, disabled, onApply, onClose }: { review: EditReviewData; busy: boolean; disabled: boolean; onApply: (warnings: string[]) => void; onClose: () => void }) {
  const [confirmed, setConfirmed] = useState(false), [warnings, setWarnings] = useState<string[]>([]), [clock, setClock] = useState(Date.now);
  const [parameterPage, setParameterPage] = useState(0), [changeIndex, setChangeIndex] = useState(0), [fieldPage, setFieldPage] = useState(0);
  useEffect(() => { const timer = window.setInterval(() => setClock(Date.now()), 1000); return () => window.clearInterval(timer); }, []);
  const expired = BigInt(review.expiresAtMs) <= BigInt(clock);
  return <section className="wb-edit-review" aria-label="생성한 변경의 대상과 매개변수 검토"><h3>변경 검토</h3>
    <p className="mono">{review.target.actualEndpoint} / {review.target.databaseName} · {review.target.databaseRole || "파일 권한"} · {review.target.purpose === "restored" ? "복원 대상" : "원본"}</p>
    <p>현재 Workspace {review.target.workspaceId} · 변경 {review.changes.length}건 · 전체 변경을 하나의 독립 트랜잭션으로 요청합니다. 지원하는 트랜잭션 효과만 검증합니다.</p>
    <details><summary>생성한 SQL · 값은 별도 바인딩</summary>{review.statementPreviews.map((sql, index) => <pre className="wb-wire-sql" key={index}>{sql}</pre>)}</details>
    <details><summary>형식을 포함한 매개변수 {review.parameterPreview.length}개</summary><ol start={parameterPage * 100 + 1}>{review.parameterPreview.slice(parameterPage * 100, (parameterPage + 1) * 100).map((value, index) => <li key={index}><code>{value.kind}: {cellText(value)}</code></li>)}</ol><button type="button" className="wb-text-button" disabled={parameterPage === 0} onClick={() => setParameterPage(parameterPage - 1)}>이전 매개변수</button><button type="button" className="wb-text-button" disabled={(parameterPage + 1) * 100 >= review.parameterPreview.length} onClick={() => setParameterPage(parameterPage + 1)}>다음 매개변수</button></details>
    <label>검토할 변경<select value={changeIndex} onChange={(event) => { setChangeIndex(Number(event.currentTarget.value)); setFieldPage(0); }}>{review.changes.map((change, index) => <option key={index} value={index}>{index + 1} · {({ insert: "행 추가", update: "행 변경", delete: "행 삭제" })[change.kind]}</option>)}</select></label>
    {review.changes.slice(changeIndex, changeIndex + 1).map((change) => <div key={changeIndex}><div className="wb-edit-comparison"><div><strong>검토 기준 원본</strong>{change.original.slice(fieldPage * 50, (fieldPage + 1) * 50).map(({ columnId, value }) => <pre key={columnId}>{columnId} · {JSON.stringify(value)}</pre>)}</div><div><strong>변경 초안</strong>{change.draft.slice(fieldPage * 50, (fieldPage + 1) * 50).map(({ columnId, value }) => <pre key={columnId}>{columnId} · {JSON.stringify(value)}</pre>)}</div></div><button type="button" className="wb-text-button" disabled={fieldPage === 0} onClick={() => setFieldPage(fieldPage - 1)}>이전 검토 열</button><button type="button" className="wb-text-button" disabled={(fieldPage + 1) * 50 >= Math.max(change.original.length, change.draft.length)} onClick={() => setFieldPage(fieldPage + 1)}>다음 검토 열</button>{change.omittedDefaultColumnIds.length > 0 && <p>누락한 열은 서버 기본값·생성값으로 검토합니다: {change.omittedDefaultColumnIds.join(", ")}</p>}</div>)}
    {review.warnings.map((warning) => <label key={warning} className="wb-check"><input type="checkbox" checked={warnings.includes(warning)} disabled={busy} onChange={(event) => setWarnings(event.currentTarget.checked ? [...warnings, warning] : warnings.filter((item) => item !== warning))} />{warningText(warning)}</label>)}
    <label className="wb-check"><input type="checkbox" checked={confirmed} disabled={busy || disabled || expired} onChange={(event) => setConfirmed(event.currentTarget.checked)} />위 실제 대상의 원본·초안·SQL·매개변수를 확인하고 이 변경을 요청합니다.</label>
    {expired && <p role="status">검토 기한이 지났습니다. 새 검토가 필요합니다.</p>}
    <div className="wb-query-actions"><button className="button button-secondary" type="button" disabled={busy} onClick={onClose}>검토 닫고 초안 유지</button><button className="button button-danger" type="button" disabled={busy || disabled || expired || !confirmed || !review.warnings.every((warning) => warnings.includes(warning))} onClick={() => onApply(warnings)}>검토한 변경 적용 요청</button></div>
  </section>;
}
