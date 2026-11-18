import { useEffect, useRef, useState } from "react";
import { Dialog } from "../../components/Dialog";
import { WorkbenchError, readonlyReasonText, workbenchErrorText, type WorkbenchClient } from "../../lib/workbench";
import type { ExportStartInput, MovementDestination, MovementFormat, NativeCounterU64, QueryReview, Session, ShownResult, Transaction } from "../../lib/workbenchTypes";
import { MAX_SQL_BYTES, sqlBytes, warningText } from "./QueryEditor";
import { MODE_LABEL, transactionLabel } from "./TransactionBar";

export function ExportDialog({ client, session, engine, shown, transaction, suspended, onAdmitted, onClose }: { client: WorkbenchClient; session: Session; engine: string; shown: ShownResult | null; transaction: Transaction | null | undefined; suspended: boolean; onAdmitted: (operationId: string) => void; onClose: () => void }) {
  const [fixedShown] = useState(shown), [source, setSource] = useState<"shown" | "fresh">(shown ? "shown" : "fresh"), [format, setFormat] = useState<MovementFormat>("csv");
  const [destination, setDestination] = useState<MovementDestination | null>(null), [sql, setSql] = useState(""), [review, setReview] = useState<QueryReview | null>(null);
  const [snapshot, setSnapshot] = useState<{ snapshotId: string; expiresAtMs: NativeCounterU64 } | null>(null), [snapshotRequested, setSnapshotRequested] = useState(false);
  const [formula, setFormula] = useState<"escape" | "raw_confirmed">("escape"), [rawConfirmed, setRawConfirmed] = useState(false), [partialPolicy, setPartialPolicy] = useState<"retain" | "remove_on_cancel">("retain");
  const [warnings, setWarnings] = useState<string[]>([]), [confirmed, setConfirmed] = useState(false), [busy, setBusy] = useState(false), [dispatched, setDispatched] = useState(false), [error, setError] = useState(""), [clock, setClock] = useState(Date.now);
  const ref = { sessionId: session.sessionId, connectionGeneration: session.connectionGeneration }, binding = JSON.stringify([ref, session.mode, session.descriptorRevision, engine]), current = useRef(binding), destinationRef = useRef<string | null>(null), snapshotRef = useRef<typeof snapshot>(null), claimed = useRef(false); current.current = binding; snapshotRef.current = snapshot;
  function releaseDestination(): void { const handle = destinationRef.current; destinationRef.current = null; if (handle && !claimed.current) void client.request("movement.file.release", { handle }).catch(() => undefined); }
  useEffect(() => { current.current = binding; return () => { current.current = ""; releaseDestination(); const held = snapshotRef.current; if (held && !claimed.current) void client.request("snapshot.end", { session: ref, snapshotId: held.snapshotId }).catch(() => undefined); }; }, [client, binding]);
  useEffect(() => { setReview(null); setConfirmed(false); setWarnings([]); setDestination(null); setSnapshot(null); }, [binding]);
  useEffect(() => { const timer = window.setInterval(() => setClock(Date.now()), 1000); return () => window.clearInterval(timer); }, []);
  const locked = busy || dispatched || suspended;
  const staleShown = !fixedShown || fixedShown.identity.sessionId !== session.sessionId || fixedShown.identity.connectionGeneration !== session.connectionGeneration || fixedShown.identity.workspaceId !== session.workspaceId || fixedShown.identity.databaseTargetId !== session.databaseTargetId || fixedShown.range.rowCount < 1 || fixedShown.range.rowCount > 200;
  const snapshotExpired = !!snapshot && BigInt(snapshot.expiresAtMs) <= BigInt(clock);
  const safeReview = !!review && review.requiredMode === "read_only" && !review.warnings.some((warning) => ["function_effects_unproven", "unknown_effects", "explain_analyze_executes", "locking_read", "session_retired_after_execution"].includes(warning));
  const freshContextReady = session.mode === "read_only" && transaction !== undefined && (!transaction || ["committed", "rolled_back"].includes(transaction.state));
  const snapshotIsolation = engine === "sqlite" ? "serializable" : ["postgresql", "mysql", "mariadb"].includes(engine) ? "repeatable_read" : null;
  const liveReview = safeReview && !!review && BigInt(review.expiresAtMs) > BigInt(clock);
  const canBeginSnapshot = !locked && source === "fresh" && !snapshot && snapshotRequested && liveReview && !!snapshotIsolation && session.mode === "read_only" && session.capabilities.snapshot?.supported === true && transaction !== undefined && (!transaction || ["committed", "rolled_back"].includes(transaction.state));
  const ready = !locked && !!destination && confirmed && (format === "json" || formula !== "raw_confirmed" || rawConfirmed) && (source === "shown" ? !staleShown : freshContextReady && safeReview && !!review && BigInt(review.expiresAtMs) > BigInt(clock) && review.warnings.every((warning) => warnings.includes(warning)) && (!snapshotRequested || snapshot !== null && !snapshotExpired));
  function invalidateReview(): void { setReview(null); setWarnings([]); setConfirmed(false); }
  async function pickDestination(): Promise<void> {
    if (locked) return; const captured = binding; setBusy(true); setError(""); releaseDestination(); setDestination(null); setConfirmed(false);
    try { const result = await client.request("movement.destination.pick", { session: ref, format }); if (current.current !== captured) { if (result.destination) void client.request("movement.file.release", { handle: result.destination.destinationHandle }).catch(() => undefined); return; } destinationRef.current = result.destination?.destinationHandle ?? null; setDestination(result.destination); }
    catch (failure) { if (current.current === captured) setError(workbenchErrorText(failure)); }
    finally { if (current.current === captured) setBusy(false); }
  }
  async function reviewRead(): Promise<void> {
    if (locked || !freshContextReady || snapshotRef.current || !sql.trim() || sqlBytes(sql) > MAX_SQL_BYTES) return;
    const captured = binding, sourceSql = sql; setBusy(true); setError(""); invalidateReview();
    try { const next = await client.request("query.review", { session: ref, sql: sourceSql, purpose: "run" }); if (current.current === captured) setReview(next); }
    catch (failure) { if (current.current === captured) setError(workbenchErrorText(failure)); }
    finally { if (current.current === captured) setBusy(false); }
  }
  async function beginSnapshot(): Promise<void> {
    if (!canBeginSnapshot || !snapshotIsolation) return;
    const captured = binding; setBusy(true); setError("");
    try { const next = await client.request("snapshot.begin", { session: ref, isolation: snapshotIsolation, maxLifetimeMs: 60000 }); if (current.current === captured) { snapshotRef.current = next; setSnapshot(next); setClock(Date.now()); } else void client.request("snapshot.end", { session: ref, snapshotId: next.snapshotId }).catch(() => undefined); }
    catch (failure) { if (current.current === captured) setError(workbenchErrorText(failure)); }
    finally { if (current.current === captured) setBusy(false); }
  }
  async function endSnapshotForReview(): Promise<void> {
    if (locked || !snapshot || claimed.current) return;
    const captured = binding; setBusy(true); setError("");
    try { const ended = await client.request("snapshot.end", { session: ref, snapshotId: snapshot.snapshotId }); if (current.current !== captured) return; if (!ended.ended) throw new WorkbenchError("WB_OUTCOME_UNKNOWN"); snapshotRef.current = null; setSnapshot(null); invalidateReview(); }
    catch (failure) { if (current.current === captured) setError(workbenchErrorText(failure)); }
    finally { if (current.current === captured) setBusy(false); }
  }
  async function close(): Promise<void> {
    if (busy) return;
    if (snapshot && !claimed.current) {
      setBusy(true); setError("");
      try { const ended = await client.request("snapshot.end", { session: ref, snapshotId: snapshot.snapshotId }); if (!ended.ended) throw new WorkbenchError("WB_OUTCOME_UNKNOWN"); snapshotRef.current = null; setSnapshot(null); }
      catch (failure) { setError(workbenchErrorText(failure)); return; }
      finally { setBusy(false); }
    }
    onClose();
  }
  async function start(): Promise<void> {
    if (!ready || !destination) return;
    const captured = binding; setBusy(true); setDispatched(true); claimed.current = true; setError("");
    const nativeSource: ExportStartInput["source"] = source === "shown" && fixedShown ? { kind: "shown", ...fixedShown } : { kind: "fresh", reviewId: review!.reviewId, snapshotId: snapshotRequested ? snapshot!.snapshotId : null };
    try { const admission = await client.request("export.start", { session: ref, source: nativeSource, destinationHandle: destination.destinationHandle, format, formulaPolicy: format === "json" ? "escape" : formula, partialPolicy }); if (current.current === captured) { onAdmitted(admission.operationId); onClose(); } }
    catch (failure) { if (current.current === captured) { setError(workbenchErrorText(failure)); if (failure instanceof WorkbenchError && failure.operationId) onAdmitted(failure.operationId); else if (failure instanceof WorkbenchError && failure.code === "WB_CAPABILITY_UNSUPPORTED") claimed.current = false; } }
    finally { if (current.current === captured) setBusy(false); }
  }
  if (suspended) return null;
  return <Dialog title="파일 내보내기 검토" description="보유 결과를 내보내거나 별도로 검토한 새 읽기를 요청합니다. 파일은 완전한 범위 확인·쓰기·동기화·최종 이름 확정 뒤에만 완료로 표시합니다." onClose={() => void close()}><div className="wb-movement-dialog">
    <p className="mono">{session.actualContext.endpoint} / {session.actualContext.database} · {session.actualContext.role || "파일 권한"} · {session.actualContext.purpose === "restored" ? "복원 대상" : "원본"}</p>
    <p>모드 {MODE_LABEL[session.mode]} · {transactionLabel(transaction)}</p>
    <fieldset disabled={locked || snapshot !== null}><legend>내보낼 범위</legend><label className="wb-check"><input type="radio" name="export-source" checked={source === "shown"} disabled={staleShown} onChange={() => { setSource("shown"); setConfirmed(false); }} />선택 시점의 보유 행만 · SQL 재실행 없음</label><label className="wb-check"><input type="radio" name="export-source" checked={source === "fresh"} onChange={() => { setSource("fresh"); setConfirmed(false); }} />새로 검토한 읽기 · 새 시점의 데이터</label></fieldset>
    {source === "shown" && fixedShown ? <p>결과 {fixedShown.identity.resultSetId} · 보유 행 [{fixedShown.range.startRow}, {(BigInt(fixedShown.range.startRow) + BigInt(fixedShown.range.rowCount)).toString()}) · {fixedShown.range.rowCount}행을 고정합니다. 만료되면 SQL을 다시 실행하지 않고 거부합니다.</p> : <>{!freshContextReady && <p className="wb-notice">새 읽기 내보내기는 읽기 전용 모드와 열린 수동 트랜잭션이 없는 연결에서 검토하세요.</p>}<p>파일 전용 새 읽기 상한 1,000,000행 / 인코딩 1GiB / 300초 · 상한·시간 제한에 도달하면 부분 파일로 남깁니다. 스냅샷은 남은 최대 60초가 우선하며 만료 뒤 새 시점으로 전환하지 않습니다.</p><label className="wb-query-label">새로 읽을 SQL 원문<textarea value={sql} disabled={locked || snapshot !== null} spellCheck={false} onChange={(event) => { if (locked || snapshotRef.current) return; if (sqlBytes(event.currentTarget.value) > MAX_SQL_BYTES) { setError("SQL은 UTF-8 1MiB 이하로 입력하세요. 기존 원문은 유지합니다."); return; } setSql(event.currentTarget.value); invalidateReview(); }} /></label><button className="button button-secondary" type="button" disabled={locked || !freshContextReady || snapshot !== null || !sql.trim()} onClick={() => void reviewRead()}>새 읽기 원문 검토</button>{review && !safeReview && <p className="wb-error" role="alert">효과나 안전한 재조회 범위를 증명하지 못한 SQL은 내보내기에 사용하지 않습니다.</p>}{review?.warnings.map((warning) => <label className="wb-check" key={warning}><input type="checkbox" checked={warnings.includes(warning)} disabled={locked} onChange={(event) => setWarnings(event.currentTarget.checked ? [...warnings, warning] : warnings.filter((item) => item !== warning))} />{warningText(warning)}</label>)}<p>{snapshot ? snapshotExpired ? "스냅샷 기한이 지났습니다. 새 페이지로 전환하지 않습니다." : "네이티브가 시작한 스냅샷 · 최대 60초 · 파일 작업에 같은 시점을 유지합니다." : "기본은 읽는 페이지별 시점이며 전체 데이터를 한 시점으로 보장하지 않습니다."}</p><label className="wb-check"><input type="checkbox" checked={snapshotRequested} disabled={locked || snapshot !== null || session.mode !== "read_only" || !session.capabilities.snapshot?.supported || !!transaction && !["committed", "rolled_back"].includes(transaction.state)} onChange={(event) => { setSnapshotRequested(event.currentTarget.checked); setConfirmed(false); }} />검증된 읽기 전용 스냅샷 요청 · 최대 60초</label>{!session.capabilities.snapshot?.supported && <small>{readonlyReasonText(session.capabilities.snapshot?.reason)}</small>}<small>안전한 읽기 SQL을 먼저 검토한 뒤 스냅샷을 시작하세요. 스냅샷 동안 원문과 검토를 고정합니다.</small>{snapshot && <button className="button button-secondary" type="button" disabled={locked} onClick={() => void endSnapshotForReview()}>스냅샷 종료 후 SQL 다시 검토</button>}{snapshotRequested && !snapshot && <button className="button button-secondary" type="button" disabled={!canBeginSnapshot} onClick={() => void beginSnapshot()}>일관된 읽기 시작</button>}</>}
    <div className="wb-query-actions"><label>파일 형식<select value={format} disabled={locked} onChange={(event) => { releaseDestination(); setDestination(null); setFormat(event.currentTarget.value as MovementFormat); setConfirmed(false); }}><option value="csv">CSV</option><option value="tsv">TSV</option><option value="json">형식 포함 JSON</option></select></label><button className="button button-secondary" type="button" disabled={locked} onClick={() => void pickDestination()}>저장 위치 선택</button></div>
    {destination && <p>저장할 파일 {destination.displayName}</p>}
    {format === "json" ? <p>JSON은 정밀 정수·소수 문자열, 바이트·시간·NULL 형식 태그를 보존합니다.</p> : <><p>UTF-8 · SQL NULL은 \N, 빈 문자열과 구분 · 기본 수식 보호는 텍스트의 표시를 변경할 수 있어 원문과 완전히 동일한 표현이 아닙니다.</p><label>스프레드시트 수식 처리<select value={formula} disabled={locked} onChange={(event) => { setFormula(event.currentTarget.value as "escape" | "raw_confirmed"); setRawConfirmed(false); setConfirmed(false); }}><option value="escape">수식 접두사 보호 · 기본</option><option value="raw_confirmed">원문 출력 · 명시적 확인 필요</option></select></label>{formula === "raw_confirmed" && <label className="wb-check"><input type="checkbox" checked={rawConfirmed} disabled={locked} onChange={(event) => setRawConfirmed(event.currentTarget.checked)} />텍스트가 스프레드시트에서 수식으로 실행될 수 있음을 확인했습니다.</label>}</>}
    <label>취소한 부분 파일 처리<select value={partialPolicy} disabled={locked} onChange={(event) => setPartialPolicy(event.currentTarget.value as "retain" | "remove_on_cancel")}><option value="retain">부분 파일 보존 · 위치 표시</option><option value="remove_on_cancel">취소 후 이 작업의 부분 파일만 제거 요청</option></select></label>
    <label className="wb-check"><input type="checkbox" checked={confirmed} disabled={locked || !destination} onChange={(event) => setConfirmed(event.currentTarget.checked)} />실제 대상·범위·일관성·파일 형식과 부분 파일 처리 방침을 확인했습니다.</label><button className="button button-primary" type="button" disabled={!ready} onClick={() => void start()}>검토한 내보내기 요청</button>
    {dispatched && <p role="status">요청을 다시 보내지 않습니다. 파일 완료 여부는 작업 상태에서 확인하세요.</p>}{error && <p className="wb-error" role="alert">{error}</p>}
  </div></Dialog>;
}
