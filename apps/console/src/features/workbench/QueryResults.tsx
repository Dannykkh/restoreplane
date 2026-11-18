import { useEffect, useRef, useState } from "react";
import { nativeU64, type Column, type Completion, type Consistency, type Count, type QueryRunReceipt, type ResultEnvelope, type ResultRow, type Session, type ShownResult, type StreamIdentity, type WorkbenchPage } from "../../lib/workbenchTypes";
import { WorkbenchError, WorkbenchStreamReceiver, boundedRowTail, streamKey, workbenchErrorText, workbenchPages, type WorkbenchClient } from "../../lib/workbench";
import { RowInspector } from "./RowInspector";
import { VirtualResultGrid, type SelectionAnchor } from "./VirtualResultGrid";

type ResultDescriptor = { identity: StreamIdentity; state: "streaming" | "terminal"; completeness: Completion | null };
interface ResultWindow { columns: Column[]; items: ResultRow[]; startRow: ReturnType<typeof nativeU64>; consistency: Consistency }
export interface QueryContext { consistency: Consistency | null; count: Count; completeness: Completion | null; receiving: boolean; shown?: ShownResult | null }
const COMPLETION: Record<Completion, string> = { complete: "조회 완료", capped: "조회 상한 도달", cancelled: "취소 확인됨", interrupted: "수신 중단", partial: "일부 결과", failed: "조회 실패" };
export function QueryResults({ client, session, receipt, onContext, visible = true }: { client: WorkbenchClient; session: Session; receipt: QueryRunReceipt; onContext: (context: QueryContext) => void; visible?: boolean }) {
  const [page, setPage] = useState<WorkbenchPage<ResultDescriptor> | null>(null), [cursor, setCursor] = useState<string | null>(null), [active, setActive] = useState<StreamIdentity | null>(null);
  const [view, setView] = useState<ResultWindow | null>(null), [selection, setSelection] = useState<SelectionAnchor | null>(null);
  const [count, setCount] = useState<Count>({ kind: "unknown" });
  const [completion, setCompletion] = useState<Completion | null>(null), [error, setError] = useState(""), [expired, setExpired] = useState(false), [following, setFollowing] = useState(true), [loading, setLoading] = useState(false);
  const key = JSON.stringify([session.sessionId, session.connectionGeneration, receipt.queryRunId, receipt.executionEpoch]);
  const current = useRef(key), callback = useRef(onContext), receiver = useRef<WorkbenchStreamReceiver | null>(null), buffer = useRef<ResultWindow | null>(null), follow = useRef(true), activeKey = useRef("");
  callback.current = onContext; current.current = key; follow.current = following;
  const ref = { sessionId: session.sessionId, connectionGeneration: session.connectionGeneration };
  useEffect(() => {
    let alive = true, pending = false;
    setPage(null); setActive(null); setError("");
    async function discover(): Promise<void> {
      if (pending) return; pending = true;
      try {
        const next = await client.request("query.results", { session: ref, queryRunId: receipt.queryRunId, page: { cursor, limit: 20 } });
        if (!alive || current.current !== key) return;
        if (next.items.length > 20 || next.items.some(({ identity }) => identity.sessionId !== session.sessionId || identity.connectionGeneration !== session.connectionGeneration || identity.workspaceId !== session.workspaceId || identity.databaseTargetId !== session.databaseTargetId || identity.queryRunId !== receipt.queryRunId)) throw new WorkbenchError("WB_SESSION_STALE");
        setPage(next); setActive((previous) => previous ?? next.items[0]?.identity ?? null);
      } catch (failure) { if (alive && current.current === key) setError(workbenchErrorText(failure)); }
      finally { pending = false; }
    }
    void discover(); const timer = window.setInterval(() => void discover(), 2000);
    return () => { alive = false; window.clearInterval(timer); };
  }, [client, key, cursor]);
  const identityKey = active ? streamKey(active) : "";
  useEffect(() => { callback.current({ consistency: view?.consistency ?? null, count, completeness: completion, receiving: !!active && completion === null && !expired, shown: active && view && !expired && view.items.length > 0 ? { identity: active, range: { startRow: view.startRow, rowCount: view.items.length } } : null }); }, [identityKey, view, count, completion, expired]);
  useEffect(() => {
    receiver.current?.dispose(); buffer.current = null; activeKey.current = identityKey; setView(null); setSelection(null); setCompletion(null); setExpired(false); setFollowing(true); follow.current = true;
    setCount({ kind: "unknown" });
    if (!active) return;
    let alive = true; let receivedThrough = 0n;
    function apply(frame: ResultEnvelope): void {
      if (!alive || current.current !== key || activeKey.current !== identityKey) return;
      if (frame.event.kind === "terminal") { setCompletion(frame.event.completeness); return; }
      let next: ResultWindow;
      if (frame.event.kind === "schema") {
        if (buffer.current) throw new WorkbenchError("WB_PROTOCOL_UNSUPPORTED");
        if (frame.event.columns.length > 512 || new Set(frame.event.columns.map((column) => column.columnId)).size !== frame.event.columns.length) throw new WorkbenchError("WB_PAYLOAD_INVALID");
        next = { columns: frame.event.columns, items: [], startRow: nativeU64("0"), consistency: frame.event.consistency };
      } else {
        const previous = buffer.current;
        if (!previous || BigInt(frame.event.startRow) !== receivedThrough || frame.event.rows.some((row) => row.cells.length !== previous.columns.length)) throw new WorkbenchError("WB_ACK_INVALID");
        receivedThrough += BigInt(frame.event.rows.length);
        const seen = new Set(previous.items.map((row) => row.rowHandle)), combined = [...previous.items, ...frame.event.rows.filter((row) => !seen.has(row.rowHandle))];
        const items = boundedRowTail(combined), dropped = combined.length - items.length;
        next = { ...previous, startRow: nativeU64((BigInt(previous.startRow) + BigInt(dropped)).toString()), items };
      }
      workbenchPages.put(`query:${identityKey}`, next); buffer.current = next;
      if (follow.current) { setView(next); setCount({ kind: "unknown" }); }
    }
    const stream = new WorkbenchStreamReceiver(client, active, apply, (failure) => { if (alive) { setError(workbenchErrorText(failure)); setExpired(true); setCompletion("interrupted"); callback.current({ consistency: buffer.current?.consistency ?? null, count: { kind: "unknown" }, completeness: "interrupted", receiving: false }); } }, (status) => { if (alive) { setExpired(true); setCompletion(status.completeness); setError("결과가 만료되어 종료 상태만 확인했습니다. 행 조회·ACK·SQL 재실행은 하지 않았습니다."); callback.current({ consistency: null, count: { kind: "unknown" }, completeness: status.completeness, receiving: false }); } });
    receiver.current = stream; void stream.attach();
    return () => {
      alive = false; stream.dispose();
      if (receiver.current === stream && activeKey.current === identityKey) { buffer.current = null; receiver.current = null; activeKey.current = ""; }
      workbenchPages.delete(`query:${identityKey}`); workbenchPages.delete(`query-page:${identityKey}`);
    };
  }, [client, key, identityKey]);
  async function retainedPage(start: bigint): Promise<void> {
    if (!active || loading || expired || start < 0n) return;
    const captured = identityKey; setLoading(true); setError(""); setFollowing(false); follow.current = false;
    try {
      const next = await client.request("result.page", { identity: active, startRow: nativeU64(start.toString()), limit: 200 });
      if (current.current !== key || activeKey.current !== captured) return;
      if (next.resultSetId !== active.resultSetId) throw new WorkbenchError("WB_SESSION_STALE");
      const window = { columns: next.columns, items: next.items, startRow: nativeU64(start.toString()), consistency: next.consistency };
      workbenchPages.put(`query-page:${captured}`, window); setView(window); setCount(next.count);
    } catch (failure) { if (activeKey.current === captured) { setError(workbenchErrorText(failure)); if (failure instanceof WorkbenchError && failure.code === "WB_RESULT_EXPIRED") setExpired(true); } }
    finally { if (activeKey.current === captured) setLoading(false); }
  }
  const row = view?.items.find((item) => item.rowHandle === selection?.rowHandle);
  return <section className="wb-query-results" aria-label="SQL 결과 집합">
    <div className="wb-query-actions">{page?.items.map((item, index) => <button className="button button-secondary" type="button" key={streamKey(item.identity)} aria-pressed={identityKey === streamKey(item.identity)} onClick={() => setActive(item.identity)}>결과 {index + 1} · {item.completeness ? COMPLETION[item.completeness] : "수신 중"}</button>)}<button className="button button-secondary" type="button" disabled={!page?.nextCursor} onClick={() => { setActive(null); setCursor(page?.nextCursor ?? null); }}>다음 결과 목록</button></div>
    {!active && <p role="status">등록된 결과 집합이 없습니다. 문장별 효과 증거는 작업 상태에서 확인하세요.</p>}
    {active && <><p role="status">{expired ? "보유 결과 만료" : completion ? COMPLETION[completion] : "결과 수신 중"} · SQL을 다시 실행하지 않고 보유 결과만 표시합니다.</p>
      {view && <><VirtualResultGrid key={identityKey} resultSetId={active.resultSetId} rows={view.items} columns={view.columns} startRow={view.startRow} selection={selection} onSelect={setSelection} visible={visible} /><div className="wb-query-actions"><span>현재 보유 {view.items.length}행 · 시작 위치 {view.startRow} · 전체 수 미확인</span><button className="button button-secondary" type="button" disabled={loading || expired || BigInt(view.startRow) === 0n} onClick={() => void retainedPage(BigInt(view.startRow) > 200n ? BigInt(view.startRow) - 200n : 0n)}>보유 이전 행</button><button className="button button-secondary" type="button" disabled={loading || expired || view.items.length === 0} onClick={() => void retainedPage(BigInt(view.startRow) + BigInt(view.items.length))}>보유 다음 행</button><button className="button button-secondary" type="button" disabled={expired || following} onClick={() => { setFollowing(true); follow.current = true; setView(buffer.current); setCount({ kind: "unknown" }); }}>새로 수신한 행 따라가기</button></div><details><summary>선택 셀 상세</summary><RowInspector client={client} row={row} columns={view.columns} selection={selection} identity={active} /></details></>}
    </>}
    {error && <p className="wb-error" role="alert">{error}</p>}
  </section>;
}
