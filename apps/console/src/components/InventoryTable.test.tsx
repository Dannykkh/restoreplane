import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { InventoryTable } from "./InventoryTable";

const rows = [
  { id: "one", name: "주문 데이터베이스", state: "compliant" },
  { id: "two", name: "긴 이름의 고객 분석 데이터베이스", state: "non_compliant" },
];

describe("InventoryTable", () => {
  it("exposes table semantics and keyboard row navigation", async () => {
    const user = userEvent.setup();
    const onSelect = vi.fn();
    render(
      <InventoryTable
        ariaLabel="데이터베이스 인벤토리"
        columns={[
          { key: "name", header: "Database", render: (row) => row.name },
          { key: "state", header: "Protection", render: (row) => row.state },
        ]}
        rows={rows}
        rowKey={(row) => row.id}
        selectedId="one"
        onSelect={onSelect}
        emptyMessage="등록된 데이터베이스가 없습니다."
      />,
    );

    const table = screen.getByRole("table", { name: "데이터베이스 인벤토리" });
    expect(table).toBeInTheDocument();
    expect(screen.getAllByRole("columnheader")).toHaveLength(2);
    const dataRows = screen.getAllByRole("row").slice(1);
    dataRows[0].focus();
    await user.keyboard("{ArrowDown}");
    expect(onSelect).toHaveBeenCalledWith(rows[1]);
    expect(dataRows[1]).toHaveFocus();
  });
});
