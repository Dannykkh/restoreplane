import type {
  ConnectionProfile,
  RestoreEnqueueRequest,
  RestorePreflightResult,
  RestorePurpose,
  Snapshot,
  WorkItemStatus,
} from "./types";
import { isExactSnapshotInventory } from "./snapshotInventory";

export interface RecoveryInventoryEvidence<T> {
  items: readonly T[];
  nextCursor: string | null;
  totalCount: number | null;
  complete: boolean;
}

export function recoveryInventoryAllowsMutation(
  ...inventories: Array<RecoveryInventoryEvidence<unknown> | null>
): boolean {
  return inventories.length > 0 && inventories.every(inventoryIsExact);
}

function inventoryIsExact(
  inventory: RecoveryInventoryEvidence<unknown> | null,
): boolean {
  return (
    inventory !== null &&
    typeof inventory.totalCount === "number" &&
    isExactSnapshotInventory({
      items: inventory.items,
      nextCursor: inventory.nextCursor,
      totalCount: inventory.totalCount,
      complete: inventory.complete,
    })
  );
}

export function targetConnectionsFor(
  connections: readonly ConnectionProfile[],
  snapshot: Snapshot,
  purpose: RestorePurpose,
): ConnectionProfile[] {
  return connections.filter(
    (connection) =>
      connection.engine === snapshot.engine &&
      (purpose !== "restore_drill" || connection.restoreTestTarget),
  );
}

export function restoreRequestFromPreflight(
  preflight: RestorePreflightResult,
  requiredVerificationLevel: 1 | 2 | 3,
): RestoreEnqueueRequest {
  return {
    snapshotId: preflight.source.snapshotId,
    sourceReplicaId: preflight.source.replicaId,
    targetConnectionId: preflight.targetConnectionId,
    targetName: preflight.targetName,
    preflightVersion: preflight.preflightVersion,
    requiredVerificationLevel,
    purpose: preflight.purpose,
  };
}

export function isDiagnosticSnapshot(snapshot: Snapshot): boolean {
  return (
    snapshot.state !== "complete" ||
    snapshot.signatureStatus !== "valid" ||
    snapshot.artifactCompleteness !== "complete"
  );
}

export interface RestoreTimelineEntry {
  key: "queued" | "running" | "validation" | "terminal" | "cleanup";
  label: string;
  state: "complete" | "current" | "failed" | "upcoming" | "attention";
  detail: string;
}

export function restoreTimeline(
  work: WorkItemStatus | null,
): RestoreTimelineEntry[] {
  const state = work?.state ?? "queued";
  const failed =
    state === "failed" ||
    state === "failed_terminal" ||
    state === "cancelled";
  const terminal =
    failed ||
    state === "succeeded" ||
    state === "partially_succeeded";
  const running =
    state === "running" ||
    state === "claimed" ||
    state === "succeeded" ||
    state === "partially_succeeded";
  const cleanupRequired = work?.result?.cleanupRequired === true;
  return [
    {
      key: "queued",
      label: "Durable work accepted",
      state: "complete",
      detail: work?.waitReason ?? "서비스 queue에 기록됐습니다.",
    },
    {
      key: "running",
      label: "Native restore",
      state: failed ? "failed" : running ? "complete" : "current",
      detail: running
        ? "서비스가 native restore process를 실행했습니다."
        : "resource permit과 worker claim을 기다립니다.",
    },
    {
      key: "validation",
      label: "Validation oracle",
      state: failed
        ? "failed"
        : terminal
          ? "complete"
          : running
            ? "current"
            : "upcoming",
      detail:
        work?.result?.validation === "passed"
          ? "restore 뒤 validation이 통과했습니다."
          : "process exit와 별도로 validation 결과를 기다립니다.",
    },
    {
      key: "terminal",
      label: "Terminal result",
      state: failed
        ? "failed"
        : terminal
          ? "complete"
          : "upcoming",
      detail: work?.errorCode
        ? `오류 코드 ${work.errorCode}`
        : terminal
          ? state
          : "terminal evidence 전에는 성공으로 표시하지 않습니다.",
    },
    {
      key: "cleanup",
      label: "Cleanup decision",
      state: cleanupRequired
        ? "attention"
        : terminal
          ? "complete"
          : "upcoming",
      detail: cleanupRequired
        ? "자동 삭제하지 않았습니다. 명시적 cleanup이 필요합니다."
        : terminal
          ? "cleanup required 근거가 없습니다."
          : "실패 또는 취소 시 target을 보존합니다.",
    },
  ];
}
