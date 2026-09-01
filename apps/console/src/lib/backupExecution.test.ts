import { describe, expect, it } from "vitest";
import type { ServiceHealth } from "./types";
import { backupExecutionGate } from "./backupExecution";

const productionHealth: ServiceHealth = {
  bootId: "boot-production",
  phase: "production_ready",
  schemaVersion: 4,
  applicationTableCount: 67,
  executionAuthority: "service",
  cutoverMode: "production",
  cutoverMarker: "cutover-2026-08-13",
  recoveryDisposition: "forward_fix_required",
  claimsEnabled: true,
  acceptsIpc: true,
  diagnosticCode: null,
  scheduler: {
    state: "observed",
    configuredPlanCount: 1,
    observedCursorCount: 1,
    activeWorkCount: 0,
    errorPlanCount: 0,
    lastTickAtMs: 1,
    lastMaterializedForMs: 1,
    lastErrorCode: null,
    lastErrorAtMs: null,
    evidenceAtMs: 1,
  },
  retention: {
    state: "observed",
    configuredPolicyCount: 1,
    activeTombstoneCount: 0,
    queuedDeleteCount: 0,
    failedOperationCount: 0,
    lastScanAtMs: 1,
    lastErrorCode: null,
    lastErrorAtMs: null,
    evidenceAtMs: 1,
  },
};

describe("backup execution gate", () => {
  it("allows browser fixture execution only as an explicitly labelled demo", () => {
    expect(
      backupExecutionGate(
        { ...productionHealth, phase: "rehearsal_ready", claimsEnabled: false },
        "browser_fixture",
      ),
    ).toEqual({ allowed: true, reason: "브라우저 검증 데이터로 실행 흐름을 시연합니다." });
  });

  it("requires production service authority and claims for real execution", () => {
    expect(backupExecutionGate(productionHealth, "service").allowed).toBe(true);
    expect(
      backupExecutionGate(
        { ...productionHealth, executionAuthority: "legacy_direct" },
        "service",
      ),
    ).toEqual({
      allowed: false,
      reason: "서비스 실행 권한 전환이 완료되지 않았습니다.",
    });
  });
});
