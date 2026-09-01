import { describe, expect, it } from "vitest";
import {
  isDiagnosticSnapshot,
  recoveryInventoryAllowsMutation,
  restoreRequestFromPreflight,
  targetConnectionsFor,
} from "./recovery";
import type {
  ConnectionProfile,
  RestorePreflightResult,
  Snapshot,
} from "./types";

describe("recovery selectors", () => {
  it("builds the exact safe restore payload only from authoritative preflight", () => {
    const preflight = {
      schemaVersion: 1,
      preflightVersion: "a".repeat(64),
      eligible: true,
      mode: "create_new",
      purpose: "restore_drill",
      source: {
        snapshotId: "snapshot-1",
        replicaId: "replica-1",
        displayName: "orders",
        engine: "postgresql",
        restoreEligibility: "eligible",
      },
      targetConnectionId: "connection-lab",
      targetName: "orders_drill",
    } as RestorePreflightResult;

    expect(restoreRequestFromPreflight(preflight, 3)).toEqual({
      snapshotId: "snapshot-1",
      sourceReplicaId: "replica-1",
      targetConnectionId: "connection-lab",
      targetName: "orders_drill",
      preflightVersion: "a".repeat(64),
      requiredVerificationLevel: 3,
      purpose: "restore_drill",
    });
  });

  it("allows recovery mutations only with exact snapshot and replica inventories", () => {
    const snapshots = {
      items: [{ id: "snapshot-1" }],
      nextCursor: null,
      totalCount: 1,
      complete: true,
    };
    const replicas = {
      items: [{ id: "replica-1" }],
      nextCursor: null,
      totalCount: 1,
      complete: true,
    };

    expect(recoveryInventoryAllowsMutation(snapshots, replicas)).toBe(true);
    expect(
      recoveryInventoryAllowsMutation(
        { ...snapshots, nextCursor: "snapshot-2", complete: false },
        replicas,
      ),
    ).toBe(false);
    expect(
      recoveryInventoryAllowsMutation(snapshots, {
        ...replicas,
        totalCount: 2,
      }),
    ).toBe(false);
    expect(
      recoveryInventoryAllowsMutation(snapshots, replicas, {
        items: [{ id: "connection-1" }],
        nextCursor: "connection-1",
        totalCount: 2,
        complete: false,
      }),
    ).toBe(false);
  });

  it("never selects a production target for a drill", () => {
    const snapshot = {
      engine: "postgresql",
      state: "complete",
      signatureStatus: "valid",
      artifactCompleteness: "complete",
    } as Snapshot;
    const connections = [
      {
        id: "production",
        engine: "postgresql",
        restoreTestTarget: false,
      },
      {
        id: "lab",
        engine: "postgresql",
        restoreTestTarget: true,
      },
      {
        id: "mysql-lab",
        engine: "mysql",
        restoreTestTarget: true,
      },
    ] as ConnectionProfile[];
    expect(
      targetConnectionsFor(connections, snapshot, "restore_drill").map(
        (connection) => connection.id,
      ),
    ).toEqual(["lab"]);
    expect(isDiagnosticSnapshot(snapshot)).toBe(false);
    expect(
      isDiagnosticSnapshot({
        ...snapshot,
        signatureStatus: "unknown_key",
      }),
    ).toBe(true);
  });
});
