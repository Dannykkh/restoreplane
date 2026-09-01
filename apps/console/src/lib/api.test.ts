import { describe, expect, it } from "vitest";
import {
  enqueueBackup,
  getEngineMatrix,
  getSnapshotDetailPage,
  getServiceHealth,
  listBackupScopeObjects,
  listBackupScopes,
  listSnapshots,
  listWorkspaces,
  normalizeBackupScopeDiscoveryResult,
  normalizeBackupScopeListResult,
  normalizeBackupScopeObjectPageResult,
  normalizeBackupScopeSaveResult,
  normalizeConnectionPageResult,
  normalizeDiscoveryResultPage,
  normalizeTargetPageResult,
  runBackupScopeDiscovery,
  readSettings,
  saveConnection,
  saveBackupScope,
  selectConnectionTargets,
  workCancelPayload,
} from "./api";

describe("manual backup enqueue contract", () => {
  it("returns one durable Backup Work reference per Plan Target", async () => {
    const result = await enqueueBackup("plan-production-hourly");

    expect(result.schemaVersion).toBe(1);
    expect(result.planId).toBe("plan-production-hourly");
    expect(result.workItems).toHaveLength(2);
    expect(result.workItems.map((item) => item.targetName).sort()).toEqual([
      "analytics",
      "orders",
    ]);
    expect(new Set(result.workItems.map((item) => item.workItemId)).size).toBe(2);
  });
});

describe("connection verification truth normalization", () => {
  it("preserves a recovery point with no passed evidence as pending", () => {
    const page = normalizeConnectionPageResult({
      items: [{
        id: "connection-orders",
        displayName: "Orders PostgreSQL",
        engine: "postgresql",
        transport: "tcp",
        endpoint: "orders.internal:5432",
        state: "ready",
        lastProbeStatus: "passed",
        lastProbeAtMs: 100,
        restoreTestTarget: false,
        updatedAtMs: 100,
        targetCount: 1,
        targets: [{
          id: "target-orders",
          connectionId: "connection-orders",
          connectionName: "Orders PostgreSQL",
          engine: "postgresql",
          displayName: "orders",
          state: "active",
          missingSinceMs: null,
          sourceFingerprint: "postgresql:orders",
          latestRecoveryPointId: "recovery-orders",
          latestSuccessAtMs: 90,
          latestFailureAtMs: null,
          artifactCompleteness: "complete",
          replicaDurability: "healthy",
          protectionScope: "whole_database",
          protectionCompliance: "compliant",
          rpoViolation: false,
          verificationState: "pending",
          verificationOverdue: false,
          evidenceAtMs: 90,
        }],
      }],
      nextCursor: null,
      totalCount: 1,
    });

    expect(page.items[0].targets[0].verificationState).toBe("pending");
    expect(page.items[0].targets[0].verificationOverdue).toBe(false);
  });
});

describe("work cancellation contract", () => {
  it("binds cancellation to the current fencing token", () => {
    expect(workCancelPayload("work-backup-orders", 7)).toEqual({
      workItemId: "work-backup-orders",
      fencingToken: 7,
    });
  });

  it("rejects invalid fencing tokens before IPC", () => {
    expect(() => workCancelPayload("work-backup-orders", -1)).toThrow(
      "WORK_FENCING_TOKEN_INVALID",
    );
  });
});

describe("backup scope fixture contract", () => {
  it("lists whole-database and explicit-table scopes", async () => {
    const page = await listBackupScopes();

    expect(page.items).toEqual(expect.arrayContaining([
      expect.objectContaining({ mode: "whole_database" }),
      expect.objectContaining({
        id: "target-finance-warehouse",
        databaseTargetId: "target-finance-warehouse",
        databaseName: "finance_warehouse",
        engine: "sqlserver",
        mode: "whole_database",
      }),
      expect.objectContaining({
        id: "scope-orders-iwrs",
        mode: "explicit_tables",
        newObjectCount: 1,
      }),
    ]));
  });

  it("creates the canonical whole-database scope when a discovered target is selected", async () => {
    const saved = await saveConnection({
      displayName: "Fixture Scope Contract PostgreSQL",
      engine: "postgresql",
      transport: "tcp",
      host: "scope-contract.internal",
      port: 5432,
      filePath: "",
      tlsMode: "verify_full",
      username: "backup_operator",
      password: "fixture-only",
      restoreTestTarget: false,
    });
    const selection = await selectConnectionTargets(
      saved.connectionId,
      "discovery-fixture-scope-contract",
      ["database:scope_contract"],
    );
    const selectedTarget = selection.targets[0];
    const scopes = await listBackupScopes({ cursor: null, limit: 100 });

    expect(selectedTarget).toBeDefined();
    expect(scopes.items).toEqual(expect.arrayContaining([
      expect.objectContaining({
        id: selectedTarget!.id,
        databaseTargetId: selectedTarget!.id,
        databaseName: "scope_contract",
        mode: "whole_database",
      }),
    ]));
  });

  it("discovers paged database objects and saves opaque selectors", async () => {
    const discovery = await runBackupScopeDiscovery("target-orders");
    expect(discovery.state).toBe("queued");
    const page = await listBackupScopeObjects({
      databaseTargetId: "target-orders",
      discoveryRunId: discovery.operationRunId,
      cursor: null,
      limit: 50,
      search: "iwrs",
    });
    const selected = page.items[0];

    expect(selected).toEqual(expect.objectContaining({
      engineObjectId: "opaque:pg:orders/public/iwrs",
      schemaName: "public",
      objectName: "iwrs",
      eligibility: "eligible",
    }));
    const saved = await saveBackupScope({
      databaseTargetId: "target-orders",
      displayName: "IWRS 핵심 테이블",
      mode: "explicit_tables",
      discoveryRunId: discovery.operationRunId,
      engineObjectIds: [selected!.engineObjectId],
    });
    expect(saved).toEqual(expect.objectContaining({
      databaseTargetId: "target-orders",
      mode: "explicit_tables",
      selectedObjectCount: 1,
    }));
  });
});

describe("backup scope service wire adapter", () => {
  it("normalizes the paged service projection without inventing coverage or policy evidence", () => {
    const page = normalizeBackupScopeListResult({
      items: [{
        id: "scope-orders-core",
        databaseTargetId: "target-orders",
        displayName: "orders core",
        mode: "explicit_tables",
        state: "active",
        resolvedObjectCount: 2,
        selectorHashBlake3: "a".repeat(64),
        sourceFingerprint: "source-fingerprint",
      }],
      nextCursor: null,
      totalCount: 1,
    });

    expect(page.nextCursor).toBeNull();
    expect(page.items[0]).toEqual(expect.objectContaining({
      id: "scope-orders-core",
      selectedObjectCount: 2,
      eligibleObjectCount: null,
      newObjectCount: null,
      changedObjectCount: null,
      driftState: "unknown",
      planCount: null,
      retentionPolicyCount: null,
    }));
  });

  it("preserves discovery set-diff and recovery evidence from the service wire", () => {
    const page = normalizeBackupScopeListResult({
      items: [{
      id: "scope-orders-core",
      databaseTargetId: "target-orders",
      databaseName: "orders",
      connectionId: "connection-primary",
      connectionName: "Primary PostgreSQL",
      engine: "postgresql",
      displayName: "orders core",
      mode: "explicit_tables",
      state: "active",
      selectedObjectCount: 2,
      eligibleObjectCount: 3,
      newObjectCount: 1,
      missingObjectCount: 0,
      changedObjectCount: 1,
      driftState: "drifted",
      sourceDrift: false,
      discoveryRunId: "discovery-orders-4",
      inventoryHashBlake3: "a".repeat(64),
      inventoryObservedAtMs: 900,
      latestBackupState: "succeeded",
      latestRecoveryPointId: "recovery-orders-3",
      latestSuccessAtMs: 880,
      latestFailureAtMs: null,
      artifactCompleteness: "complete",
      replicaDurability: "healthy",
      protectionCompliance: "compliant",
      restoreEligibility: "eligible",
      latestVerifiedLevel: 2,
      lastVerifiedAtMs: 890,
      recoveryEvidenceAtMs: 910,
      rpoMs: 3_600_000,
      rpoState: "current",
      verificationRequestedLevel: 2,
      verificationState: "current",
      planCount: 1,
      activePlanCount: 1,
      retentionPolicyCount: 1,
      evidenceAtMs: 910,
      }],
      nextCursor: null,
      totalCount: 1,
    });

    expect(page.items[0]).toEqual(expect.objectContaining({
      databaseName: "orders",
      newObjectCount: 1,
      changedObjectCount: 1,
      driftState: "drifted",
      latestRecoveryPointId: "recovery-orders-3",
      restoreEligibility: "eligible",
      activePlanCount: 1,
      retentionPolicyCount: 1,
    }));
  });

  it("rejects unknown scope modes instead of widening them to whole-database", () => {
    const invalidList = () => normalizeBackupScopeListResult({
      items: [{
        id: "scope-orders-future",
        databaseTargetId: "target-orders",
        mode: "schema_pattern",
        state: "active",
      }],
      nextCursor: null,
      totalCount: 1,
    });
    const invalidSave = () => normalizeBackupScopeSaveResult({
      id: "scope-orders-future",
      databaseTargetId: "target-orders",
      mode: "schema_pattern",
      state: "active",
    });

    for (const normalize of [invalidList, invalidSave]) {
      let rejected: unknown;
      try {
        normalize();
      } catch (error) {
        rejected = error;
      }
      expect(rejected).toMatchObject({ code: "APPLICATION_PAYLOAD_INVALID" });
    }
  });

  it("rejects an unknown scope mode in the preview transport", async () => {
    await expect(saveBackupScope({
      databaseTargetId: "target-orders",
      displayName: "Future scope",
      mode: "schema_pattern" as never,
      discoveryRunId: "scope-discovery-preview",
      engineObjectIds: [],
    })).rejects.toMatchObject({ code: "APPLICATION_PAYLOAD_INVALID" });
  });

  it("normalizes service discovery, object fingerprint, and save identifiers", () => {
    expect(normalizeBackupScopeDiscoveryResult({
      discoveryRunId: "discovery-orders",
      workItemId: "work-discovery-orders",
      state: "queued",
    })).toEqual({
      operationRunId: "discovery-orders",
      workItemId: "work-discovery-orders",
      state: "queued",
    });

    const objects = normalizeBackupScopeObjectPageResult({
      items: [{
        discoveryRunId: "discovery-orders",
        engineObjectId: "opaque:pg:orders/public/iwrs",
        schemaName: "public",
        objectName: "iwrs",
        objectType: "table",
        eligibility: "eligible",
        exclusionReason: null,
        objectFingerprintBlake3: "b".repeat(64),
      }],
      nextCursor: null,
      totalCount: 1,
      eligibleCount: 1,
      discoveryRunId: "discovery-orders",
      inventoryHashBlake3: "c".repeat(64),
      observedAtMs: 100,
    });
    expect(objects.items[0]?.fingerprint).toBe("b".repeat(64));

    expect(normalizeBackupScopeSaveResult({
      id: "scope-orders-core",
      databaseTargetId: "target-orders",
      mode: "explicit_tables",
      state: "active",
      resolvedObjectCount: 1,
      selectorHashBlake3: "d".repeat(64),
      sourceFingerprint: "source-fingerprint",
    })).toEqual(expect.objectContaining({
      scopeId: "scope-orders-core",
      selectedObjectCount: 1,
    }));
  });

  it("preserves the exact object eligibility wire enum and rejects unknown widening", () => {
    const page = (eligibility: string) => ({
      items: [{
        discoveryRunId: "discovery-orders",
        engineObjectId: "opaque:pg:orders/public/orders_view",
        schemaName: "public",
        objectName: "orders_view",
        objectType: "view",
        eligibility,
        exclusionReason: "not a supported base table",
        objectFingerprintBlake3: "e".repeat(64),
      }],
      nextCursor: null,
      totalCount: 1,
      eligibleCount: 0,
      discoveryRunId: "discovery-orders",
      inventoryHashBlake3: "f".repeat(64),
      observedAtMs: 100,
    });

    expect(normalizeBackupScopeObjectPageResult(page("unsupported")).items[0]?.eligibility)
      .toBe("unsupported");
    for (const invalid of ["unknown", "missing"]) {
      let rejected: unknown;
      try {
        normalizeBackupScopeObjectPageResult(page(invalid));
      } catch (error) {
        rejected = error;
      }
      expect(rejected).toMatchObject({ code: "APPLICATION_PAYLOAD_INVALID" });
    }
  });
});

describe("paged service count truth", () => {
  it("rejects missing and invalid totalCount instead of synthesizing item length", () => {
    const normalizers: Array<(value: unknown) => unknown> = [
      normalizeConnectionPageResult,
      normalizeTargetPageResult,
      normalizeDiscoveryResultPage,
      (value) => normalizeBackupScopeListResult(value),
      (value) => normalizeBackupScopeObjectPageResult({
        ...(value as object),
        eligibleCount: 0,
        discoveryRunId: "discovery-1",
        inventoryHashBlake3: "a".repeat(64),
        observedAtMs: 1,
      }),
    ];

    for (const normalize of normalizers) {
      for (const totalCount of [undefined, -1, 1.5, "1"]) {
        let rejected: unknown;
        try {
          normalize({ items: [], nextCursor: null, totalCount });
        } catch (error) {
          rejected = error;
        }
        expect(rejected).toMatchObject({ code: "APPLICATION_PAYLOAD_INVALID" });
      }
    }
  });
});

describe("runtime health fixture contract", () => {
  it("uses durable scheduler and retention evidence fields", async () => {
    const health = await getServiceHealth();

    expect(health.scheduler).toEqual(expect.objectContaining({
      state: "observed",
      configuredPlanCount: 2,
      observedCursorCount: 2,
    }));
    expect(health.retention).toEqual(expect.objectContaining({
      state: "observed",
      configuredPolicyCount: 2,
      activeTombstoneCount: 1,
    }));
  });
});

describe("bounded global and snapshot fixture contracts", () => {
  it("returns summary-only snapshots and bounded selected detail pages", async () => {
    const snapshots = await listSnapshots({
      cursor: null,
      limit: 100,
      scope: "all",
    });
    const selected = snapshots.items.find(
      (snapshot) => snapshot.id === "rp-target-orders-latest",
    );

    expect(selected).toEqual(expect.objectContaining({
      partCount: 2,
      replicaCount: 2,
      verificationEvidenceCount: 1,
      restoreDrillCount: 1,
    }));
    expect(selected).not.toHaveProperty("parts");
    expect(selected).not.toHaveProperty("replicas");
    expect(selected).not.toHaveProperty("verificationEvidence");
    expect(selected).not.toHaveProperty("restoreDrills");

    const first = await getSnapshotDetailPage({
      snapshotId: selected!.id,
      section: "replicas",
      cursor: null,
      limit: 1,
    });
    expect(first).toEqual(expect.objectContaining({
      section: "replicas",
      totalCount: 2,
      complete: false,
    }));
    expect(first.items).toHaveLength(1);
    expect(first.nextCursor).not.toBeNull();
  });

  it("exposes explicit completeness for every bounded global inventory", async () => {
    const [workspaces, matrix, settings] = await Promise.all([
      listWorkspaces(),
      getEngineMatrix(),
      readSettings(),
    ]);
    expect(workspaces.length).toBeGreaterThan(0);
    expect(matrix.complete).toBe(true);
    expect(matrix.certificationTotalCount).toBe(5);
    expect(settings.complete).toBe(true);
    expect(settings.inventoryCounts.tools).toBe(settings.tools.length);
  });
});
