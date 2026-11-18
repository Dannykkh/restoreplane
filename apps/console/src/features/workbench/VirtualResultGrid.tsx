import { useId, useLayoutEffect, useRef, useState, type KeyboardEvent } from "react";
import { cellText } from "../../lib/workbench";
import type { Column, NativeCounterU64, ResultRow } from "../../lib/workbenchTypes";

export interface SelectionAnchor { resultSetId: string; rowHandle: string; columnId: string }
interface Props {
  resultSetId: string; rows: ResultRow[]; columns: Column[]; startRow?: NativeCounterU64;
  selection: SelectionAnchor | null; onSelect: (selection: SelectionAnchor) => void;
  visible?: boolean;
}
const ROW_HEIGHT = 32, COLUMN_WIDTH = 184, OVERSCAN = 3;
export function VirtualResultGrid({ resultSetId, rows, columns, startRow, selection, onSelect, visible = true }: Props) {
  const viewport = useRef<HTMLDivElement>(null);
  const [position, setPosition] = useState({ top: 0, left: 0, width: 800, height: 384 });
  const composing = useRef(false);
  const prefix = useId().replaceAll(":", "");
  useLayoutEffect(() => {
    const node = viewport.current;
    if (!visible) { composing.current = false; return; }
    if (!node) return;
    node.scrollTop = position.top; node.scrollLeft = position.left;
    const measure = () => setPosition({ top: node.scrollTop, left: node.scrollLeft, width: node.clientWidth || 800, height: node.clientHeight || 384 });
    measure();
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(measure); observer.observe(node); return () => observer.disconnect();
  // Only presentation visibility changes the observer; native stream lifetime is owned by the parent.
  }, [visible]);
  const rowFrom = Math.max(0, Math.floor(position.top / ROW_HEIGHT) - OVERSCAN);
  const rowTo = Math.min(rows.length, rowFrom + Math.ceil(position.height / ROW_HEIGHT) + OVERSCAN * 2);
  const colFrom = Math.max(0, Math.floor(position.left / COLUMN_WIDTH) - OVERSCAN);
  const colTo = Math.min(columns.length, colFrom + Math.ceil(position.width / COLUMN_WIDTH) + OVERSCAN * 2);
  const activeRow = selection?.resultSetId === resultSetId ? rows.findIndex((row) => row.rowHandle === selection.rowHandle) : -1;
  const activeColumn = selection?.resultSetId === resultSetId ? columns.findIndex((column) => column.columnId === selection.columnId) : -1;
  const mountedActive = activeRow >= rowFrom && activeRow < rowTo && activeColumn >= colFrom && activeColumn < colTo;
  const cellId = (row: number, column: number) => `${prefix}-${row}-${column}`;
  function move(event: KeyboardEvent<HTMLDivElement>): void {
    if (event.isDefaultPrevented() || event.nativeEvent.isComposing || event.nativeEvent.keyCode === 229 || composing.current || !rows.length || !columns.length) return;
    let row = Math.max(0, activeRow), column = Math.max(0, activeColumn);
    switch (event.key) {
      case "ArrowDown": row++; break; case "ArrowUp": row--; break;
      case "ArrowRight": column++; break; case "ArrowLeft": column--; break;
      case "PageDown": row += Math.max(1, Math.floor(position.height / ROW_HEIGHT) - 1); break;
      case "PageUp": row -= Math.max(1, Math.floor(position.height / ROW_HEIGHT) - 1); break;
      case "Home": if (event.ctrlKey) row = 0; column = 0; break;
      case "End": if (event.ctrlKey) row = rows.length - 1; column = columns.length - 1; break;
      default: return;
    }
    event.preventDefault(); row = Math.max(0, Math.min(rows.length - 1, row)); column = Math.max(0, Math.min(columns.length - 1, column));
    const node = viewport.current;
    if (node) {
      if (row * ROW_HEIGHT < node.scrollTop) node.scrollTop = row * ROW_HEIGHT;
      if ((row + 2) * ROW_HEIGHT > node.scrollTop + position.height) node.scrollTop = (row + 2) * ROW_HEIGHT - position.height;
      if (column * COLUMN_WIDTH < node.scrollLeft) node.scrollLeft = column * COLUMN_WIDTH;
      if ((column + 1) * COLUMN_WIDTH > node.scrollLeft + position.width) node.scrollLeft = (column + 1) * COLUMN_WIDTH - position.width;
      setPosition((previous) => ({ ...previous, top: node.scrollTop, left: node.scrollLeft }));
    }
    onSelect({ resultSetId, rowHandle: rows[row].rowHandle, columnId: columns[column].columnId });
  }
  if (!visible) return null;
  return <div className="wb-grid-wrap">
    <p id={`${prefix}-help`} className="wb-grid-help">방향키로 셀 이동 · Page Up/Down으로 화면 이동 · 행 상세에서 형식 확인</p>
    <div ref={viewport} className="wb-grid" role="grid" tabIndex={0} aria-label="조회 결과" aria-readonly="true"
      aria-rowcount={-1} aria-colcount={columns.length} aria-describedby={`${prefix}-help`}
      aria-activedescendant={mountedActive ? cellId(activeRow, activeColumn) : undefined}
      onKeyDown={move} onCompositionStart={() => { composing.current = true; }} onCompositionEnd={() => { composing.current = false; }}
      onScroll={(event) => { const { scrollTop, scrollLeft } = event.currentTarget; setPosition((previous) => ({ ...previous, top: scrollTop, left: scrollLeft })); }}>
      <div role="presentation" className="wb-grid-space" style={{ width: columns.length * COLUMN_WIDTH, height: (rows.length + 1) * ROW_HEIGHT }}>
        <div role="row" className="wb-grid-head" aria-rowindex={1} style={{ height: ROW_HEIGHT, width: columns.length * COLUMN_WIDTH }}>
          {columns.slice(colFrom, colTo).map((column, offset) => <div role="columnheader" aria-colindex={colFrom + offset + 1} key={column.columnId} className="wb-grid-cell" style={{ left: (colFrom + offset) * COLUMN_WIDTH, width: COLUMN_WIDTH }} title={`${column.name} · ${column.typeName}`}>{column.name}<span>{column.typeName}</span></div>)}
        </div>
        {rows.slice(rowFrom, rowTo).map((row, offset) => {
          const logicalRow = rowFrom + offset;
          return <div key={row.rowHandle} role="row" ref={(node) => node?.setAttribute("aria-rowindex", (BigInt(startRow ?? "0") + BigInt(logicalRow) + 2n).toString())} aria-label={`행 ${(BigInt(startRow ?? "0") + BigInt(logicalRow) + 1n).toString()}`} className="wb-grid-row" style={{ top: (logicalRow + 1) * ROW_HEIGHT, height: ROW_HEIGHT, width: columns.length * COLUMN_WIDTH }}>
            {columns.slice(colFrom, colTo).map((column, columnOffset) => {
              const index = colFrom + columnOffset, cell = row.cells[index];
              const text = cell ? cellText(cell) : "값 없음";
              const selected = activeRow === logicalRow && activeColumn === index;
              return <div key={column.columnId} id={cellId(logicalRow, index)} role="gridcell" aria-colindex={index + 1}
                aria-selected={selected} aria-label={`${column.name}: ${text.slice(0, 256)}`}
                className={`wb-grid-cell ${selected ? "is-selected" : ""} ${cell?.kind === "null" ? "is-null" : ""}`}
                style={{ left: index * COLUMN_WIDTH, width: COLUMN_WIDTH }}
                onClick={() => { viewport.current?.focus(); onSelect({ resultSetId, rowHandle: row.rowHandle, columnId: column.columnId }); }}>
                {text.slice(0, 256)}{text.length > 256 ? "…" : ""}
              </div>;
            })}
          </div>;
        })}
      </div>
    </div>
    {selection && activeRow < 0 && <p role="status" className="wb-notice">선택한 행은 현재 보유 페이지에 없습니다. 행 선택은 보존되며 자동 재조회하지 않습니다.</p>}
    <span className="wb-sr-only" aria-live="polite">{mountedActive ? `${columns[activeColumn].name}: ${cellText(rows[activeRow].cells[activeColumn]).slice(0, 256)}` : "선택 셀이 현재 화면 밖에 있습니다."}</span>
  </div>;
}
