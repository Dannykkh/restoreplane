import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { BackupPlans } from "./BackupPlans";

describe("BackupPlans operations", () => {
  it("loads persisted Backup Scopes into the Plan wizard with exact coverage", async () => {
    const user = userEvent.setup();
    render(<BackupPlans route={{ page: "backup-plans" }} />);

    await user.click(await screen.findByRole("button", { name: "새 Backup Plan" }));

    expect(
      await screen.findByText("orders IWRS 선택 백업"),
    ).toBeInTheDocument();
    expect(screen.getByText("1 / 3")).toBeInTheDocument();
    expect(screen.getByText("새 테이블 1개 미보호")).toBeInTheDocument();
  });

  it("tracks and cancels each Target work returned by a multi-database Plan", async () => {
    const user = userEvent.setup();
    render(<BackupPlans route={{ page: "backup-plans" }} />);

    expect(await screen.findByText("매시간 15분")).toBeInTheDocument();

    const runNow = screen.getByRole("button", { name: "지금 실행" });
    expect(runNow).toBeEnabled();
    await user.click(runNow);

    expect(
      await screen.findByRole("heading", { name: "실행 중인 백업" }),
    ).toBeInTheDocument();
    expect(screen.getByText("orders · postgresql")).toBeInTheDocument();
    expect(screen.getByText("analytics · mysql")).toBeInTheDocument();
    expect(screen.getByText("public.orders")).toBeInTheDocument();
    expect(
      screen.getByRole("progressbar", { name: "Production hourly orders 진행률" }),
    ).toHaveAttribute("aria-valuenow", "38");
    expect(
      screen.getByRole("progressbar", { name: "Production hourly analytics 단계 진행" }),
    ).not.toHaveAttribute("aria-valuenow");

    await user.click(
      screen.getByRole("button", { name: "Production hourly orders 취소" }),
    );
    expect(await screen.findByText("cancelled")).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "Production hourly orders 취소" }),
    ).not.toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Production hourly analytics 취소" }),
    ).toBeEnabled();
  });
});
