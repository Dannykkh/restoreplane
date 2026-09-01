import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { ActivityEvent } from "../lib/types";
import { VirtualActivityList } from "./VirtualActivityList";

describe("VirtualActivityList", () => {
  it("represents 100,000 events while keeping the rendered DOM bounded", () => {
    const items: ActivityEvent[] = Array.from({ length: 50 }, (_, index) => ({
      seq: 100_000 - index,
      id: `event-${index}`,
      occurredAtMs: 1_700_000_000_000 - index,
      severity: index === 0 ? "error" : "info",
      eventCode: "BACKUP_PHASE",
      phase: "queued",
      entityType: "work_item",
      entityId: `work-${index}`,
      correlationId: null,
      workItemId: `work-${index}`,
      attemptId: null,
      retryability: index === 0 ? "retryable" : null,
      message: `phase ${index}`,
    }));
    render(
      <VirtualActivityList
        items={items}
        totalCount={100_000}
        selectedId={null}
        onSelect={() => undefined}
      />,
    );

    const list = screen.getByRole("list", { name: "Activity events" });
    expect(list).toHaveAttribute("aria-setsize", "100000");
    expect(screen.getAllByRole("listitem")).toHaveLength(50);
    expect(list.querySelectorAll("*").length).toBeLessThan(600);
  });
});
