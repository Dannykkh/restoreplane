import { describe, expect, it } from "vitest";
import {
  collectSnapshotSectionPages,
  selectCompleteReaderReplica,
  snapshotMutationInventoryReady,
} from "./snapshotInventory";
import type { Snapshot, SnapshotReplica } from "./types";

const eligible = {
  id: "replica-good",
  state: "committed",
  readerStatus: "eligible",
} as SnapshotReplica;

describe("snapshot detail mutation boundary", () => {
  it("selects a restore source only from an exact complete replica inventory", () => {
    expect(
      selectCompleteReaderReplica({
        items: [eligible],
        nextCursor: null,
        totalCount: 1,
        complete: true,
      })?.id,
    ).toBe("replica-good");
  });

  it("fails closed when the replica page is partial or its exact count disagrees", () => {
    expect(
      selectCompleteReaderReplica({
        items: [eligible],
        nextCursor: "replica-good",
        totalCount: 2,
        complete: false,
      }),
    ).toBeNull();
    expect(
      selectCompleteReaderReplica({
        items: [eligible],
        nextCursor: null,
        totalCount: 2,
        complete: true,
      }),
    ).toBeNull();
  });

  it("collects a selected snapshot section only when every page has the same identity", async () => {
    const snapshot = { id: "snapshot-1", replicaCount: 2 } as Snapshot;
    const result = await collectSnapshotSectionPages(
      "snapshot-1",
      "replicas",
      async (cursor) =>
        cursor === null
          ? {
              snapshot,
              section: "replicas" as const,
              items: [eligible],
              nextCursor: "replica-good",
              totalCount: 2,
              complete: false,
            }
          : {
              snapshot,
              section: "replicas" as const,
              items: [{ ...eligible, id: "replica-second" }],
              nextCursor: null,
              totalCount: 2,
              complete: true,
            },
    );

    expect(result.snapshot.id).toBe("snapshot-1");
    expect(result.items.map((item) => item.id)).toEqual([
      "replica-good",
      "replica-second",
    ]);
    expect(result.complete).toBe(true);
  });

  it("rejects identity drift and contradictory server completeness", async () => {
    const snapshot = { id: "snapshot-1", replicaCount: 0 } as Snapshot;
    await expect(
      collectSnapshotSectionPages("snapshot-1", "replicas", async () => ({
        snapshot: { id: "snapshot-2" } as Snapshot,
        section: "replicas" as const,
        items: [],
        nextCursor: null,
        totalCount: 0,
        complete: true,
      })),
    ).rejects.toThrow("SNAPSHOT_DETAIL_IDENTITY_MISMATCH");
    await expect(
      collectSnapshotSectionPages("snapshot-1", "replicas", async () => ({
        snapshot,
        section: "replicas" as const,
        items: [],
        nextCursor: null,
        totalCount: 0,
        complete: false,
      })),
    ).rejects.toThrow("SNAPSHOT_DETAIL_COMPLETENESS_INVALID");
  });

  it("rejects a detail total that disagrees with the selected summary", async () => {
    const snapshot = { id: "snapshot-1", replicaCount: 1 } as Snapshot;
    await expect(
      collectSnapshotSectionPages("snapshot-1", "replicas", async () => ({
        snapshot,
        section: "replicas" as const,
        items: [],
        nextCursor: null,
        totalCount: 0,
        complete: true,
      })),
    ).rejects.toThrow("SNAPSHOT_DETAIL_COUNT_MISMATCH");
  });

  it("blocks mutation when a refresh failed despite cached complete data", () => {
    expect(snapshotMutationInventoryReady("ready", "ready", true, true)).toBe(true);
    expect(snapshotMutationInventoryReady("error", "ready", true, true)).toBe(false);
    expect(snapshotMutationInventoryReady("ready", "error", true, true)).toBe(false);
  });
});
