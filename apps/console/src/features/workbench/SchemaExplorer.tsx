import { useEffect, useRef, useState } from "react";
import { useDebouncedValue } from "../../lib/queries";
import { workbenchErrorText, workbenchPages, type WorkbenchClient } from "../../lib/workbench";
import type { CatalogItem, ObjectRef, SessionRef, WorkbenchPage } from "../../lib/workbenchTypes";

export function SchemaExplorer({ client, session, onRelation, onSchema }: { client: WorkbenchClient; session: SessionRef; onRelation: (item: CatalogItem) => void; onSchema: (schema: CatalogItem | null) => void }) {
  const [parent, setParent] = useState<CatalogItem | null>(null);
  const [search, setSearch] = useState(""); const debounced = useDebouncedValue(search);
  const [cursor, setCursor] = useState<string | null>(null);
  const [page, setPage] = useState<WorkbenchPage<CatalogItem> | null>(null);
  const [state, setState] = useState<"loading" | "ready" | "error">("loading");
  const [error, setError] = useState(""); const [refresh, setRefresh] = useState(0);
  const [scroll, setScroll] = useState(0); const list = useRef<HTMLDivElement>(null);
  const requestKey = JSON.stringify([session, parent?.object, debounced, cursor, refresh]);
  useEffect(() => {
    let active = true;
    const payload = { session, parent: parent?.object ?? null, kind: parent ? "relations" as const : "schemas" as const, search: debounced, page: { cursor, limit: 100 } };
    setState("loading"); setPage(null); setScroll(0); if (list.current) list.current.scrollTop = 0;
    const key = `catalog:${requestKey}`, cached = workbenchPages.get<WorkbenchPage<CatalogItem>>(key);
    const load = cached ? Promise.resolve(cached) : client.request("catalog.page", payload);
    void load.then((value) => {
      if (!active) return;
      if (value.items.length > 200) throw new Error("WB_LIMIT");
      workbenchPages.put(key, value); setPage(value); setState("ready");
    }).catch((failure: unknown) => { if (active) { setError(workbenchErrorText(failure)); setState("error"); } });
    return () => { active = false; };
  // requestKey contains the full immutable session, scope, version and page identity.
  }, [client, requestKey]);
  function enter(item: CatalogItem): void {
    if (!parent) { setParent(item); onSchema(item); setCursor(null); setSearch(""); }
    else onRelation(item);
  }
  function root(): void { setParent(null); onSchema(null); setCursor(null); setSearch(""); }
  const from = Math.max(0, Math.floor(scroll / 32) - 3), to = Math.min(page?.items.length ?? 0, from + 22);
  return <aside className="wb-explorer" aria-label="스키마 탐색기">
    <div className="wb-panel-title"><h3>스키마 탐색</h3><button type="button" className="wb-text-button" onClick={() => { setCursor(null); setRefresh((value) => value + 1); }}>새로고침</button></div>
    <label className="wb-search">이름 검색<input value={search} onChange={(event) => { setSearch(event.target.value); setCursor(null); }} placeholder="스키마 또는 테이블 이름" maxLength={256} /></label>
    <div className="wb-path"><button type="button" onClick={root} className="wb-text-button">스키마</button>{parent && <><span>/</span><strong title={parent.name}>{parent.name}</strong></>}</div>
    {state === "loading" && <p role="status">메타데이터를 불러오는 중입니다.</p>}
    {state === "error" && <p role="alert">{error}</p>}
    {state === "ready" && page?.items.length === 0 && <p>일치하는 항목이 없습니다.</p>}
    <div ref={list} className="wb-tree" role="tree" aria-label={parent ? "테이블" : "스키마"} onScroll={(event) => setScroll(event.currentTarget.scrollTop)}>
      <div style={{ height: (page?.items.length ?? 0) * 32, position: "relative" }}>
        {page?.items.slice(from, to).map((item, index) => <button type="button" role="treeitem" key={objectKey(item.object)}
          aria-level={parent ? 2 : 1} aria-posinset={from + index + 1} aria-setsize={-1} aria-expanded={parent ? undefined : false}
          className="wb-tree-item" style={{ top: (from + index) * 32 }} onClick={() => enter(item)}
          onKeyDown={(event) => {
            if (event.nativeEvent.isComposing) return;
            if (event.key === "ArrowRight" || event.key === "Enter") { event.preventDefault(); enter(item); }
            if (event.key === "ArrowLeft" && parent) { event.preventDefault(); root(); }
            if (event.key === "ArrowDown" || event.key === "ArrowUp") {
              event.preventDefault(); const destination = Math.min((page?.items.length ?? 1) - 1, Math.max(0, from + index + (event.key === "ArrowDown" ? 1 : -1)));
              if (list.current) { list.current.scrollTop = Math.max(0, destination * 32 - 160); setScroll(list.current.scrollTop); }
              requestAnimationFrame(() => list.current?.querySelectorAll<HTMLButtonElement>("[role=treeitem]")[Math.max(0, destination - Math.max(0, Math.floor((list.current?.scrollTop ?? 0) / 32) - 3))]?.focus());
            }
          }}><span aria-hidden="true">{parent ? "▦" : "▸"}</span><span>{item.name}</span><small>{item.kind}</small></button>)}
      </div>
    </div>
    {page && <div className="wb-list-foot"><span>{page.count.kind === "unknown" ? "총 개수 미확인" : `${page.count.kind === "estimate" ? "추정 " : "전체 "}${page.count.value}`}</span><button className="button button-secondary" type="button" disabled={!page.nextCursor || state === "loading"} onClick={() => setCursor(page.nextCursor)}>다음 목록</button></div>}
  </aside>;
}
export function objectKey(object: ObjectRef): string { return `${object.catalogId}:${object.schemaVersion}`; }
