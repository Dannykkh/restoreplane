import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Snapshot } from "../lib/types";
import { Snapshots } from "./Snapshots";

const apiMocks = vi.hoisted(() => ({
  listSnapshots: vi.fn(),
  getSnapshotDetailPage: vi.fn(),
  enqueueVerification: vi.fn(),
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
  requiredPartCount: 0,
  presentPartCount: 0,
  plaintextBytes: 1,
  cipherBytes: 1,
  firstIngestedAtMs: 90,
  lastIngestedAtMs: 100,
  diagnosticReason: null,
  partCount: 0,
  replicaCount: 1,
  verificationEvidenceCount: 0,
  restoreDrillCount: 0,
} satisfies Snapshot;

describe("Snapshots stale inventory gate", () => {
  beforeEach(() => {
    for (const mock of Object.values(apiMocks)) mock.mockReset();
    apiMocks.listSnapshots
      .mockResolvedValueOnce({ items: [snapshot], nextCursor: null, totalCount: 1 })
      .mockRejectedValueOnce(new Error("refresh failed"));
    apiMocks.getSnapshotDetailPage.mockImplementation(
      async (request: { section: string }) => {
        const replicaItems = request.section === "replicas"
          ? [{
              id: "replica-1",
              destinationFingerprint: "s3:source",
              destinationKind: "s3",
              requirement: "required",
              state: "committed",
              immutabilityCapability: "compliance",
              retentionUntilMs: null,
              readerStatus: "eligible",
              observedAtMs: 100,
            }]
          : [];
        return {
          snapshot,
          section: request.section,
          items: replicaItems,
          nextCursor: null,
          totalCount: replicaItems.length,
          complete: true,
        };
      },
    );
  });

  it("disables verification after refresh fails with cached complete rows", async () => {
    const user = userEvent.setup();
    render(<Snapshots route={{ page: "snapshots" }} />);

    const l1 = await screen.findByRole("button", { name: "L1" });
    await waitFor(() => expect(l1).toBeEnabled());
    await user.click(screen.getByRole("button", { name: "근거 새로고침" }));

    expect(
      await screen.findByText("마지막 Snapshot 근거를 표시합니다."),
    ).toBeInTheDocument();
    expect(l1).toBeDisabled();
    expect(apiMocks.enqueueVerification).not.toHaveBeenCalled();
  });
});
