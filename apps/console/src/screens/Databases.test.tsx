import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { Databases } from "./Databases";

describe("Databases Backup Scope integration", () => {
  it("shows persisted coverage and opens the scope editor for the selected database", async () => {
    const user = userEvent.setup();
    render(<Databases route={{ page: "databases", selected: "target-orders" }} />);

    expect(
      await screen.findByRole("heading", { name: "Backup Scopes" }),
    ).toBeInTheDocument();
    expect(screen.getByText("orders 전체 백업")).toBeInTheDocument();
    expect(screen.getByText("orders IWRS 선택 백업")).toBeInTheDocument();
    expect(screen.getByText("1 / 3")).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "Scope recovery evidence" })).toBeInTheDocument();
    expect(screen.getByText(/Drift drifted/)).toBeInTheDocument();
    expect(screen.getAllByText("whole_database").length).toBeGreaterThan(0);
    expect(screen.getAllByText("pending").length).toBeGreaterThan(0);

    await user.click(screen.getByRole("button", { name: "새 선택 범위" }));
    expect(
      await screen.findByRole("heading", { name: "orders Backup Scope" }),
    ).toBeInTheDocument();
    expect(screen.getByRole("radio", { name: "전체 데이터베이스" })).toBeChecked();
  });
});
