import { useEffect, useRef, useState } from "react";
import { Dialog } from "../components/Dialog";
import { activeWorkspaceId, listDatabaseTargets } from "../lib/api";
import { useDebouncedValue, useServiceQuery } from "../lib/queries";
import type { CursorPage, DatabaseTarget, RouteState } from "../lib/types";
import { nativeU64, type CatalogItem, type Completion, type ResultEnvelope, type ResultNotice, type ResultPage, type Session, type ShownResult, type StreamIdentity, type Transaction } from "../lib/workbenchTypes";
import { WorkbenchError, WorkbenchStreamReceiver, boundedRowTail, setWorkbenchDepartureGuard, workbenchClient, workbenchErrorText, workbenchPages, type WorkbenchClient } from "../lib/workbench";
import { SchemaExplorer } from "../features/workbench/SchemaExplorer";
import { SchemaDiagram } from "../features/workbench/SchemaDiagram";
import { VirtualResultGrid, type SelectionAnchor } from "../features/workbench/VirtualResultGrid";
import { RowInspector } from "../features/workbench/RowInspector";
import { RelationMetadata } from "../features/workbench/RelationMetadata";
import { SqlWorkspace } from "../features/workbench/SqlWorkspace";
import type { QueryContext } from "../features/workbench/QueryResults";
import { EditDraft } from "../features/workbench/EditDraft";
import { OperationHistory } from "../features/workbench/OperationHistory";
import { ImportReview } from "../features/workbench/ImportReview";
import { ExportDialog } from "../features/workbench/ExportDialog";
import { MovementProgressPanel } from "../features/workbench/MovementProgressPanel";
import { ProtectionPanel } from "../features/workbench/ProtectionPanel";
import { protectionErrorText, restoredInspection, workbenchProtection, type WorkbenchProtectionGateway } from "../lib/workbenchProtection";
import "../features/workbench/workbench.css";
import { TransactionFence } from "../lib/transactionFence";
import { TransactionBar } from "../features/workbench/TransactionBar";

const LIMITS = { maxRows: 200, maxBytes: 512 * 1024, statementTimeoutMs: 30_000, lockWaitMs: 2_000 };
export type TargetLoader = (request: { cursor: string | null; limit: number; search: string; connectionId?: string }) => Promise<CursorPage<DatabaseTarget>>;
export interface WorkbenchDependencies { client?: WorkbenchClient; loadTargets?: TargetLoader; protection?: WorkbenchProtectionGateway }
export function Workbench({ route, client = workbenchClient, loadTargets = listDatabaseTargets, protection = workbenchProtection }: { route: RouteState } & WorkbenchDependencies) {
  const workspaceId = activeWorkspaceId();
  const restoreRequested = route.action?.startsWith("restore-inspect") === true;
  const restoreWorkItemId = restoreRequested ? route.action!.slice("restore-inspect:".length) : null;
  const restoreProof = useServiceQuery(`workbench-restore-proof:${workspaceId}:${restoreWorkItemId}`, () => restoreRequested && client.available ? restoredInspection(protection, restoreWorkItemId ?? "") : Promise.resolve(null));
  const [targetCursor, setTargetCursor] = useState<string | null>(null), [targetSearch, setTargetSearch] = useState("");
  const search = useDebouncedValue(targetSearch);
  const restoredProfile = restoreProof.data?.engine === "sqlite" ? undefined : restoreProof.data?.connectionProfileId;
  const targets = useServiceQuery(`workbench-targets:${workspaceId}:${restoreWorkItemId}:${restoreProof.data?.engine}:${restoreProof.data?.connectionProfileId}:${targetCursor}:${search}`, () => restoreRequested && !restoreProof.data ? Promise.resolve({ items: [], nextCursor: null, totalCount: 0 }) : loadTargets({ cursor: targetCursor, limit: 50, search, ...(restoredProfile ? { connectionId: restoredProfile } : {}) }));
  const [targetId, setTargetId] = useState(restoreRequested ? "" : route.target ?? route.selected ?? "");
  const target = targets.data?.items.find((item) => item.id === targetId) ?? null;
  const [username, setUsername] = useState(""), [secret, setSecret] = useState("");
  const [session, setSession] = useState<Session | null>(null), [transaction, setTransaction] = useState<Transaction | null | undefined>(undefined);
  const transactionFence = useRef(new TransactionFence()).current;
  function acceptTransaction(value: Transaction | null): void { transactionFence.invalidate(); setTransaction(value); }
  const sessionRef = useRef<Session | null>(null); sessionRef.current = session;
  const [busy, setBusy] = useState(false), [error, setError] = useState("");
  const [result, setResult] = useState<ResultPage | null>(null), [completion, setCompletion] = useState<Completion | null>(null);
  const [expiredResult, setExpiredResult] = useState(false);
  const [identity, setIdentity] = useState<StreamIdentity | null>(null), [selection, setSelection] = useState<SelectionAnchor | null>(null);
  const [relation, setRelation] = useState<CatalogItem | null>(null), [schema, setSchema] = useState<CatalogItem | null>(null);
  const [pane, setPane] = useState<"rows" | "diagram" | "sql">("rows");
  const [targetDetailsOpen, setTargetDetailsOpen] = useState(false);
  const [explorerExpanded, setExplorerExpanded] = useState(true);
  const explorerChosen = useRef(false);
  const layout = useRef<HTMLDivElement>(null);
  const [sqlOpened, setSqlOpened] = useState(false), [dirtySql, setDirtySql] = useState(false), [discardSql, setDiscardSql] = useState(false);
  const [queryContext, setQueryContext] = useState<QueryContext>({ consistency: null, count: { kind: "unknown" }, completeness: null, receiving: false });
  const [sqlRunning, setSqlRunning] = useState(false), [dirtyEdit, setDirtyEdit] = useState(false);
  const [editSource, setEditSource] = useState<{ object: CatalogItem; resultSetId: string; columns: ResultPage["columns"] } | null>(null);
  const unsavedDrafts = dirtySql || dirtyEdit;
  const [historyOpen, setHistoryOpen] = useState(false);
  const [movementDialog, setMovementDialog] = useState<"import" | "export" | null>(null), [continuationParent, setContinuationParent] = useState<string | null>(null);
  const [movementJobs, setMovementJobs] = useState<{ operationId: string; kind: "import" | "export" }[]>([]);
  const [resultStartRow, setResultStartRow] = useState(nativeU64("0"));
  const resultStart = useRef(resultStartRow); resultStart.current = resultStartRow;
  const [closeDialog, setCloseDialog] = useState(false), [pendingDeparture, setPendingDeparture] = useState<(() => void) | null>(null);
  const [uncertainty, setUncertainty] = useState("");
  const [snapshot, setSnapshot] = useState<{ snapshotId: string; expiresAtMs: import("../lib/workbenchTypes").NativeCounterU64 } | null>(null);
  const [clock, setClock] = useState(Date.now);
  const [snapshotUnsupported, setSnapshotUnsupported] = useState(false);
  const snapshotExpired = snapshot !== null && BigInt(snapshot.expiresAtMs) <= BigInt(clock);
  const receiver = useRef<WorkbenchStreamReceiver | null>(null), epoch = useRef(0);
  const resultRef = useRef<ResultPage | null>(null); resultRef.current = result;
  useEffect(() => {
    setTargetDetailsOpen(false);
    const node = layout.current;
    if (!session || !node) return;
    const update = () => {
      // Local presentation sizing only: actual shell/banner and wrapped controls determine the remaining height.
      const height = `${Math.max(360, Math.floor((window.visualViewport?.height ?? window.innerHeight) - node.getBoundingClientRect().top - 16))}px`;
      if (node.style.getPropertyValue("--wb-pane-height") !== height) node.style.setProperty("--wb-pane-height", height);
      if (!explorerChosen.current) setExplorerExpanded(node.clientWidth > 860);
    };
    update(); window.addEventListener("resize", update); window.visualViewport?.addEventListener("resize", update);
    const observer = typeof ResizeObserver === "undefined" ? null : new ResizeObserver(update);
    for (const region of Array.from(node.parentElement?.children ?? [])) if (region !== node) observer?.observe(region);
    for (const selector of [".topbar", ".runtime-banner"]) { const region = document.querySelector(selector); if (region) observer?.observe(region); }
    return () => { observer?.disconnect(); window.removeEventListener("resize", update); window.visualViewport?.removeEventListener("resize", update); };
  }, [session?.sessionId, session?.connectionGeneration]);
  useEffect(() => {
    if (!restoreRequested && !targetId && targets.data?.items[0]) setTargetId(targets.data.items[0].id);
  }, [targetId, targets.data, restoreRequested]);
  useEffect(() => { if (restoreRequested) { setTargetId(""); setTargetCursor(null); } }, [restoreRequested, restoreWorkItemId]);
  useEffect(() => {
    setWorkbenchDepartureGuard((proceed) => {
      if (!sessionRef.current) return true;
      setPendingDeparture(() => proceed); setDiscardSql(false); setCloseDialog(true); return false;
    });
    return () => { setWorkbenchDepartureGuard(null); epoch.current++; receiver.current?.dispose(); workbenchPages.clear(); };
  }, []);
  useEffect(() => { if (!snapshot) return; const timer = window.setInterval(() => setClock(Date.now()), 1000); return () => window.clearInterval(timer); }, [snapshot]);
  useEffect(() => {
    const protect = (event: BeforeUnloadEvent) => { if (sessionRef.current) { event.preventDefault(); event.returnValue = ""; } };
    window.addEventListener("beforeunload", protect); return () => window.removeEventListener("beforeunload", protect);
  }, []);
  function clearResult(): void {
    receiver.current?.dispose(); receiver.current = null; resultRef.current = null;
    setResult(null); setIdentity(null); setSelection(null); setCompletion(null); setExpiredResult(false); setResultStartRow(nativeU64("0")); resultStart.current = nativeU64("0"); workbenchPages.clear();
  }
  async function connect(): Promise<void> {
    if (!target || busy || session) return;
    if (restoreRequested && (!restoreProof.data || target.engine !== restoreProof.data.engine || restoreProof.data.engine !== "sqlite" && target.connectionId !== restoreProof.data.connectionProfileId)) { setError("완료된 복원의 등록 대상을 직접 선택해야 합니다. 원본 대상으로 대신 연결하지 않습니다."); return; }
    const requestEpoch = ++epoch.current; setBusy(true); setError("");
    const scope = { workspaceId, connectionProfileId: target.connectionId, databaseTargetId: target.id };
    try {
      const credential = await client.request("credential.put", { scope, credentialRef: null, label: `${target.displayName.slice(0, 80)} 조회`, username: target.engine === "sqlite" ? "" : username, secret: target.engine === "sqlite" ? "" : secret });
      if (requestEpoch !== epoch.current) return;
      setSecret("");
      const opened = await client.request("session.open", { scope, credentialRef: credential.credentialRef, expectedDescriptorRevision: null, mode: "read_only", ...(restoreRequested && restoreWorkItemId ? { restoreWorkItemId } : {}) });
      if (requestEpoch !== epoch.current) { void client.request("session.close", { session: { sessionId: opened.sessionId, connectionGeneration: opened.connectionGeneration }, disposition: "rollback_request" }).catch(() => undefined); return; }
      if (opened.workspaceId !== scope.workspaceId || opened.connectionProfileId !== scope.connectionProfileId || opened.databaseTargetId !== scope.databaseTargetId) throw new WorkbenchError("WB_SESSION_STALE");
      if (restoreRequested && (opened.actualContext.purpose !== "restored" || opened.mode !== "read_only")) { void client.request("session.close", { session: { sessionId: opened.sessionId, connectionGeneration: opened.connectionGeneration }, disposition: "rollback_request" }).catch(() => undefined); throw new WorkbenchError("WB_BINDING_UNPROVEN"); }
      sessionRef.current = opened; setSession(opened); setTransaction(undefined); setSnapshot(null); setSnapshotUnsupported(false); clearResult(); setRelation(null); setSchema(null);
      transactionFence.bind(JSON.stringify({ sessionId: opened.sessionId, connectionGeneration: opened.connectionGeneration }));
      const transactionRead = transactionFence.read();
      void client.request("transaction.get", { session: { sessionId: opened.sessionId, connectionGeneration: opened.connectionGeneration } }).then((value) => { if (epoch.current === requestEpoch && transactionRead && transactionFence.acceptsRead(transactionRead)) acceptTransaction(value); }).catch(() => undefined);
    } catch (failure) { if (epoch.current === requestEpoch) { setSecret(""); setError(workbenchErrorText(failure)); } }
    finally { if (epoch.current === requestEpoch) setBusy(false); }
  }
  function applyFrame(frame: ResultEnvelope): void {
    const current = resultRef.current;
    if (!current || frame.identity.resultSetId !== current.resultSetId) throw new WorkbenchError("WB_SESSION_STALE");
    if (frame.event.kind === "terminal") { setCompletion(frame.event.completeness); return; }
    if (frame.event.kind === "schema") {
      const signature = (columns: ResultPage["columns"]) => JSON.stringify(columns.map((column) => [column.columnId, column.name, column.typeName, column.nullable, column.editable, column.readonlyReason]));
      if (signature(current.columns) !== signature(frame.event.columns) || current.consistency.kind !== frame.event.consistency.kind || (current.consistency.kind === "transaction_snapshot" && frame.event.consistency.kind === "transaction_snapshot" && current.consistency.snapshotId !== frame.event.consistency.snapshotId)) throw new WorkbenchError("WB_SESSION_STALE");
      return;
    }
    const seen = new Set(current.items.map((row) => row.rowHandle));
    const items = [...current.items, ...frame.event.rows.filter((row) => !seen.has(row.rowHandle))];
    // minimal: retain at most 200 rows per surface — result.page supplies another explicit retained window.
    const retained = boundedRowTail(items), next = { ...current, items: retained };
    const start = nativeU64((BigInt(resultStart.current) + BigInt(items.length - retained.length)).toString()); resultStart.current = start; setResultStartRow(start);
    workbenchPages.put(`result:${current.resultSetId}`, next); resultRef.current = next; setResult(next);
  }
  async function browse(item: CatalogItem): Promise<void> {
    const current = sessionRef.current;
    if (!current || busy) return;
    if (sqlRunning) { setError("실행 중인 SQL의 상태를 확인하거나 중지를 요청한 뒤 새 조회를 시작하세요."); return; }
    const requestEpoch = ++epoch.current; setBusy(true); setError(""); clearResult(); setRelation(item); setPane("rows");
    let streamReceiver: WorkbenchStreamReceiver | null = null;
    let early: ResultNotice | null = null;
    try {
      const ref = { sessionId: current.sessionId, connectionGeneration: current.connectionGeneration };
      if (snapshotExpired) throw new WorkbenchError("WB_RESULT_EXPIRED");
      const reply = await client.request("browse.open", { session: ref, object: item.object, filter: { kind: "and", items: [] }, order: [], snapshotId: snapshot?.snapshotId ?? null, limits: LIMITS }, (notice) => {
        if (requestEpoch !== epoch.current) return;
        if (streamReceiver) streamReceiver.notice(notice);
        else early = notice;
      });
      if (epoch.current !== requestEpoch) return;
      if (reply.result.schemaVersion !== item.object.schemaVersion) throw new WorkbenchError("WB_CURSOR_INVALID");
      if (reply.stream && (reply.stream.sessionId !== current.sessionId || reply.stream.connectionGeneration !== current.connectionGeneration || reply.stream.workspaceId !== current.workspaceId || reply.stream.databaseTargetId !== current.databaseTargetId || reply.stream.resultSetId !== reply.result.resultSetId)) throw new WorkbenchError("WB_SESSION_STALE");
      workbenchPages.put(`result:${reply.result.resultSetId}`, reply.result); resultRef.current = reply.result;
      setResult(reply.result); setIdentity(reply.stream); setCompletion(null);
      setEditSource({ object: item, resultSetId: reply.result.resultSetId, columns: reply.result.columns });
      if (reply.stream) {
        const stream = new WorkbenchStreamReceiver(client, reply.stream, applyFrame, (failure) => { setError(workbenchErrorText(failure)); setCompletion("interrupted"); setExpiredResult(true); }, (status) => { setCompletion(status.completeness); setExpiredResult(true); setError("결과 보유 기한이 지나 종료 상태만 확인했습니다. 행을 다시 가져오거나 SQL을 다시 실행하지 않았습니다."); });
        streamReceiver = stream; receiver.current = stream; if (early) stream.notice(early); early = null; await stream.attach();
      }
    } catch (failure) { if (epoch.current === requestEpoch) setError(workbenchErrorText(failure)); }
    finally { if (epoch.current === requestEpoch) setBusy(false); }
  }
  async function nextPage(): Promise<void> {
    const current = sessionRef.current, previous = resultRef.current;
    if (!current || !previous?.nextCursor || busy || expiredResult) return;
    const requestEpoch = epoch.current; setBusy(true); setError("");
    const requestedStart = nativeU64((BigInt(resultStart.current) + BigInt(previous.items.length)).toString());
    try {
      const next = await client.request("browse.page", { session: { sessionId: current.sessionId, connectionGeneration: current.connectionGeneration }, cursor: previous.nextCursor, limit: 200 });
      if (epoch.current !== requestEpoch) return;
      if (next.resultSetId !== previous.resultSetId || next.schemaVersion !== previous.schemaVersion) throw new WorkbenchError("WB_CURSOR_INVALID");
      workbenchPages.put(`result:${next.resultSetId}`, next); resultRef.current = next; setResult(next); resultStart.current = requestedStart; setResultStartRow(requestedStart);
    } catch (failure) { if (epoch.current === requestEpoch) setError(workbenchErrorText(failure)); }
    finally { if (epoch.current === requestEpoch) setBusy(false); }
  }
  async function toggleSnapshot(): Promise<void> {
    const current = sessionRef.current; if (!current || busy) return;
    const requestEpoch = epoch.current; setBusy(true); setError("");
    const ref = { sessionId: current.sessionId, connectionGeneration: current.connectionGeneration };
    try {
      if (snapshot) {
        const ended = await client.request("snapshot.end", { session: ref, snapshotId: snapshot.snapshotId });
        if (!ended.ended) throw new WorkbenchError("WB_OUTCOME_UNKNOWN");
        if (requestEpoch === epoch.current) setSnapshot(null);
      } else {
        const opened = await client.request("snapshot.begin", { session: ref, isolation: "repeatable_read", maxLifetimeMs: 60_000 });
        if (requestEpoch === epoch.current) { setSnapshot(opened); setClock(Date.now()); }
      }
    } catch (failure) { if (requestEpoch === epoch.current) { if (failure instanceof WorkbenchError && failure.code === "WB_CAPABILITY_UNSUPPORTED") setSnapshotUnsupported(true); setError(workbenchErrorText(failure)); } }
    finally { if (requestEpoch === epoch.current) setBusy(false); }
  }
  async function reconnect(): Promise<void> {
    const current = sessionRef.current; if (!current || busy) return;
    const requestEpoch = ++epoch.current; setBusy(true); setError(""); clearResult();
    try {
      const next = await client.request("session.reconnect", { session: { sessionId: current.sessionId, connectionGeneration: current.connectionGeneration }, expectedDescriptorRevision: current.descriptorRevision });
      if (epoch.current === requestEpoch) {
        if (next.workspaceId !== current.workspaceId || next.databaseTargetId !== current.databaseTargetId || BigInt(next.connectionGeneration) <= BigInt(current.connectionGeneration)) throw new WorkbenchError("WB_SESSION_STALE");
        sessionRef.current = next; setSession(next); setRelation(null); setSchema(null); setTransaction(undefined); setSnapshot(null); setSnapshotUnsupported(false);
      }
    } catch (failure) { if (epoch.current === requestEpoch) { setError(workbenchErrorText(failure)); setSession({ ...current, state: "retiring" }); } }
    finally { if (epoch.current === requestEpoch) setBusy(false); }
  }
  async function close(): Promise<void> {
    const current = sessionRef.current; if (!current || busy || (unsavedDrafts && !discardSql)) return;
    setBusy(true); setError("");
    try {
      const reply = await client.request("session.close", { session: { sessionId: current.sessionId, connectionGeneration: current.connectionGeneration }, disposition: "rollback_request" });
      if (reply.transaction) setTransaction(reply.transaction);
      if (reply.transaction?.state === "outcome_unknown" || transaction?.state === "outcome_unknown") setUncertainty("닫은 연결에 결과를 확인하지 못한 트랜잭션이 남아 있습니다.");
      if (reply.state !== "closed") { setError("연결 종료를 아직 확인하지 못했습니다."); return; }
      epoch.current++; clearResult(); sessionRef.current = null; setSession(null); setRelation(null); setSchema(null); setSnapshot(null); setCloseDialog(false); setSqlOpened(false); setDirtySql(false); setDiscardSql(false); setEditSource(null); setDirtyEdit(false); setSqlRunning(false); setMovementDialog(null); setMovementJobs([]);
      const proceed = pendingDeparture; setPendingDeparture(null); proceed?.();
    } catch (failure) { setError(workbenchErrorText(failure)); }
    finally { setBusy(false); }
  }
  const sessionReference = session ? { sessionId: session.sessionId, connectionGeneration: session.connectionGeneration } : null;
  function applySessionControl(next: Session): void {
    const current = sessionRef.current;
    if (!current || next.workspaceId !== current.workspaceId || next.connectionProfileId !== current.connectionProfileId || next.databaseTargetId !== current.databaseTargetId) { setError(workbenchErrorText(new WorkbenchError("WB_SESSION_STALE"))); return; }
    epoch.current++; clearResult(); sessionRef.current = next; setSession(next); setTransaction(undefined); setSnapshot(null);
  }
  function admitMovement(operationId: string, kind: "import" | "export"): void { setMovementJobs((jobs) => [...jobs.filter((job) => job.operationId !== operationId), { operationId, kind }].slice(-4)); }
  function continueImport(parentId: string): void { if (!session || !editSource) { setError("읽기 연결을 열고 원래 가져오기 대상 테이블을 선택한 뒤 미전송 구간을 새로 검토하세요."); return; } setHistoryOpen(false); setContinuationParent(parentId); setMovementDialog("import"); }
  const row = result?.items.find((item) => item.rowHandle === selection?.rowHandle);
  const shownResult: ShownResult | null = pane === "sql" ? queryContext.shown ?? null : identity && result && !expiredResult && result.items.length > 0 ? { identity, range: { startRow: resultStartRow, rowCount: result.items.length } } : null;
  const mode = session?.mode === "read_only" ? "읽기 전용" : session?.mode === "read_write" ? "읽기·쓰기" : session?.mode === "full" ? "전체 SQL 모드" : "미연결";
  const visibleCount = pane === "sql" ? queryContext.count : result?.count ?? { kind: "unknown" as const };
  const count = visibleCount.kind === "unknown" ? "미확인" : `${visibleCount.kind === "estimate" ? "추정 " : "정확 "}${visibleCount.value}`;
  const transactionText = snapshot ? snapshotExpired ? "스냅샷 종료 확인 필요" : "조회용 스냅샷" : transaction === undefined ? "미확인" : transaction === null ? "열린 트랜잭션 없음" : ({ opening: "열기 확인 중", active: "진행 중", failed_needs_rollback: "롤백 필요", commit_requested: "COMMIT 확인 중", committed: "COMMIT 확인됨", rollback_requested: "롤백 확인 중", rolled_back: "롤백 확인됨", outcome_unknown: "결과 미확인" }[transaction.state]);
  const completionText = pane === "sql" ? queryContext.completeness ? ({ complete: "조회 완료", capped: "조회 상한 도달", cancelled: "취소 확인됨", interrupted: "수신 중단", partial: "일부 결과", failed: "조회 실패" }[queryContext.completeness]) : queryContext.receiving ? "결과 수신 중" : "실행 전" : expiredResult ? "결과 만료 · 상태만 확인" : completion ? ({ complete: "조회 완료", capped: "조회 상한 도달", cancelled: "취소 확인됨", interrupted: "수신 중단", partial: "일부 결과", failed: "조회 실패" }[completion]) : result ? result.nextCursor ? "계속 조회 가능" : result.completeness === "limited" ? "제한된 미리보기" : identity ? "결과 수신 중" : "현재 페이지" : "미조회";
  return <div className="wb-workspace" data-connected={!!session}>
    <header className="wb-title"><h1>데이터베이스 작업</h1><div><button className="button button-secondary" type="button" disabled={!client.available || (!session && !target)} onClick={() => setHistoryOpen(true)}>작업 기록</button>{session && <><button className="button button-secondary" type="button" disabled={busy} onClick={() => void reconnect()}>다시 연결</button><button className="button button-secondary" type="button" disabled={busy} onClick={() => { setDiscardSql(false); setCloseDialog(true); }}>연결 닫기</button></>}</div></header>
    <section className="wb-context" aria-label="실제 연결 대상과 조회 상태">
      <dl><div className="wb-target-context"><dt>실제 대상</dt><dd><span className="mono wb-target-value">{session ? `${session.actualContext.endpoint} / ${session.actualContext.database}` : "미연결 · 실제 대상 확인 전"}</span>{session && <button type="button" className="wb-text-button" onClick={() => setTargetDetailsOpen(true)}>전체 대상 보기</button>}</dd></div><div><dt>사용자 · 용도</dt><dd>{session ? `${session.actualContext.role || "파일 권한"} · ${session.actualContext.purpose === "restored" ? "복원 대상" : "원본"}` : "미확인"}</dd></div><div><dt>모드</dt><dd>{mode}</dd></div><div><dt>트랜잭션</dt><dd>{transactionText}</dd></div><div><dt>일관성</dt><dd>{pane === "sql" ? queryContext.consistency === null ? "미조회" : queryContext.consistency.kind === "live_per_page" ? "페이지별 현재 상태" : "트랜잭션 스냅샷" : !result ? "미조회" : result.consistency.kind === "live_per_page" ? "페이지별 현재 상태" : "트랜잭션 스냅샷"}</dd></div><div><dt>전체 행 수</dt><dd className="mono">{count}</dd></div><div><dt>조회 범위</dt><dd>{completionText}</dd></div></dl>
    </section>
    {!client.available && <p className="wb-notice" role="status">{workbenchErrorText(new WorkbenchError("WB_NATIVE_UNAVAILABLE"))}</p>}
    {client !== workbenchClient && <p className="wb-notice" role="status">테스트 모의 전송 · 실제 네이티브 연결 증거가 아닙니다.</p>}
    {error && <p className="wb-error" role="alert">{error}</p>}{uncertainty && <p className="wb-error" role="alert">{uncertainty}</p>}
    {restoreRequested && <section className="wb-notice" aria-label="복원 대상 읽기 검사"><h2>완료된 복원 대상 읽기 검사</h2>{restoreProof.data ? <><p>복원 기록의 데이터베이스 {restoreProof.data.databaseName} · 새 읽기 전용 연결 · 등록 대상을 직접 선택하세요.</p><p>표시 이름은 대상 정체성의 증거가 아닙니다. 연결 시 실제 서버·데이터베이스·파일 정체성을 다시 확인하며 원본으로 대신 연결하지 않습니다.</p><details><summary>설명용 복원 근거</summary><code>{restoreProof.data.actualDatabaseIdentity}</code><p>완료 시각 {restoreProof.data.completedAtMs}</p></details></> : restoreProof.status === "error" ? <p role="alert">{protectionErrorText(restoreProof.error)}</p> : <p role="status">완료된 복원의 정체성 근거를 확인하는 중입니다.</p>}<p>아직 등록되지 않은 복원 대상은 기존 <a href="#/connections">대상 발견·등록</a>을 먼저 진행하세요.</p></section>}
    {session && <TransactionBar client={client} session={session} engine={target?.engine ?? "unknown"} transaction={transaction} onSession={applySessionControl} onTransaction={acceptTransaction} suspended={closeDialog || historyOpen || movementDialog !== null || targetDetailsOpen} transactionFence={transactionFence} executionBlocked={busy || snapshot !== null || sqlRunning} compact />}
    {!session ? <section className="wb-connect" aria-label="조회할 데이터베이스 연결">
      <div><h2>조회할 대상</h2><label>대상 이름 검색<input value={targetSearch} maxLength={256} onChange={(event) => { setTargetSearch(event.target.value); setTargetCursor(null); setTargetId(""); }} /></label>
        <label>데이터베이스<select value={targetId} disabled={busy || targets.status !== "ready"} onChange={(event) => setTargetId(event.target.value)}><option value="">대상을 선택하세요</option>{targets.data?.items.map((item) => <option key={item.id} value={item.id}>{item.connectionName} / {item.displayName} · {item.engine}</option>)}</select></label>
        {targets.status === "loading" && <p role="status">대상 목록을 읽는 중입니다.</p>}{targets.status === "error" && <p role="alert">대상 목록을 확인하지 못했습니다. <button type="button" className="wb-text-button" onClick={targets.refresh}>다시 읽기</button></p>}
        <div className="wb-list-foot"><span>현재 페이지 {targets.data?.items.length ?? 0}개</span><button className="button button-secondary" type="button" disabled={!targets.data?.nextCursor || busy} onClick={() => { setTargetCursor(targets.data?.nextCursor ?? null); setTargetId(""); }}>다음 대상</button></div>
      </div>
      <form onSubmit={(event) => { event.preventDefault(); void connect(); }}><h2>읽기 전용 연결</h2>
        {target?.engine === "sqlite" ? <p>SQLite는 데이터베이스 암호를 요구하지 않습니다. 현재 Windows 사용자의 파일 접근 권한으로 실제 파일을 확인합니다.</p> : <><label>데이터베이스 사용자<input value={username} maxLength={128} autoComplete="username" onChange={(event) => setUsername(event.target.value)} disabled={busy} /></label><label>대화형 연결 암호<input type="password" value={secret} maxLength={65536} autoComplete="off" onChange={(event) => setSecret(event.target.value)} disabled={busy} /></label><p className="wb-muted">백업 서비스의 자격증명과 별도로 현재 사용자에게 저장합니다.</p></>}
        <button className="button button-primary" type="submit" disabled={busy || !client.available || !target || target.state === "retired" || target.state === "disabled"}>{busy ? "대상 확인 중…" : "실제 대상 확인하고 연결"}</button>
      </form>
    </section> : <div className="wb-layout" ref={layout} data-explorer-open={explorerExpanded}>
      <details className="wb-explorer-fold" open={explorerExpanded}><summary onClick={(event) => { event.preventDefault(); explorerChosen.current = true; setExplorerExpanded((open) => !open); }}>스키마 탐색기</summary>{sessionReference && <SchemaExplorer key={`${session.sessionId}:${session.connectionGeneration}`} client={client} session={sessionReference} onRelation={(item) => void browse(item)} onSchema={setSchema} />}</details>
      <section className="wb-results" aria-label="테이블 조회 작업 영역">
        <div className="wb-result-toolbar"><h2>{pane === "sql" ? "SQL 작성·검토" : relation?.name ?? "테이블을 선택하세요"}</h2><div className="wb-pane-navigation"><button className="button button-secondary" type="button" onClick={() => setPane("rows")} aria-pressed={pane === "rows"}>행 조회</button><button className="button button-secondary" type="button" disabled={!schema} onClick={() => setPane("diagram")} aria-pressed={pane === "diagram"}>관계도</button><button className="button button-secondary" type="button" disabled={snapshot !== null} onClick={() => { setSqlOpened(true); setPane("sql"); }} aria-pressed={pane === "sql"}>SQL 작성·검토</button>{sqlRunning && pane !== "sql" && <button className="button button-secondary" type="button" onClick={() => setPane("sql")}>실행 중인 SQL 상태·중지</button>}</div>
          <details className="wb-secondary-actions"><summary>테이블·파일 작업</summary><div><button className="button button-secondary" type="button" disabled={busy || snapshotUnsupported || !!(transaction && !["committed", "rolled_back"].includes(transaction.state))} onClick={() => void toggleSnapshot()}>{snapshot ? "스냅샷 종료" : "일관된 조회 시작"}</button><button className="button button-secondary" type="button" disabled={!relation || busy || snapshotExpired} onClick={() => relation && void browse(relation)}>새로 조회</button><button className="button button-secondary" type="button" disabled={busy || !editSource || sqlRunning} onClick={() => { setContinuationParent(null); setMovementDialog("import"); }}>파일 가져오기</button><button className="button button-secondary" type="button" disabled={busy} onClick={() => setMovementDialog("export")}>파일 내보내기</button></div></details>
        </div>
        {sqlOpened && <div className="wb-sql-slot" data-workbench-slot="sql" data-active={pane === "sql"}><SqlWorkspace client={client} session={session} engine={target?.engine ?? "unknown"} transaction={transaction} onSession={applySessionControl} onTransaction={acceptTransaction} transactionFence={transactionFence} onDirty={setDirtySql} onContext={setQueryContext} onRunning={setSqlRunning} suspended={closeDialog || historyOpen || movementDialog !== null || targetDetailsOpen || snapshot !== null} schema={schema} relation={relation} visible={pane === "sql"} renderTransactionControls={false} /></div>}
        {snapshot && <p className="wb-muted" role="status">{snapshotExpired ? "스냅샷 만료 · 새 조회를 보내지 않습니다. 종료한 뒤 다시 시작해 주세요." : "읽기 전용 스냅샷 사용 중 · 최대 60초 · 시작 전 결과는 자동 재조회하지 않습니다."}</p>}
        {busy && <p role="status">데이터를 읽는 중입니다.</p>}
        <div className="wb-diagram-slot" hidden={pane !== "diagram"}>{pane === "diagram" && schema && sessionReference && <SchemaDiagram key={schema.object.catalogId} client={client} session={sessionReference} schema={schema} />}</div>
        <div className="wb-rows-slot" hidden={pane !== "rows"}>{result ? <>
          <VirtualResultGrid startRow={resultStartRow} resultSetId={result.resultSetId} rows={result.items} columns={result.columns} selection={selection} onSelect={setSelection} visible={pane === "rows"} />
          <div className="wb-result-foot"><span>현재 보유 {result.items.length}행 · 캐시 추정 {Math.ceil(workbenchPages.retainedBytes / 1024)}KiB / 운용 8MiB</span><span>{result.consistency.kind === "transaction_snapshot" ? snapshot?.snapshotId === result.consistency.snapshotId && !snapshotExpired ? "스냅샷에서 조회한 결과입니다." : "종료되거나 만료된 스냅샷의 보유 결과입니다." : result.nextCursor ? "다음 페이지는 다른 시점의 데이터일 수 있습니다." : result.completeness === "limited" ? "정렬과 전체 범위를 보장하지 않는 읽기 전용 미리보기입니다." : "현재 조회 범위를 표시합니다."}</span><button className="button button-secondary" type="button" disabled={!result.nextCursor || busy || expiredResult || (result.consistency.kind === "transaction_snapshot" && (snapshot?.snapshotId !== result.consistency.snapshotId || snapshotExpired))} onClick={() => void nextPage()}>다음 행</button></div>
          <details className="wb-inspector-fold"><summary>선택 행 상세</summary><RowInspector key={result.resultSetId} row={row} columns={result.columns} selection={selection} identity={identity} client={client} /></details>
          {relation && sessionReference && <RelationMetadata key={relation.object.catalogId} client={client} session={sessionReference} relation={relation} columns={result.columns} />}
        </> : <div className="wb-empty"><h3>스키마에서 테이블을 선택하세요.</h3><p>실제 연결 대상에 대한 메타데이터를 필요한 페이지만 읽습니다. SQL은 자동 실행하지 않습니다.</p></div>}</div>
        {editSource && <details className="wb-edit-fold" open><summary>데이터 변경 검토</summary><EditDraft client={client} session={{ ...session, state: busy || sqlRunning ? "busy" : session.state }} object={editSource.object} resultSetId={editSource.resultSetId} columns={editSource.columns} selectedRow={result?.resultSetId === editSource.resultSetId ? row : undefined} transaction={transaction} onDirty={setDirtyEdit} onRefresh={() => void browse(editSource.object)} suspended={closeDialog || historyOpen || movementDialog !== null} /></details>}
        {target && <details className="wb-protection-fold"><summary>백업 보호와 변경 전 별도 준비</summary><ProtectionPanel client={client} gateway={protection} session={session} target={target} currentObject={relation} /></details>}
        {movementJobs.map((job) => <MovementProgressPanel key={job.operationId} client={client} operationId={job.operationId} kind={job.kind} onContinue={job.kind === "import" ? continueImport : undefined} />)}
      </section>
    </div>}
    {targetDetailsOpen && session && <Dialog title="실제 연결 대상 전체 보기" description="현재 네이티브 세션이 확인한 대상입니다. 표시 이름은 권한이나 대상 정체성의 증거가 아닙니다." onClose={() => setTargetDetailsOpen(false)}><pre className="wb-full-target mono" tabIndex={0}>{session.actualContext.endpoint} / {session.actualContext.database}</pre><p>사용자 {session.actualContext.role || "파일 권한"} · {session.actualContext.purpose === "restored" ? "복원 대상" : "원본"} · {mode}</p><button className="button button-secondary" type="button" onClick={() => setTargetDetailsOpen(false)}>닫기</button></Dialog>}
    {movementDialog === "import" && session && editSource && <ImportReview client={client} session={session} object={editSource.object} columns={editSource.columns} transaction={transaction} parentOperationId={continuationParent} suspended={closeDialog || historyOpen} onAdmitted={(id) => admitMovement(id, "import")} onClose={() => setMovementDialog(null)} />}
    {movementDialog === "export" && session && <ExportDialog client={client} session={session} engine={target?.engine ?? "unknown"} shown={shownResult} transaction={transaction} suspended={closeDialog || historyOpen} onAdmitted={(id) => admitMovement(id, "export")} onClose={() => setMovementDialog(null)} />}
    {historyOpen && (session || target) && <OperationHistory client={client} scope={session ? { workspaceId: session.workspaceId, connectionProfileId: session.connectionProfileId, databaseTargetId: session.databaseTargetId } : { workspaceId, connectionProfileId: target!.connectionId, databaseTargetId: target!.id }} suspended={closeDialog || movementDialog !== null} onContinueImport={session && editSource ? continueImport : undefined} onClose={() => setHistoryOpen(false)} />}
    {closeDialog && <Dialog title="연결을 닫을까요?" description="열린 트랜잭션의 롤백을 요청하고 연결 종료 결과를 확인합니다. 확인되지 않은 결과는 미확인 상태로 유지합니다." onClose={() => { if (!busy) { setCloseDialog(false); setPendingDeparture(null); } }}>
      {unsavedDrafts && <label className="wb-check"><input type="checkbox" checked={discardSql} disabled={busy} onChange={(event) => setDiscardSql(event.currentTarget.checked)} />현재 창의 저장하지 않은 SQL·변경 초안을 버립니다.</label>}
      <div className="dialog-actions"><button type="button" className="button button-secondary" disabled={busy} onClick={() => { setCloseDialog(false); setPendingDeparture(null); }}>계속 조회</button><button type="button" className="button button-danger" disabled={busy || (unsavedDrafts && !discardSql)} onClick={() => void close()}>롤백 요청하고 연결 닫기</button></div>
    </Dialog>}
  </div>;
}
