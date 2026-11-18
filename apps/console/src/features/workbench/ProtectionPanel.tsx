import { useEffect, useRef, useState } from "react";
import { BackupDependency } from "./BackupDependency";
import { useMutationAttemptRegistry } from "../../lib/mutationAttempts";
import type { BackupScopeDiscoveryRun, BackupScopeSaveResult, DatabaseTarget, WorkItemStatus } from "../../lib/types";
import { WorkbenchError, nativeFreshness, type WorkbenchClient } from "../../lib/workbench";
import { getProtectionSelection, protectionErrorText, selectionProof, type WorkbenchProtectionGateway } from "../../lib/workbenchProtection";
import type { CatalogItem, Session, WorkbenchSelection, WorkbenchSelectionProof } from "../../lib/workbenchTypes";

export function ProtectionPanel({ client, gateway, session, target, currentObject }: { client: WorkbenchClient; gateway: WorkbenchProtectionGateway; session: Session; target: DatabaseTarget; currentObject: CatalogItem | null }) {
  const [objects, setObjects] = useState<CatalogItem[]>([]), [selection, setSelection] = useState<WorkbenchSelection | null>(null), [proof, setProof] = useState<WorkbenchSelectionProof | null>(null);
  const [run, setRun] = useState<BackupScopeDiscoveryRun | null>(null), [work, setWork] = useState<WorkItemStatus | null>(null), [name, setName] = useState(`${target.displayName} 선택 테이블`), [confirmed, setConfirmed] = useState(false);
  const [saved, setSaved] = useState<BackupScopeSaveResult | null>(null), [busy, setBusy] = useState(false), [error, setError] = useState(""), [beforeChange, setBeforeChange] = useState(false);
  const [clock, setClock] = useState(Date.now);
  const attempts = useMutationAttemptRegistry(), binding = JSON.stringify([session.sessionId, session.connectionGeneration, session.descriptorRevision]);
  const current = useRef(binding); current.current = binding;
  const unsupported = target.engine === "sqlite";
  const proofExpired = !!proof && (BigInt(proof.verifiedAtMs) > BigInt(clock) || BigInt(clock) - BigInt(proof.verifiedAtMs) > 30_000n);
  useEffect(() => { if (!proof) return; setClock(Date.now()); const timer = window.setInterval(() => setClock(Date.now()), 1000); return () => window.clearInterval(timer); }, [proof]);
  useEffect(() => { setObjects([]); setSelection(null); setProof(null); setRun(null); setWork(null); setConfirmed(false); setSaved(null); setError(""); }, [binding]);
  useEffect(() => {
    if (!run || !selection || proof) return;
    let active = true, pending = false, finished = false;
    async function refresh(): Promise<void> {
      if (pending || finished) return; pending = true;
      try {
        const next = await gateway.work(run!.workItemId);
        if (!active) return; if (next.workItemId !== run!.workItemId) throw new WorkbenchError("WB_BINDING_STALE"); setWork(next);
        if (next.state === "succeeded") setProof(selectionProof(next, selection!, run!.operationRunId));
        else if (["failed", "failed_terminal", "cancelled", "partially_succeeded"].includes(next.state)) { finished = true; setError("선택 구조 확인이 성공하지 않았습니다. 이 실행을 근거로 범위를 저장하지 않습니다."); }
      } catch (failure) { if (active) setError(protectionErrorText(failure)); }
      finally { pending = false; }
    }
    void refresh(); const timer = window.setInterval(() => void refresh(), 2000); return () => { active = false; window.clearInterval(timer); };
  }, [gateway, run, selection, proof]);
  function invalidate(): void { setSelection(null); setProof(null); setRun(null); setWork(null); setConfirmed(false); setSaved(null); setError(""); }
  function add(): void {
    if (!currentObject || currentObject.kind !== "table" || unsupported || busy) return;
    if (objects.length >= 100 && !objects.some((object) => object.object.catalogId === currentObject.object.catalogId)) { setError("테이블 선택은 100개까지 보유합니다. 전체 목록을 자동으로 수집하지 않습니다."); return; }
    invalidate(); setObjects([...objects.filter((object) => object.object.catalogId !== currentObject.object.catalogId), currentObject]);
  }
  async function discover(): Promise<void> {
    if (busy || unsupported || objects.length === 0) return; const captured = binding; setBusy(true); setError(""); setProof(null); setConfirmed(false);
    try {
      const selected = await getProtectionSelection(client, session, objects.map((object) => object.object));
      if (current.current !== captured) return;
      const payload = { databaseTargetId: session.databaseTargetId, workbenchSelection: selected };
      const result = await attempts.run("workbench-scope-discovery", "backup_scope.discovery_run", payload, (key) => gateway.discover(session.databaseTargetId, selected, key));
      if (current.current === captured) { setSelection(selected); setRun(result); setWork(null); }
    } catch (failure) { if (current.current === captured) setError(protectionErrorText(failure)); }
    finally { if (current.current === captured) setBusy(false); }
  }
  async function save(): Promise<void> {
    if (!proof || !selection || !run || !confirmed || busy || !name.trim() || BigInt(Date.now()) - BigInt(proof.verifiedAtMs) > 30_000n || BigInt(proof.verifiedAtMs) > BigInt(Date.now())) return;
    const captured = binding; setBusy(true); setError("");
    try {
      const payload = { databaseTargetId: session.databaseTargetId, displayName: name.trim(), mode: "explicit_tables" as const, discoveryRunId: run.operationRunId, engineObjectIds: selection.items.map((item) => item.engineObjectId), expectedWorkbenchSelectionDigest: proof.selectionDigest };
      const result = await attempts.run("workbench-scope-save", "backup_scope.save", payload, (key) => gateway.save(payload, key));
      if (result.databaseTargetId !== session.databaseTargetId || result.mode !== "explicit_tables") throw new WorkbenchError("WB_BINDING_STALE");
      if (current.current === captured) { setSaved(result); setConfirmed(false); }
    } catch (failure) { if (current.current === captured) setError(protectionErrorText(failure)); }
    finally { if (current.current === captured) setBusy(false); }
  }
  return <section className="wb-protection" aria-label="선택 테이블의 백업 보호 연결"><h3>선택 테이블을 기존 백업 범위에 연결</h3>
    <p className="mono">{session.actualContext.endpoint} / {session.actualContext.database} · 현재 Workspace의 등록 대상</p>
    {unsupported && <p className="wb-notice" role="status">SQLite의 명시 테이블 백업은 지원하지 않습니다. 이 선택을 전체 데이터베이스 백업으로 자동 확장하지 않습니다.</p>}
    <div className="wb-query-actions"><button className="button button-secondary" type="button" disabled={busy || unsupported || !currentObject || currentObject.kind !== "table"} onClick={add}>현재 테이블을 보호 선택에 추가</button><span>선택 {objects.length} / 100개</span></div>
    <ul className="wb-protection-selection">{objects.map((object) => <li key={object.object.catalogId}>{object.name} · 관측 구조 <code>{object.object.schemaVersion.slice(0, 12)}</code><button className="wb-text-button" type="button" disabled={busy} onClick={() => { invalidate(); setObjects(objects.filter((item) => item.object.catalogId !== object.object.catalogId)); }}>선택 제거</button></li>)}</ul>
    <button className="button button-secondary" type="button" disabled={busy || unsupported || objects.length === 0} onClick={() => void discover()}>이 선택의 구조를 서비스에서 다시 확인</button>
    {run && <p role="status">구조 확인 작업 {run.workItemId} · {work?.state === "succeeded" ? "성공 확인" : work?.state === "running" ? "확인 중" : work?.state === "queued" || !work ? "접수됨" : "완료 근거 확인 필요"}</p>}
    {proof && <><p>{nativeFreshness(proof.verifiedAtMs)}에 확인한 정확한 {proof.items.length}개 테이블 · 구조 확인은 30초 동안 유효하며 만료·변경 시 다시 검토합니다.{proofExpired ? " 확인 기한이 지났습니다." : ""}</p><label className="wb-query-label">저장할 백업 범위 이름<input maxLength={128} value={name} disabled={busy} onChange={(event) => { setName(event.currentTarget.value); setConfirmed(false); }} /></label><label className="wb-check"><input type="checkbox" checked={confirmed} disabled={busy || !!saved || proofExpired} onChange={(event) => setConfirmed(event.currentTarget.checked)} />실제 대상과 정확한 선택을 확인했습니다. 기존 범위·계획을 자동 변경하지 않고 새 범위를 저장합니다.</label><button className="button button-primary" type="button" disabled={busy || !confirmed || !name.trim() || !!saved || proofExpired} onClick={() => void save()}>확인한 선택으로 백업 범위 저장</button></>}
    {saved && <p role="status">백업 범위 {saved.scopeId} 저장 확인 · 데이터 백업 완료를 의미하지 않습니다.</p>}
    <label className="wb-check"><input type="checkbox" checked={beforeChange} onChange={(event) => setBeforeChange(event.currentTarget.checked)} />변경 전에 백업을 별도 작업으로 준비</label>
    {beforeChange && <BackupDependency gateway={gateway} session={session} preferredScopeId={saved?.scopeId} />}

    {error && <p className="wb-error" role="alert">{error}</p>}
  </section>;
}
