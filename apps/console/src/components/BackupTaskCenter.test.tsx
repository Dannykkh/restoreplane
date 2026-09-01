import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { WorkItemStatus } from "../lib/types";
import { BackupTaskCenter } from "./BackupTaskCenter";

function work(overrides: Partial<WorkItemStatus> = {}): WorkItemStatus {
  return {
    workItemId: "work-backup-orders",
    state: "running",
    activityAfterSeq: 41,
    kind: "backup",
    currentAttemptNo: 2,
    fencingToken: 7,
    waitReason: null,
    operationRunId: null,
    operationKind: null,
    result: null,
    errorCode: null,
    discoveredCount: 0,
    eligibleDiscoveredCount: 0,
    discovered: [],
    subject: {
      planId: "plan-hourly",
      planName: "Production hourly",
      targetId: "target-orders",
      targetName: "orders",
      engine: "postgresql",
    },
    progress: {
      phase: "dumping",
      completedUnits: 384,
      totalUnits: 1_024,
      unit: "mib",
      currentObject: "public.orders",
      updatedAtMs: 1_720_000_000_000,
    },
    createdAtMs: 1_719_999_990_000,
    updatedAtMs: 1_720_000_000_000,
    ...overrides,
  };
}

describe("BackupTaskCenter", () => {
  it("shows measured progress, current object and the durable work identity", () => {
    render(<BackupTaskCenter items={[work()]} onCancel={vi.fn()} />);

    expect(screen.getByRole("heading", { name: "실행 중인 백업" })).toBeInTheDocument();
    expect(screen.getByText("Production hourly")).toBeInTheDocument();
    expect(screen.getByText(/orders · postgresql/)).toBeInTheDocument();
    expect(screen.getByText("public.orders")).toBeInTheDocument();
    expect(screen.getByRole("progressbar", { name: "Production hourly orders 진행률" })).toHaveAttribute(
      "aria-valuenow",
      "38",
    );
    expect(screen.getByText("384 / 1,024 MiB · 38%")).toBeInTheDocument();
    expect(screen.getByText("work-backup-orders")).toBeInTheDocument();
  });

  it("uses an indeterminate stage when the service has no measurable total", () => {
    render(
      <BackupTaskCenter
        items={[
          work({
            progress: {
              phase: "replicating",
              completedUnits: null,
              totalUnits: null,
              unit: "objects",
              currentObject: "s3-primary",
              updatedAtMs: 1_720_000_000_000,
            },
          }),
        ]}
        onCancel={vi.fn()}
      />,
    );

    const progress = screen.getByRole("progressbar", {
      name: "Production hourly orders 단계 진행",
    });
    expect(progress).not.toHaveAttribute("aria-valuenow");
    expect(screen.getByText("총량을 측정하지 않는 단계입니다.")).toBeInTheDocument();
  });

  it("passes the current fencing token when cancellation is requested", async () => {
    const user = userEvent.setup();
    const onCancel = vi.fn();
    render(<BackupTaskCenter items={[work()]} onCancel={onCancel} />);

    await user.click(screen.getByRole("button", { name: "Production hourly orders 취소" }));

    expect(onCancel).toHaveBeenCalledWith("work-backup-orders", 7);
  });

  it("does not offer cancellation for terminal work", () => {
    render(
      <BackupTaskCenter
        items={[work({ state: "succeeded", progress: null })]}
        onCancel={vi.fn()}
      />,
    );

    expect(screen.queryByRole("button", { name: /취소/ })).not.toBeInTheDocument();
  });

  it("locks cancellation after the recovery point commit boundary", () => {
    render(
      <BackupTaskCenter
        items={[work({ state: "committing", progress: null })]}
        onCancel={vi.fn()}
      />,
    );

    expect(screen.queryByRole("button", { name: /취소/ })).not.toBeInTheDocument();
    expect(screen.getByText("커밋이 시작되어 취소할 수 없습니다.")).toBeInTheDocument();
  });
});
