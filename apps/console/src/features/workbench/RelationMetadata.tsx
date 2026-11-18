import { useEffect, useState } from "react";
import { nativeFreshness, readonlyReasonText, workbenchErrorText, workbenchPages, type WorkbenchClient } from "../../lib/workbench";
import type { CatalogDetail, CatalogItem, Column, SessionRef, WorkbenchCommands, WorkbenchPage } from "../../lib/workbenchTypes";

type Part = WorkbenchCommands["catalog.describe"][0]["part"];
export function RelationMetadata({ client, session, relation, columns }: { client: WorkbenchClient; session: SessionRef; relation: CatalogItem; columns: Column[] }) {
  const [open, setOpen] = useState(false), [part, setPart] = useState<Part>("columns"), [cursor, setCursor] = useState<string | null>(null);
  const [page, setPage] = useState<WorkbenchPage<CatalogDetail> | null>(null), [error, setError] = useState("");
  const key = JSON.stringify([session, relation.object, part, cursor]);
  useEffect(() => {
    if (!open) return;
    let active = true; setPage(null); setError("");
    const cacheKey = `describe:${key}`, cached = workbenchPages.get<WorkbenchPage<CatalogDetail>>(cacheKey);
    void (cached ? Promise.resolve(cached) : client.request("catalog.describe", { session, object: relation.object, part, page: { cursor, limit: 100 } })).then((value) => {
      if (!active) return; workbenchPages.put(cacheKey, value); setPage(value);
    }).catch((failure: unknown) => { if (active) setError(workbenchErrorText(failure)); });
    return () => { active = false; };
  }, [client, key, open]);
  const columnNames = new Map(columns.map((column) => [column.columnId, column.name]));
  const names = (ids: string[]) => ids.map((id) => columnNames.get(id) ?? id).join(", ");
  return <details className="wb-relation-metadata" onToggle={(event) => setOpen(event.currentTarget.open)}>
    <summary>열·키·외래키 정보</summary>
    {open && <><label>표시할 메타데이터<select value={part} onChange={(event) => { setPart(event.target.value as Part); setCursor(null); }}><option value="columns">열</option><option value="keys">고유 키</option><option value="indexes">인덱스</option><option value="foreign_keys">외래키</option></select></label>
      {error ? <p role="alert">{error}</p> : !page ? <p role="status">메타데이터를 읽는 중입니다.</p> : <>
        <ul>{page.items.map((item, index) => <li key={index}>{item.kind === "column" ? <>{item.column.name} · <code>{item.column.typeName}</code> · {item.column.nullable ? "NULL 허용" : "NULL 불가"} · {readonlyReasonText(item.column.readonlyReason)}</> : item.kind === "foreign_key" ? <>{names(item.columns)} → <code>{item.referencedRelation.catalogId}</code><small>참조 버전 {item.referencedRelation.schemaVersion}</small></> : item.kind === "key" ? <>{item.primary ? "기본 키" : "고유 키"}: {names(item.columns)}{item.nullable || item.partial || item.expression ? " · 전체 행의 안정된 순서로 사용할 수 없습니다." : ""}</> : <>인덱스: {names(item.columns)}{item.unique ? " · 고유" : ""}{item.predicate ? " · 조건부 인덱스" : ""}</>}</li>)}</ul>
        {page.items.length === 0 && <p>현재 범위에 표시할 항목이 없습니다.</p>}
        <div className="wb-list-foot"><span>{nativeFreshness(page.asOfMs)} · {page.completeness === "limited" ? "일부 항목" : "현재 범위"}</span><button className="button button-secondary" type="button" disabled={!page.nextCursor} onClick={() => setCursor(page.nextCursor)}>다음 메타데이터</button></div>
      </>}
      <small className="wb-muted">선택한 테이블의 버전 {relation.object.schemaVersion}</small>
    </>}
  </details>;
}
