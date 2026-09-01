import { collectCursorPages, type CollectedCursorPage } from "./pagination";
import type { Snapshot, SnapshotReplica } from "./types";

export type SnapshotDetailSection =
  | "parts"
  | "replicas"
  | "verification_evidence"
  | "restore_drills";

export interface SnapshotSectionPage<T, S extends SnapshotDetailSection> {
  snapshot: Snapshot;
  section: S;
  items: T[];
  nextCursor: string | null;
  totalCount: number;
  complete: boolean;
}

export interface CollectedSnapshotSection<T, S extends SnapshotDetailSection>
  extends CollectedCursorPage<T> {
  snapshot: Snapshot;
  section: S;
}

export interface SnapshotReplicaInventory {
  items: SnapshotReplica[];
  nextCursor: string | null;
  totalCount: number | null;
  complete: boolean;
}

export function isExactSnapshotInventory<T>(inventory: {
  items: readonly T[];
  nextCursor: string | null;
  totalCount: number | null;
  complete: boolean;
}): boolean {
  return (
    inventory.complete &&
    inventory.nextCursor === null &&
    inventory.totalCount !== null &&
    Number.isSafeInteger(inventory.totalCount) &&
    inventory.totalCount >= 0 &&
    inventory.items.length === inventory.totalCount
  );
}

export function selectCompleteReaderReplica(
  inventory: SnapshotReplicaInventory | null,
): SnapshotReplica | null {
  if (!inventory || !isExactSnapshotInventory(inventory)) return null;
  return (
    inventory.items.find(
      (replica) =>
        replica.state === "committed" && replica.readerStatus === "eligible",
    ) ?? null
  );
}

export function snapshotMutationInventoryReady(
  snapshotStatus: "loading" | "ready" | "error",
  detailStatus: "loading" | "ready" | "error",
  snapshotInventoryComplete: boolean,
  detailInventoryComplete: boolean,
): boolean {
  return snapshotStatus === "ready" &&
    detailStatus === "ready" &&
    snapshotInventoryComplete &&
    detailInventoryComplete;
}

export async function collectSnapshotSectionPages<
  T,
  S extends SnapshotDetailSection,
>(
  snapshotId: string,
  section: S,
  fetchPage: (cursor: string | null) => Promise<SnapshotSectionPage<T, S>>,
): Promise<CollectedSnapshotSection<T, S>> {
  let snapshot: Snapshot | null = null;
  const collected = await collectCursorPages(async (cursor) => {
    const page = await fetchPage(cursor);
    if (page.snapshot.id !== snapshotId || page.section !== section) {
      throw new Error("SNAPSHOT_DETAIL_IDENTITY_MISMATCH");
    }
    if (page.complete !== (page.nextCursor === null)) {
      throw new Error("SNAPSHOT_DETAIL_COMPLETENESS_INVALID");
    }
    if (page.totalCount !== snapshotSectionCount(page.snapshot, section)) {
      throw new Error("SNAPSHOT_DETAIL_COUNT_MISMATCH");
    }
    if (snapshot && snapshot.setManifestChecksumBlake3 !== page.snapshot.setManifestChecksumBlake3) {
      throw new Error("SNAPSHOT_DETAIL_IDENTITY_MISMATCH");
    }
    snapshot = page.snapshot;
    return page;
  });
  if (!snapshot) throw new Error("SNAPSHOT_DETAIL_IDENTITY_MISMATCH");
  return { ...collected, snapshot, section };
}

function snapshotSectionCount(
  snapshot: Snapshot,
  section: SnapshotDetailSection,
): number {
  switch (section) {
    case "parts":
      return snapshot.partCount;
    case "replicas":
      return snapshot.replicaCount;
    case "verification_evidence":
      return snapshot.verificationEvidenceCount;
    case "restore_drills":
      return snapshot.restoreDrillCount;
  }
}
