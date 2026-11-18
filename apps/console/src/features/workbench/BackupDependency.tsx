import { useEffect, useRef, useState } from "react";
import { formatDateTime } from "../../lib/format";
import { routeFor } from "../../lib/route";
import type { BackupScope, Snapshot } from "../../lib/types";
import { WorkbenchError } from "../../lib/workbench";
import { backupDependencyEvidence, protectionErrorText, type WorkbenchProtectionGateway } from "../../lib/workbenchProtection";
import type { Session } from "../../lib/workbenchTypes";

export function BackupDependency({ gateway, session, preferredScopeId }: { gateway: WorkbenchProtectionGateway; session: Session; preferredScopeId?: string }) {
  const [scopes, setScopes] = useState<BackupScope[]>([]), [nextCursor, setNextCursor] = useState<string | null>(null), [selected, setSelected] = useState("");
  const [pinned, setPinned] = useState<{ scope: BackupScope; snapshot: Snapshot; checkedAt: number } | null>(null), [busy, setBusy] = useState(false), [error, setError] = useState("");
  const binding = JSON.stringify([session.sessionId, session.connectionGeneration, session.databaseTargetId, session.connectionProfileId]), current = useRef(binding); current.current = binding;
  async function load(cursor: string | null): Promise<void> {
    const captured = binding; setBusy(true); setError(""); setPinned(null); setSelected("");
    try {
      if (!gateway.scopes) throw new WorkbenchError("WB_NATIVE_UNAVAILABLE");
      const page = await gateway.scopes(session.databaseTargetId, cursor);
      if (current.current !== captured) return;
      if (page.items.length > 20 || page.items.some((scope) => scope.databaseTargetId !== session.databaseTargetId || scope.connectionId !== session.connectionProfileId)) throw new WorkbenchError("WB_BINDING_STALE");
      setScopes(page.items); setNextCursor(page.nextCursor);
      if (page.items.some((scope) => scope.id === preferredScopeId)) setSelected(preferredScopeId!);
    } catch (failure) { if (current.current === captured) { setScopes([]); setNextCursor(null); setError(protectionErrorText(failure)); } }
    finally { if (current.current === captured) setBusy(false); }
  }
  useEffect(() => { void load(null); return () => { current.current = ""; }; }, [gateway, binding]);
  async function pin(): Promise<void> {
    const scope = scopes.find((item) => item.id === selected), captured = binding;
    if (busy || !scope?.latestRecoveryPointId) return;
    setBusy(true); setError(""); setPinned(null);
    try {
      if (!gateway.snapshot) throw new WorkbenchError("WB_NATIVE_UNAVAILABLE");
      const snapshot = await gateway.snapshot(scope.latestRecoveryPointId);
      backupDependencyEvidence(session, scope, snapshot);
      if (current.current === captured) setPinned({ scope, snapshot, checkedAt: Date.now() });
    } catch (failure) { if (current.current === captured) setError(protectionErrorText(failure)); }
    finally { if (current.current === captured) setBusy(false); }
  }
  return <div className="wb-notice" aria-label="변경 전 별도 백업 참고 근거">
    <p>현재 대상 {session.actualContext.endpoint} / {session.actualContext.database}의 범위를 직접 선택하고, 범위에 연결된 복구지점 상세를 확인하세요.</p>
    <label className="wb-query-label">참고할 백업 범위<select disabled={busy} value={selected} onChange={(event) => { setSelected(event.currentTarget.value); setPinned(null); setError(""); }}><option value="">범위 선택</option>{scopes.map((scope) => <option key={scope.id} value={scope.id}>{scope.displayName} · {scope.latestRecoveryPointId ? "복구지점 있음" : "복구지점 없음"}</option>)}</select></label>
    <div className="wb-query-actions"><button className="button button-secondary" type="button" disabled={busy || !scopes.some((scope) => scope.id === selected && !!scope.latestRecoveryPointId)} onClick={() => void pin()}>복구지점 상세 확인 후 근거 고정</button><button className="wb-text-button" type="button" disabled={busy} onClick={() => void load(null)}>범위 처음부터 새로 확인</button>{nextCursor && <button className="wb-text-button" type="button" disabled={busy} onClick={() => void load(nextCursor)}>다음 범위 20개</button>}</div>
    {busy && <p role="status">범위와 복구지점 근거를 확인하고 있습니다.</p>}
    {pinned && <div role="status"><p>참고 근거 고정 · 대상 {session.databaseTargetId} · 범위 {pinned.scope.displayName} ({pinned.scope.id})</p><p>복구지점 <code>{pinned.snapshot.id}</code> · 완료 / 서명 확인됨 · 완료 시각 {formatDateTime(pinned.snapshot.committedAtMs)} · 확인 당시 경과 {Math.floor((pinned.checkedAt - pinned.snapshot.committedAtMs!) / 60000)}분</p><p>상세 응답에 백업 실행 ID가 없어 복구지점 ID로 기록합니다. 선택한 근거는 현재 화면 메모리에만 보관합니다.</p><a href={routeFor("snapshots", { selected: pinned.snapshot.id, scope: pinned.scope.id, target: session.databaseTargetId })}>고정한 복구지점 근거 열기</a></div>}
    {error && <p className="wb-error" role="alert">{error} 완료 상태·서명·실제 대상·범위가 일치하는 복구지점만 참고 근거로 고정합니다.</p>}
    <p>이 근거는 별도 백업 작업에 대한 참고 의존성입니다. SQL·변경 초안·가져오기는 자동으로 실행되지 않습니다. 백업과 변경은 하나의 트랜잭션이 아니며 자동 되돌리기를 보장하지 않습니다.</p>
    <a href={routeFor("backup-plans", { action: "new", ...(selected ? { scope: selected } : {}) })}>기존 백업 계획에서 별도 준비</a>
  </div>;
}
