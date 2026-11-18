import { describe, expect, it } from "vitest";
import { NAVIGATION, parseRoute, serializeRoute } from "./route";

describe("route state", () => {
  it("starts at the all-workspaces dashboard", () => {
    expect(parseRoute("")).toEqual({ page: "workspaces" });
    expect(parseRoute("#/")).toEqual({ page: "workspaces" });
  });

  it("adds Workbench while preserving the existing navigation order", () => {
    expect(NAVIGATION.map((item) => item.label)).toEqual([
      "Overview",
      "Connections",
      "Databases",
      "데이터베이스 작업",
      "Destinations",
      "Backup Plans",
      "Activity",
      "Snapshots",
      "Recovery",
      "Settings",
    ]);
  });

  it("round-trips filter, cursor, and selection without leaking unrelated state", () => {
    const serialized = serializeRoute({
      page: "activity",
      filter: "failed",
      cursor: "1722200000000:42",
      selected: "work-42",
    });
    expect(parseRoute(serialized)).toEqual({
      page: "activity",
      filter: "failed",
      cursor: "1722200000000:42",
      selected: "work-42",
    });
    expect(serialized).not.toContain("undefined");
  });

  it("round-trips plan, database target, and backup scope selections independently", () => {
    const serialized = serializeRoute({
      page: "backup-plans",
      selected: "plan-hourly",
      target: "target-orders",
      scope: "scope-orders-iwrs",
      action: "new",
    });

    expect(parseRoute(serialized)).toEqual({
      page: "backup-plans",
      selected: "plan-hourly",
      target: "target-orders",
      scope: "scope-orders-iwrs",
      action: "new",
    });
  });

  it("redirects legacy routes into the v2 information architecture", () => {
    expect(parseRoute("#/jobs").page).toBe("backup-plans");
    expect(parseRoute("#/schedule").page).toBe("backup-plans");
    expect(parseRoute("#/restore").page).toBe("recovery");
  });
});
