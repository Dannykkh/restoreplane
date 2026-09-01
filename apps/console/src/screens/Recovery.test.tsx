import { render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ConnectionProfile, Destination, Snapshot } from "../lib/types";
import { Recovery } from "./Recovery";

const apiMocks = vi.hoisted(() => ({
  listSnapshots: vi.fn(),
  listConnections: vi.fn(),
  listDestinations: vi.fn(),
  getSnapshotDetailPage: vi.fn(),
}));

vi.mock("../lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/api")>()),
  ...apiMocks,
}));

const snapshot = {
  id: "snapshot-1",
  sourceTargetIdentity: "postgresql:orders",
  sourceDisplayName: "orders",
  sourceConnectionId: "connection-source",
  sourceConnectionName: "Source PostgreSQL",
  engine: "postgresql",
  state: "complete",
  committedAtMs: 100,
  manifestSchemaVersion: 2,
  setManifestChecksumBlake3: "a".repeat(64),
  signingKeyId: "key-1",
  artifactCompleteness: "complete",
  replicaDurability: "healthy",
  protectionCompliance: "compliant",
  restoreEligibility: "eligible",
  highestVerifiedLevel: 1,
  lastVerifiedAtMs: 100,
  signatureStatus: "valid",
  requiredPartCount: 1,
  presentPartCount: 1,
  plaintextBytes: 1,
  cipherBytes: 1,
  firstIngestedAtMs: 90,
  lastIngestedAtMs: 100,
  diagnosticReason: null,
  partCount: 1,
  replicaCount: 1,
  verificationEvidenceCount: 0,
  restoreDrillCount: 0,
} satisfies Snapshot;

describe("Recovery bounded inventories", () => {
  beforeEach(() => {
    for (const mock of Object.values(apiMocks)) mock.mockReset();
    apiMocks.listSnapshots.mockResolvedValue({
      items: [snapshot],
      nextCursor: null,
      totalCount: 1,
    });
    apiMocks.listConnections.mockResolvedValue({
      items: [{
        id: "connection-target",
        engine: "postgresql",
        displayName: "Restore target",
        restoreTestTarget: true,
      } as ConnectionProfile],
      nextCursor: "stuck-connection-cursor",
      totalCount: 2,
    });
    apiMocks.listDestinations.mockResolvedValue({
      items: [{
        id: "destination-1",
        displayName: "DR destination",
        kind: "s3",
        state: "ready",
      } as Destination],
      nextCursor: null,
      totalCount: 1,
    });
    apiMocks.getSnapshotDetailPage.mockResolvedValue({
      snapshot,
      section: "replicas",
      items: [{
        id: "replica-1",
        destinationFingerprint: "s3:source",
        destinationKind: "s3",
        requirement: "required",
        state: "committed",
        immutabilityCapability: "compliance",
        retentionUntilMs: null,
        readerStatus: "eligible",
        observedAtMs: 100,
      }],
      nextCursor: null,
      totalCount: 1,
      complete: true,
    });
  });

  it("blocks recovery when the Connection inventory cursor cannot terminate", async () => {
    render(<Recovery route={{ page: "recovery" }} />);

    const targetButton = await screen.findByRole("button", {
      name: "Target 선택",
    });
    await waitFor(() =>
      expect(screen.getByText("복구 인벤토리가 완전하지 않습니다."))
        .toBeInTheDocument(),
    );
    expect(targetButton).toBeDisabled();
    expect(apiMocks.listConnections).toHaveBeenCalledTimes(2);
  });
});
