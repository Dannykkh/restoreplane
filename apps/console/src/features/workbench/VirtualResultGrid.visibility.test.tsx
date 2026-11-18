// Staged candidate only. Final path: src/features/workbench/VirtualResultGrid.visibility.test.tsx.
// NOT TYPECHECKED / NOT RUN. JSDOM scroll properties are not real viewport/zoom proof.
import { fireEvent, render, screen } from "@testing-library/react";
import { useState } from "react";
import { describe, expect, it } from "vitest";
import { VirtualResultGrid, type SelectionAnchor } from "./VirtualResultGrid";
import type { Column, ResultRow } from "../../lib/workbenchTypes";

const columns: Column[] = Array.from({ length: 20 }, (_, index) => ({ columnId: `column-${index}`, name: `열${index}`, typeName: "text", nullable: false, editable: false, readonlyReason: "읽기 전용" }));
const rows: ResultRow[] = Array.from({ length: 160 }, (_, row) => ({ rowHandle: `row-${row}`, cells: columns.map((_, column) => ({ kind: "text", value: `r${row}-c${column}` })) }));
function Harness() {
  const [visible, setVisible] = useState(true);
  const [selection, setSelection] = useState<SelectionAnchor | null>(null);
  return <><button onClick={() => setVisible((value) => !value)}>{visible ? "결과 숨기기" : "결과 다시 보기"}</button><output aria-label="선택 위치">{selection ? `${selection.rowHandle}/${selection.columnId}` : "선택 없음"}</output><VirtualResultGrid resultSetId="result-mock" rows={rows} columns={columns} selection={selection} onSelect={setSelection} visible={visible} /></>;
}

describe("grid visibility without remounting its owner (component fixture)", () => {
  it("removes hidden grid DOM and restores selection plus both scroll axes on return", () => {
    render(<Harness />); const original = screen.getByRole("grid");
    fireEvent.scroll(original, { target: { scrollTop: 3200, scrollLeft: 1840 } });
    fireEvent.click(screen.getByRole("gridcell", { name: "열10: r100-c10" }));
    const selected = original.getAttribute("aria-activedescendant"); expect(selected).toBeTruthy();
    expect(screen.getByLabelText("선택 위치")).toHaveTextContent("row-100/column-10");
    fireEvent.click(screen.getByRole("button", { name: "결과 숨기기" }));
    expect(screen.queryByRole("grid", { hidden: true })).not.toBeInTheDocument();
    expect(screen.queryAllByRole("gridcell", { hidden: true })).toHaveLength(0);
    expect(screen.getByLabelText("선택 위치")).toHaveTextContent("row-100/column-10");
    fireEvent.click(screen.getByRole("button", { name: "결과 다시 보기" }));
    const restored = screen.getByRole("grid"); expect(restored).not.toBe(original);
    expect(restored.scrollTop).toBe(3200); expect(restored.scrollLeft).toBe(1840);
    expect(restored).toHaveAttribute("aria-activedescendant", selected);
    expect(screen.getByRole("gridcell", { name: "열10: r100-c10" })).toHaveAttribute("aria-selected", "true");
    restored.focus(); fireEvent.keyDown(restored, { key: "ArrowRight" });
    expect(screen.getByLabelText("선택 위치")).toHaveTextContent("row-100/column-11");
  });

  it("allows navigation after hide/show even if the removed DOM never emitted compositionend", () => {
    render(<Harness />); const grid = screen.getByRole("grid");
    fireEvent.click(screen.getByRole("gridcell", { name: "열0: r0-c0" }));
    fireEvent.compositionStart(grid); fireEvent.keyDown(grid, { key: "ArrowDown", isComposing: true });
    expect(screen.getByLabelText("선택 위치")).toHaveTextContent("row-0/column-0");
    fireEvent.click(screen.getByRole("button", { name: "결과 숨기기" }));
    fireEvent.click(screen.getByRole("button", { name: "결과 다시 보기" }));
    fireEvent.keyDown(screen.getByRole("grid"), { key: "ArrowDown" });
    expect(screen.getByLabelText("선택 위치")).toHaveTextContent("row-1/column-0");
  });
});
