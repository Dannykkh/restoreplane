import { createElement } from "react";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type {
  DatabaseTarget,
  Destination,
  BackupScope,
  EngineCapability,
  EngineMatrix,
  EngineMatrixRow,
} from "../lib/types";
import { BackupPlanWizard, engineCapabilitiesFromMatrix } from "./BackupPlanWizard";

const apiMocks = vi.hoisted(() => ({
  savePlan: vi.fn(),
  publishPlan: vi.fn(),
}));

vi.mock("../lib/api", () => apiMocks);

const capabilityNames: EngineCapability["capability"][] = [
  "probe",
  "discovery",
  "tool_preflight",
  "backup",
  "verify_l1",
  "verify_l2",
  "restore_create_new",
  "validation",
  "restore_l3",
];

function matrixRow(
  engine: string,
  supportStatus: EngineMatrixRow["supportStatus"],
  capabilityStatus: EngineCapability["supportStatus"],
): EngineMatrixRow {
  return {
    certificationId: `${engine}-current`,
    engine,
    serverVersionRange: null,
    toolName: null,
    toolVersionRange: null,
    operatingSystem: "windows",
    architecture: "x86_64",
    dumpFormat: null,
    consistencyGrade: null,
    supportStatus,
    evidenceLocator: `artifact://${engine}`,
    evidenceChecksumBlake3: null,
    certifiedAtMs: 1,
    expiresAtMs: 2,
    capabilities: capabilityNames.map((capability) => ({
      capability,
      supportStatus: capabilityStatus,
      limitation: null,
      evidenceLocator: `artifact://${engine}/${capability}`,
    })),
  };
}

function matrix(...rows: EngineMatrixRow[]): EngineMatrix {
  return {
    generatedAtMs: 1,
    certificationTotalCount: rows.length,
    complete: true,
    rows,
  };
}

const target: DatabaseTarget = {
  id: "target-orders",
  connectionId: "connection-pg",
  connectionName: "Production PostgreSQL",
  engine: "postgresql",
  displayName: "orders",
  state: "active",
  missingSinceMs: null,
  sourceFingerprint: "postgresql:orders",
  latestRecoveryPointId: null,
  latestSuccessAtMs: null,
  latestFailureAtMs: null,
  artifactCompleteness: "unknown",
  replicaDurability: "unknown",
  protectionScope: "none",
  protectionCompliance: "not_configured",
  rpoViolation: false,
  verificationState: "not_configured",
  verificationOverdue: false,
  evidenceAtMs: null,
};

const destination: Destination = {
  id: "destination-vault",
  displayName: "Recovery Vault",
  kind: "local",
  endpoint: "D:\\Backup",
  state: "ready",
  immutabilityCapability: "none",
  lastProbeStatus: "passed",
  lastProbeAtMs: 1,
  updatedAtMs: 1,
  roles: [
    { role: "backup_writer", credentialBound: true, probeStatus: "passed", probedAtMs: 1 },
    { role: "restore_reader", credentialBound: true, probeStatus: "passed", probedAtMs: 1 },
  ],
};

const explicitScope: BackupScope = {
  id: "scope-orders-iwrs",
  databaseTargetId: target.id,
  databaseName: target.displayName,
  connectionId: target.connectionId,
  connectionName: target.connectionName,
  engine: target.engine,
  displayName: "orders IWRS 선택 백업",
  mode: "explicit_tables",
  state: "active",
  selectedObjectCount: 1,
  eligibleObjectCount: 2,
  newObjectCount: 0,
  missingObjectCount: 0,
  changedObjectCount: 0,
  driftState: "current",
  sourceDrift: false,
  discoveryRunId: "discovery-orders",
  inventoryHashBlake3: "a".repeat(64),
  inventoryObservedAtMs: 1,
  latestBackupState: null,
  latestRecoveryPointId: null,
  latestSuccessAtMs: null,
  latestFailureAtMs: null,
  artifactCompleteness: "unknown",
  replicaDurability: "unknown",
  protectionCompliance: "not_configured",
  restoreEligibility: "unknown",
  latestVerifiedLevel: null,
  lastVerifiedAtMs: null,
  recoveryEvidenceAtMs: null,
  rpoMs: 7_200_000,
  rpoState: "unknown",
  rpoViolation: false,
  verificationRequestedLevel: 2,
  verificationState: "unknown",
  verificationOverdue: false,
  planCount: 0,
  activePlanCount: 0,
  retentionPolicyCount: 0,
  evidenceAtMs: 1,
};

describe("Engine Matrix plan preflight", () => {
  it("blocks a SQL Server candidate even when tool preflight passed", () => {
    const row = matrixRow("sqlserver", "candidate", "not_run");
    row.capabilities.find(
      (capability) => capability.capability === "tool_preflight",
    )!.supportStatus = "passed";

    expect(engineCapabilitiesFromMatrix("sqlserver", 2, matrix(row))).toEqual([
      expect.objectContaining({ status: "experimental" }),
      expect.objectContaining({ status: "experimental" }),
    ]);
  });

  it("allows certified SQLite backup and the requested restore drill", () => {
    const result = engineCapabilitiesFromMatrix(
      "sqlite",
      3,
      matrix(matrixRow("sqlite", "supported", "passed")),
    );

    expect(result.map((capability) => capability.status)).toEqual([
      "supported",
      "supported",
    ]);
    expect(result[1]?.evidence).toBe("artifact://sqlite/restore_l3");
  });

  it("maps explicit-table preflight to the persisted discovery and backup capabilities", () => {
    const row = matrixRow("postgresql", "supported", "passed");

    const wholeDatabase = engineCapabilitiesFromMatrix(
      "postgresql",
      2,
      matrix(row),
      "whole_database",
    );
    const explicitTables = engineCapabilitiesFromMatrix(
      "postgresql",
      2,
      matrix(row),
      "explicit_tables",
    );

    expect(wholeDatabase.map((item) => item.capability)).toEqual([
      "postgresql backup",
      "postgresql verify L2",
    ]);
    expect(explicitTables.map((item) => item.capability)).toEqual([
      "postgresql object discovery",
      "postgresql selective backup",
      "postgresql verify L2",
    ]);
    expect(explicitTables.map((item) => item.status)).toEqual([
      "supported",
      "supported",
      "supported",
    ]);
    expect(explicitTables[0]?.evidence).toBe("artifact://postgresql/discovery");
    expect(explicitTables[1]?.evidence).toBe("artifact://postgresql/backup");
  });

  it("treats an absent engine row as unknown instead of assuming support", () => {
    expect(engineCapabilitiesFromMatrix("oracle", 1, matrix())).toEqual([
      expect.objectContaining({ status: "unknown" }),
      expect.objectContaining({ status: "unknown" }),
    ]);
  });
});

describe("Backup Plan scope contract", () => {
  it("does not fabricate a whole-database scope when persisted scope evidence is missing", async () => {
    const user = userEvent.setup();
    apiMocks.savePlan.mockReset();
    apiMocks.publishPlan.mockReset();

    render(createElement(BackupPlanWizard, {
      targets: [target],
      destinations: [destination],
      engineMatrix: matrix(matrixRow("postgresql", "supported", "passed")),
      onCancel: vi.fn(),
      onComplete: vi.fn(),
    }));

    await user.type(screen.getByRole("textbox", { name: "Plan 이름" }), "Unknown scope");

    expect(screen.getByText("선택할 Backup Scope가 없습니다.")).toBeInTheDocument();
    expect(screen.queryByRole("checkbox", { name: /orders/ })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: /다음:/ })).toBeDisabled();
    expect(apiMocks.savePlan).not.toHaveBeenCalled();
  });

  it("publishes scope IDs with a daily, weekly, monthly retention policy", async () => {
    const user = userEvent.setup();
    apiMocks.savePlan.mockResolvedValue({ planId: "plan-1", revisionId: "revision-1" });
    apiMocks.publishPlan.mockResolvedValue({
      planId: "plan-1",
      revisionId: "revision-1",
      state: "enabled",
      publishedAtMs: 1,
    });

    render(createElement(BackupPlanWizard, {
      targets: [target],
      scopes: [explicitScope],
      destinations: [destination],
      engineMatrix: matrix(matrixRow("postgresql", "supported", "passed")),
      onCancel: vi.fn(),
      onComplete: vi.fn(),
    }));

    await user.type(screen.getByRole("textbox", { name: "Plan 이름" }), "Orders hourly");
    expect(screen.getByText("1 / 2")).toBeInTheDocument();
    await user.click(screen.getByRole("checkbox", { name: /orders IWRS 선택 백업/ }));
    await user.click(screen.getByRole("button", { name: /다음:/ }));
    await user.click(screen.getByRole("checkbox", { name: /Recovery Vault/ }));
    await user.click(screen.getByRole("button", { name: /다음:/ }));
    await user.click(screen.getByRole("button", { name: /다음:/ }));
    await user.click(screen.getByRole("button", { name: /다음:/ }));
    await user.click(screen.getByRole("button", { name: "검증 후 Plan 게시" }));

    await waitFor(() => expect(apiMocks.savePlan).toHaveBeenCalledTimes(1));
    expect(apiMocks.savePlan).toHaveBeenCalledWith(expect.objectContaining({
      scopeIds: ["scope-orders-iwrs"],
      retention: { daily: 30, weekly: 8, monthly: 12 },
    }), expect.any(String));
    expect(apiMocks.savePlan.mock.calls[0]?.[0]).not.toHaveProperty("targetIds");
    expect(apiMocks.savePlan.mock.calls[0]?.[0]).not.toHaveProperty("keepDays");
  });

  it("does not repeat plan.save when publish has an ambiguous transport response", async () => {
    const user = userEvent.setup();
    apiMocks.savePlan.mockReset().mockResolvedValue({
      planId: "plan-transport",
      revisionId: "revision-transport",
    });
    apiMocks.publishPlan.mockReset()
      .mockRejectedValueOnce(new Error("transport unavailable"))
      .mockResolvedValue({
        planId: "plan-transport",
        revisionId: "revision-transport",
        state: "enabled",
        publishedAtMs: 1,
      });

    render(createElement(BackupPlanWizard, {
      targets: [target],
      scopes: [explicitScope],
      destinations: [destination],
      engineMatrix: matrix(matrixRow("postgresql", "supported", "passed")),
      onCancel: vi.fn(),
      onComplete: vi.fn(),
    }));

    await user.type(screen.getByRole("textbox", { name: "Plan 이름" }), "Orders retry");
    await user.click(screen.getByRole("checkbox", { name: /orders IWRS 선택 백업/ }));
    await user.click(screen.getByRole("button", { name: /다음:/ }));
    await user.click(screen.getByRole("checkbox", { name: /Recovery Vault/ }));
    await user.click(screen.getByRole("button", { name: /다음:/ }));
    await user.click(screen.getByRole("button", { name: /다음:/ }));
    await user.click(screen.getByRole("button", { name: /다음:/ }));

    const publish = screen.getByRole("button", { name: "검증 후 Plan 게시" });
    await user.click(publish);
    await waitFor(() => expect(apiMocks.publishPlan).toHaveBeenCalledTimes(1));
    await user.click(publish);
    await waitFor(() => expect(apiMocks.publishPlan).toHaveBeenCalledTimes(2));

    expect(apiMocks.savePlan).toHaveBeenCalledTimes(1);
    expect(apiMocks.publishPlan.mock.calls[1]?.[2]).toBe(
      apiMocks.publishPlan.mock.calls[0]?.[2],
    );
  });
});
