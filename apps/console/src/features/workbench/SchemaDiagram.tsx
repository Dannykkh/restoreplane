import { useEffect, useState } from "react";
import { WorkbenchError, nativeFreshness, workbenchErrorText, workbenchPages, type WorkbenchClient } from "../../lib/workbench";
import type { CatalogItem, GraphNode, SessionRef, WorkbenchPage } from "../../lib/workbenchTypes";

const MAX_NODES = 80, MAX_EDGES = 200;
export function diagramMermaid(nodes: GraphNode[]): string {
  const ids = new Map(nodes.map((node, index) => [node.nodeId, `n${index}`]));
  const label = (value: string) => value.replaceAll("&", "&amp;").replaceAll('"', "&#34;").replaceAll("<", "&lt;").replaceAll(">", "&gt;").replace(/[\r\n]/g, " ");
  return ["flowchart LR", ...nodes.flatMap((node) => [`  %% ref ${node.relation.catalogId} version ${node.relation.schemaVersion}`, `  ${ids.get(node.nodeId)}["${label(node.name ?? node.nodeId)}"]`]), ...nodes.flatMap((node) => node.edges.filter((edge) => ids.has(edge.to)).map((edge) => `  ${ids.get(node.nodeId)} -->|"${label(edge.name)}"| ${ids.get(edge.to)}`))].join("\n");
}
function download(filename: string, type: string, text: string): void {
  const url = URL.createObjectURL(new Blob([text], { type }));
  const link = document.createElement("a"); link.href = url; link.download = filename; link.click();
  window.setTimeout(() => URL.revokeObjectURL(url), 1000);
}
function exportSvg(node: SVGSVGElement): string {
  const clone = node.cloneNode(true) as SVGSVGElement;
  const original = [node, ...node.querySelectorAll("*")], copies = [clone, ...clone.querySelectorAll("*")];
  original.forEach((element, index) => {
    const style = getComputedStyle(element);
    for (const name of ["fill", "stroke", "font-family", "font-size", "background-color"]) (copies[index] as SVGElement).style.setProperty(name, style.getPropertyValue(name));
  });
  return new XMLSerializer().serializeToString(clone);
}
export function SchemaDiagram({ client, session, schema }: { client: WorkbenchClient; session: SessionRef; schema: CatalogItem }) {
  const [page, setPage] = useState<WorkbenchPage<GraphNode> | null>(null);
  const [cursor, setCursor] = useState<string | null>(null);
  const [error, setError] = useState(""); const [loading, setLoading] = useState(true); const [refresh, setRefresh] = useState(0);
  const svg = useState(() => ({ current: null as SVGSVGElement | null }))[0];
  const key = JSON.stringify([session, schema.object, cursor, refresh]);
  useEffect(() => {
    let active = true; setLoading(true); setPage(null); setError("");
    const cacheKey = `graph:${key}`, cached = workbenchPages.get<WorkbenchPage<GraphNode>>(cacheKey);
    void (cached ? Promise.resolve(cached) : client.request("catalog.graph", { session, schema: schema.object, page: { cursor, limit: MAX_NODES } })).then((value) => {
      if (!active) return;
      if (value.items.length > MAX_NODES || value.items.reduce((count, node) => count + node.edges.length, 0) > MAX_EDGES) throw new WorkbenchError("WB_LIMIT");
      workbenchPages.put(cacheKey, value); setPage(value); setLoading(false);
    }).catch((failure: unknown) => { if (active) { setError(workbenchErrorText(failure)); setLoading(false); } });
    return () => { active = false; };
  }, [client, key]);
  const nodes = page?.items ?? [], width = 640, height = Math.max(240, Math.ceil(nodes.length / 3) * 90);
  const positions = new Map(nodes.map((node, index) => [node.nodeId, { x: 12 + index % 3 * 210, y: 12 + Math.floor(index / 3) * 90 }]));
  return <section className="wb-diagram" aria-label="스키마 관계도">
    <div className="wb-panel-title"><h3>{schema.name} 관계도</h3><div><button className="button button-secondary" type="button" disabled={!page || loading} onClick={() => svg.current && download("workbench-schema.svg", "image/svg+xml", exportSvg(svg.current))}>SVG 내보내기</button><button className="button button-secondary" type="button" disabled={!page || loading} onClick={() => download("workbench-schema.mmd", "text/plain;charset=utf-8", diagramMermaid(nodes))}>Mermaid 내보내기</button></div></div>
    <p className="wb-muted">관계별 메타데이터 버전 보존 · 최대 {MAX_NODES}개 노드 / {MAX_EDGES}개 관계 · 전체 스키마로 간주하지 않습니다.</p>
    <div className="wb-list-foot"><span>{page ? nativeFreshness(page.asOfMs) : "조회시각 미확인"}</span><button type="button" className="wb-text-button" disabled={loading} onClick={() => { setCursor(null); setRefresh((value) => value + 1); }}>관계 새로고침</button></div>
    {loading && <p role="status">관계를 읽는 중입니다.</p>}{error && <p role="alert">{error}</p>}
    <div className="wb-diagram-viewport"><svg ref={(node) => { svg.current = node; }} xmlns="http://www.w3.org/2000/svg" viewBox={`0 0 ${width} ${height}`} role="img" aria-label={`${schema.name} 관계도, ${nodes.length}개 항목`}>
      <title>{schema.name} 관계도</title>
      <metadata>{JSON.stringify({ asOfMs: page?.asOfMs ?? null, relations: nodes.map((node) => node.relation) })}</metadata>
      {nodes.flatMap((node) => node.edges.map((edge, index) => {
        const from = positions.get(node.nodeId), to = positions.get(edge.to);
        return from && to ? <g key={`${node.nodeId}:${index}`}><line x1={from.x + 180} y1={from.y + 28} x2={to.x} y2={to.y + 28} /><title>{edge.name}</title></g> : null;
      }))}
      {nodes.map((node) => { const position = positions.get(node.nodeId)!; return <g key={node.nodeId} transform={`translate(${position.x} ${position.y})`}><rect width={188} height={56} rx={5} /><text x={8} y={23}>{(node.name ?? node.nodeId).slice(0, 23)}</text><text x={8} y={43} className="wb-diagram-id">{node.relation.catalogId.slice(0, 20)}</text><title>{node.name ?? node.nodeId} · {node.relation.catalogId}</title></g>; })}
    </svg></div>
    {page && <div className="wb-list-foot"><span>{page.completeness === "limited" || page.nextCursor ? "일부 관계만 표시" : "현재 범위의 관계 표시"}</span><button className="button button-secondary" type="button" disabled={!page.nextCursor} onClick={() => setCursor(page.nextCursor)}>다음 관계</button></div>}
    <details><summary>텍스트 관계와 버전</summary><ul>{nodes.map((node) => <li key={node.nodeId}>{node.name ?? node.nodeId}: {node.edges.map((edge) => `${edge.name} → ${edge.to}`).join("; ") || "표시할 외래키 없음"}<code>{node.relation.catalogId} @ {node.relation.schemaVersion}</code></li>)}</ul></details>
  </section>;
}
