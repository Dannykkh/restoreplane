import { beforeEach, describe, expect, it } from "vitest";
import {
  DEFAULT_WORKSPACE_ID,
  activeWorkspaceId,
  scopeServicePayload,
  setActiveWorkspaceId,
} from "./api";

describe("workspace request scope", () => {
  beforeEach(() => {
    window.localStorage.clear();
    setActiveWorkspaceId(DEFAULT_WORKSPACE_ID);
  });

  it("adds the selected workspace to every operational command", () => {
    const selected = "11111111-1111-4111-8111-111111111111";
    setActiveWorkspaceId(selected);

    expect(activeWorkspaceId()).toBe(selected);
    expect(
      scopeServicePayload("connection.list", { cursor: null, limit: 50 }),
    ).toEqual({ workspaceId: selected, cursor: null, limit: 50 });
    expect(scopeServicePayload("backup.enqueue", { planId: "plan-a" })).toEqual({
      workspaceId: selected,
      planId: "plan-a",
    });
    expect(scopeServicePayload("backup_scope.list", { cursor: null, limit: 50 })).toEqual({
      workspaceId: selected,
      cursor: null,
      limit: 50,
    });
    expect(scopeServicePayload("backup_scope.discovery_run", { databaseTargetId: "target-a" })).toEqual({
      workspaceId: selected,
      databaseTargetId: "target-a",
    });
    expect(window.localStorage.getItem("dbbackup.workspaceId")).toBe(selected);
  });

  it("keeps service-wide commands outside workspace scope", () => {
    expect(scopeServicePayload("service.health", {})).toEqual({});
    expect(scopeServicePayload("workspace.list", {})).toEqual({});
    expect(scopeServicePayload("settings.read", {})).toEqual({});
  });
});
