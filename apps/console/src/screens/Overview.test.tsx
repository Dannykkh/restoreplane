import { render, screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { Overview } from "./Overview";

describe("Workspace Overview runtime summary", () => {
  it("shows scheduler, queue, spool, retention, and Backup Scope coverage together", async () => {
    render(<Overview />);

    const runtime = await screen.findByRole("region", {
      name: "프로그램 소유 백업 엔진",
    });
    expect(within(runtime).getByText("Scheduler")).toBeInTheDocument();
    expect(within(runtime).getByText("Queue / permit")).toBeInTheDocument();
    expect(within(runtime).getByText("Spool")).toBeInTheDocument();
    expect(within(runtime).getByText("Retention")).toBeInTheDocument();
    expect(within(runtime).getByText("1 / 4 permits")).toBeInTheDocument();
    expect(within(runtime).getByText("2 / 2 cursors")).toBeInTheDocument();
    expect(within(runtime).getByText("0 queued · 1 tombstones")).toBeInTheDocument();

    const protectionSummary = screen.getByRole("region", { name: "Protection summary" });
    expect(within(protectionSummary).getByText("검증 기한 초과").nextElementSibling)
      .toHaveTextContent("1");
    expect(screen.getByText("Partial coverage").nextElementSibling).toHaveTextContent("1");
    expect(within(protectionSummary).getByText("Verification unknown").nextElementSibling)
      .toHaveTextContent("1");
    expect(screen.getByText("Verification evidence missing")).toBeInTheDocument();

    const scopes = screen.getByRole("region", { name: "Backup Scope coverage" });
    expect(within(scopes).getByText("orders IWRS 선택 백업")).toBeInTheDocument();
    expect(within(scopes).getByText("1 / 3")).toBeInTheDocument();
    expect(within(scopes).getByText("drifted")).toBeInTheDocument();
  });
});
