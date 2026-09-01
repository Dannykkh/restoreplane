import { describe, expect, it } from "vitest";
import type { Workspace } from "../lib/types";
import { workspaceOperationalState } from "./Workspaces";

function workspace(overrides: Partial<Workspace> = {}): Workspace {
  return {
    id: "11111111-1111-4111-8111-111111111111",
    displayName: "Finance Operations",
    state: "active",
    isDefault: false,
    createdAtMs: 1,
    updatedAtMs: 1,
    connectionCount: 2,
    databaseTargetCount: 8,
    destinationCount: 2,
    planCount: 3,
    activeWorkCount: 0,
    latestActivityAtMs: 1_000,
    ...overrides,
  };
}

describe("workspace fleet status", () => {
  it("prioritizes running work without claiming backup health", () => {
    expect(workspaceOperationalState(workspace({ activeWorkCount: 2 }))).toEqual({
      key: "running",
      label: "실행 중",
      detail: "2개 작업이 처리 중입니다.",
      tone: "info",
    });
  });

  it("identifies the first missing protection boundary", () => {
    expect(
      workspaceOperationalState(workspace({ databaseTargetCount: 0 })),
    ).toMatchObject({ key: "needs_setup", label: "DB 등록 필요" });
    expect(
      workspaceOperationalState(workspace({ destinationCount: 0 })),
    ).toMatchObject({ key: "needs_setup", label: "목적지 필요" });
    expect(
      workspaceOperationalState(workspace({ planCount: 0 })),
    ).toMatchObject({ key: "needs_setup", label: "Plan 필요" });
  });

  it("calls a fully wired workspace configured rather than healthy", () => {
    expect(workspaceOperationalState(workspace())).toMatchObject({
      key: "configured",
      label: "구성됨",
    });
  });
});
