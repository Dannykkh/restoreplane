import { useEffect, useState } from "react";
import { Dialog } from "../../components/Dialog";
import { workbenchErrorText, type WorkbenchClient } from "../../lib/workbench";
import type { NativeCounterU64, SavedQuery, Scope, WorkbenchPage } from "../../lib/workbenchTypes";
import { sqlBytes } from "./QueryEditor";

export function SavedQueries({ client, scope, mode, sql, queryId, version, name, onLoaded, onSaved, onDeleted, onClose }: {
  client: WorkbenchClient; scope: Scope; mode: "load" | "save"; sql: string; queryId: string | null;
  version: NativeCounterU64 | null; name: string; onLoaded: (query: SavedQuery) => void;
  onSaved: (query: SavedQuery) => void; onDeleted?: () => void; onClose: () => void;
}) {
  const [page, setPage] = useState<WorkbenchPage<{ queryId: string; name: string }> | null>(null), [cursor, setCursor] = useState<string | null>(null);
  const [savedName, setSavedName] = useState(name), [consent, setConsent] = useState(false), [busy, setBusy] = useState(false), [error, setError] = useState("");
  const [deleteConfirmed, setDeleteConfirmed] = useState(false);
  const scopeKey = JSON.stringify(scope);
  useEffect(() => {
    if (mode !== "load") return;
    let active = true; setBusy(true); setError(""); setPage(null);
    void client.request("saved_query.list", { scope, page: { cursor, limit: 50 } }).then((value) => { if (active) setPage(value); }).catch((failure: unknown) => { if (active) setError(workbenchErrorText(failure)); }).finally(() => { if (active) setBusy(false); });
    return () => { active = false; };
  }, [client, scopeKey, mode, cursor]);
  async function load(id: string): Promise<void> {
    setBusy(true); setError("");
    try { onLoaded(await client.request("saved_query.get", { queryId: id })); onClose(); }
    catch (failure) { setError(workbenchErrorText(failure)); }
    finally { setBusy(false); }
  }
  async function save(): Promise<void> {
    if (!consent || !savedName.trim() || busy) return;
    if (sqlBytes(savedName) + sqlBytes(sql) > 240 * 1024) { setError("암호화 저장할 이름과 SQL은 합쳐서 240KiB 이하로 줄여 주세요. 편집 초안은 그대로 유지됩니다."); return; }
    setBusy(true); setError("");
    try {
      const saved = await client.request("saved_query.put", { scope, queryId, name: savedName, sql, expectedVersion: version });
      onSaved({ ...saved, name: savedName, sql }); onClose();
    } catch (failure) { setError(workbenchErrorText(failure)); }
    finally { setBusy(false); }
  }
  async function remove(): Promise<void> {
    if (!queryId || !version || !deleteConfirmed || busy || !onDeleted) return;
    setBusy(true); setError("");
    try { const reply = await client.request("saved_query.delete", { queryId, expectedVersion: version }); if (!reply.deleted) { setError("저장 항목 삭제를 확인하지 못했습니다. 현재 초안은 유지합니다."); return; } onDeleted(); onClose(); }
    catch (failure) { setError(workbenchErrorText(failure)); }
    finally { setBusy(false); }
  }
  return <Dialog title={mode === "load" ? "저장한 SQL 열기" : "SQL 이름을 정해 저장"} description="저장한 이름과 SQL은 현재 사용자용 암호화 저장소에서만 읽고 씁니다. 실행 이력이나 결과를 자동 저장하지 않습니다." onClose={() => { if (!busy) onClose(); }}>
    {mode === "load" ? <>
      {busy && <p role="status">저장한 SQL을 읽는 중입니다.</p>}
      {page?.items.map((query) => <button type="button" className="wb-saved-item" key={query.queryId} disabled={busy} onClick={() => void load(query.queryId)}>{query.name}</button>)}
      {page && page.items.length === 0 && <p>현재 범위에 저장한 SQL이 없습니다.</p>}
      <button className="button button-secondary" type="button" disabled={busy || !page?.nextCursor} onClick={() => setCursor(page?.nextCursor ?? null)}>다음 목록</button>
    </> : <form onSubmit={(event) => { event.preventDefault(); void save(); }}>
      <label className="wb-query-label">저장할 이름<input value={savedName} maxLength={128} onChange={(event) => setSavedName(event.currentTarget.value)} disabled={busy} /></label>
      <pre className="wb-wire-sql">{sql}</pre>
      <label className="wb-check"><input type="checkbox" checked={consent} disabled={busy} onChange={(event) => setConsent(event.currentTarget.checked)} />이 이름과 SQL을 현재 사용자용 암호화 저장소에 저장합니다.</label>
      <div className="dialog-actions"><button className="button button-secondary" type="button" disabled={busy} onClick={onClose}>취소</button><button className="button button-primary" type="submit" disabled={busy || !consent || !savedName.trim()}>암호화해서 저장</button></div>
      {queryId && version && onDeleted && <fieldset><legend>저장 항목 삭제</legend><label className="wb-check"><input type="checkbox" checked={deleteConfirmed} disabled={busy} onChange={(event) => setDeleteConfirmed(event.currentTarget.checked)} />암호화 저장소의 이 항목을 삭제합니다. 편집 중인 SQL 초안은 창에 유지합니다.</label><button className="button button-danger" type="button" disabled={busy || !deleteConfirmed} onClick={() => void remove()}>확인하고 저장 항목 삭제</button></fieldset>}
    </form>}
    {error && <p className="wb-error" role="alert">{error}</p>}
  </Dialog>;
}
