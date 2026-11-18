import { useState } from "react";
import { cellText, readonlyReasonText, workbenchErrorText, type WorkbenchClient } from "../../lib/workbench";
import { nativeU64, type Column, type ResultRow, type StreamIdentity } from "../../lib/workbenchTypes";
import type { SelectionAnchor } from "./VirtualResultGrid";

export function RowInspector({ row, columns, selection, identity, client }: { row: ResultRow | undefined; columns: Column[]; selection: SelectionAnchor | null; identity: StreamIdentity | null; client: WorkbenchClient }) {
  const [detail, setDetail] = useState<{ key: string; base64: string } | null>(null);
  const [notice, setNotice] = useState("");
  const selectedColumn = columns.findIndex((column) => column.columnId === selection?.columnId);
  async function copyCell(cell: ResultRow["cells"][number]): Promise<void> {
    if (!navigator.clipboard?.writeText) { setNotice("현재 환경에서는 클립보드를 사용할 수 없습니다."); return; }
    try { await navigator.clipboard.writeText(JSON.stringify(cell)); setNotice("형식을 포함한 값을 복사했습니다."); }
    catch { setNotice("클립보드에 접근하지 못했습니다."); }
  }
  async function readLarge(): Promise<void> {
    const cell = row?.cells[selectedColumn];
    if (!identity || cell?.kind !== "large") return;
    try {
      const reply = await client.request("result.value.get", { identity, valueHandle: cell.valueHandle, offset: nativeU64("0"), maxBytes: 64 * 1024 });
      setDetail({ key: cell.valueHandle, base64: reply.base64 });
      setNotice(reply.nextOffset === null ? "값 읽기를 완료했습니다." : "앞 64KiB만 읽었습니다. 전체 값으로 표시하지 않습니다.");
    } catch (error) { setNotice(workbenchErrorText(error)); }
  }
  return <section className="wb-inspector" aria-label="행 상세">
    <h3>행 상세</h3>
    {!row ? <p className="wb-muted">결과에서 셀을 선택하면 행의 형식과 원문을 확인할 수 있습니다.</p> : <>
      <span className="mono wb-muted">행 {row.rowHandle}</span>
      <dl>{columns.slice(0, 512).map((column, index) => {
        const cell = row.cells[index];
        if (!cell) return null;
        const selected = column.columnId === selection?.columnId;
        const readonly = cell.kind === "unsupported" ? readonlyReasonText(cell.reason) : cell.kind === "large" ? "잘린 미리보기는 편집할 수 없습니다." : readonlyReasonText(column.readonlyReason);
        return <div key={column.columnId} className={selected ? "wb-field-selected" : ""}>
          <dt>{column.name} <span className="mono">{column.typeName}</span></dt>
          <dd><pre>{cellText(cell).slice(0, 2048)}</pre><small>{readonly}</small>
            {cellText(cell).length > 2048 && <small>화면에는 앞 2,048자만 표시합니다. 원문 형식은 유지됩니다.</small>}
            {cell.kind === "temporal" && <small>오프셋 {cell.offset ?? "없음"} · 정밀도 {cell.precision}</small>}
            {cell.kind === "large" && selected && <><small>원문 {cell.byteLength} bytes</small><button type="button" className="button button-secondary" disabled={!identity} onClick={() => void readLarge()}>앞 64KiB 읽기</button>{detail?.key === cell.valueHandle && <pre>{detail.base64}</pre>}</>}
            <button type="button" className="wb-text-button" onClick={() => void copyCell(cell)}>형식 포함 복사</button>
          </dd>
        </div>;
      })}</dl>
    </>}
    {notice && <p role="status">{notice}</p>}
  </section>;
}
