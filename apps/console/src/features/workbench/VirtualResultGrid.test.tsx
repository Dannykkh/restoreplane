import { fireEvent, render, screen, within } from "@testing-library/react";
import { useState } from "react";
import { describe, expect, it } from "vitest";
import { VirtualResultGrid, type SelectionAnchor } from "./VirtualResultGrid";
import { RowInspector } from "./RowInspector";
import { WorkbenchClient } from "../../lib/workbench";
import type { Column, ResultRow } from "../../lib/workbenchTypes";

const columns: Column[] = Array.from({ length: 100 }, (_, index) => ({ columnId: `column-${index}`, name: `열${index}`, typeName: "decimal", nullable: true, editable: false, readonlyReason: "읽기 전용" }));
const rows: ResultRow[] = Array.from({ length: 2000 }, (_, index) => ({ rowHandle: `row-${index}`, cells: columns.map(() => ({ kind: "decimal", value: "12345678901234567890.000000001" })) }));
function GridMockHarness() {
  const [selection, setSelection] = useState<SelectionAnchor | null>({ resultSetId: "result-mock", rowHandle: "row-0", columnId: "column-0" });
  return <VirtualResultGrid resultSetId="result-mock" rows={rows} columns={columns} selection={selection} onSelect={setSelection} />;
}
describe("row and column virtual grid, component fixture only", () => {
  it("bounds mounted rows/columns and preserves focus while the selected cell is evicted from DOM", () => {
    render(<GridMockHarness />);
    const grid = screen.getByRole("grid"); grid.focus();
    expect(screen.getAllByRole("gridcell").length).toBeLessThan(400);
    expect(screen.getAllByRole("columnheader").length).toBeLessThan(20);
    const initial = grid.getAttribute("aria-activedescendant"); expect(initial).toBeTruthy();
    fireEvent.scroll(grid, { target: { scrollTop: 32_000, scrollLeft: 9_200 } });
    expect(document.activeElement).toBe(grid); expect(grid).not.toHaveAttribute("aria-activedescendant");
    expect(document.getElementById(initial!)).toBeNull();
    fireEvent.keyDown(grid, { key: "ArrowDown" });
    expect(grid.getAttribute("aria-activedescendant")).toBeTruthy();
    expect(document.getElementById(grid.getAttribute("aria-activedescendant")!)).not.toBeNull();
    expect(document.activeElement).toBe(grid);
  });
  it("keeps exact numeric text and ignores Korean composition Enter", () => {
    render(<GridMockHarness />); const grid = screen.getByRole("grid"); grid.focus();
    const active = grid.getAttribute("aria-activedescendant");
    fireEvent.compositionStart(grid); fireEvent.keyDown(grid, { key: "Enter", isComposing: true }); fireEvent.compositionEnd(grid);
    expect(grid.getAttribute("aria-activedescendant")).toBe(active);
    expect(within(grid).getAllByText("12345678901234567890.000000001").length).toBeGreaterThan(0);
  });
  it("provides a readonly row alternative with unsupported/truncated reasons and distinct NULL/empty", () => {
    const localColumns = columns.slice(0, 4);
    const row: ResultRow = { rowHandle: "row-mock", cells: [{ kind: "null" }, { kind: "text", value: "" }, { kind: "unsupported", typeName: "geography", reason: "codec_not_certified" }, { kind: "large", valueHandle: "large-mock", typeName: "blob", byteLength: "9007199254740993" as never, preview: "잘린 값" }] };
    render(<RowInspector row={row} columns={localColumns} selection={{ resultSetId: "result", rowHandle: row.rowHandle, columnId: "column-3" }} identity={null} client={new WorkbenchClient({ available: false, invoke: async () => undefined })} />);
    expect(screen.getByText("NULL")).toBeVisible(); expect(screen.getByText("빈 문자열")).toBeVisible();
    expect(screen.getByText("이 값 형식의 정확한 변환이 검증되지 않았습니다.")).toBeVisible();
    expect(screen.getByText("잘린 미리보기는 편집할 수 없습니다.")).toBeVisible();
    expect(screen.getByRole("button", { name: "앞 64KiB 읽기" })).toBeDisabled();
    expect(screen.queryByRole("textbox")).not.toBeInTheDocument();
  });
});
