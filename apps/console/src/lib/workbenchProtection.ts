import { isTauri } from "@tauri-apps/api/core";
import { activeWorkspaceId, getWorkItem, getWorkbenchRestoreWorkItem, runBackupScopeDiscovery, saveBackupScope, listBackupScopes, getSnapshotDetailPage } from "./api";
import type { BackupScope, Snapshot, BackupScopeDiscoveryRun, BackupScopeSaveRequest, BackupScopeSaveResult, WorkItemStatus } from "./types";
import { WorkbenchError, workbenchErrorText, type WorkbenchClient } from "./workbench";
import { nativeU64, type ObjectRef, type Session, type WorkbenchRestoreProof, type WorkbenchSelection, type WorkbenchSelectionItem, type WorkbenchSelectionProof } from "./workbenchTypes";

export interface WorkbenchProtectionGateway {
  scopes?(targetId: string, cursor: string | null): Promise<{ items: BackupScope[]; nextCursor: string | null }>;
  snapshot?(snapshotId: string): Promise<Snapshot>;
  discover(targetId: string, selection: WorkbenchSelection, idempotencyKey: string): Promise<BackupScopeDiscoveryRun>;
  work(workItemId: string, includeRestoreProof?: boolean): Promise<WorkItemStatus>;
  save(request: BackupScopeSaveRequest, idempotencyKey: string): Promise<BackupScopeSaveResult>;
}
function nativeService(): void { if (!isTauri()) throw new WorkbenchError("WB_NATIVE_UNAVAILABLE"); }
export const workbenchProtection: WorkbenchProtectionGateway = {
  async scopes(targetId, cursor) { nativeService(); return listBackupScopes({ databaseTargetId: targetId, cursor, limit: 20 }); },
  async snapshot(snapshotId) { nativeService(); const result = await getSnapshotDetailPage({ snapshotId, section: "replicas", cursor: null, limit: 1 }); if (result.snapshot.id !== snapshotId) throw new WorkbenchError("WB_BINDING_STALE"); return result.snapshot; },
  async discover(targetId, selection, key) { nativeService(); return runBackupScopeDiscovery(targetId, key, selection); },
  async work(id, includeProof) { nativeService(); return includeProof ? getWorkbenchRestoreWorkItem(id) : getWorkItem(id); },
  async save(request, key) { nativeService(); return saveBackupScope(request, key); },
};
const digest = (value: unknown): value is string => typeof value === "string" && /^[0-9a-f]{64}$/.test(value);
function itemsKey(items: WorkbenchSelectionItem[]): string {
  return JSON.stringify(items.map((item) => [item.engineObjectId, item.schemaName, item.objectName, item.structureSignature]).sort((a, b) => JSON.stringify(a) < JSON.stringify(b) ? -1 : JSON.stringify(a) > JSON.stringify(b) ? 1 : 0));
}
export async function getProtectionSelection(client: WorkbenchClient, session: Session, objects: ObjectRef[]): Promise<WorkbenchSelection> {
  if (objects.length < 1 || objects.length > 100 || session.workspaceId !== activeWorkspaceId()) throw new WorkbenchError("WB_PAYLOAD_INVALID");
  const result = await client.request("protection.selection.get", { session: { sessionId: session.sessionId, connectionGeneration: session.connectionGeneration }, objects });
  const selected = result.workbenchSelection;
  if (result.scope.workspaceId !== session.workspaceId || result.scope.connectionProfileId !== session.connectionProfileId || result.scope.databaseTargetId !== session.databaseTargetId || selected.signatureVersion !== 1 || selected.expectedDescriptorRevision !== session.descriptorRevision || selected.items.length !== objects.length || selected.items.length > 100 || new Set(selected.items.map((item) => item.engineObjectId)).size !== selected.items.length || selected.items.some((item) => !digest(item.structureSignature)) || new TextEncoder().encode(JSON.stringify(selected)).byteLength > 64 * 1024) throw new WorkbenchError("WB_BINDING_STALE");
  return selected;
}
export function selectionProof(work: WorkItemStatus, selection: WorkbenchSelection, operationRunId: string): WorkbenchSelectionProof {
  if (work.state !== "succeeded" || work.operationRunId !== operationRunId) throw new WorkbenchError("WB_BINDING_UNPROVEN");
  const value = work.result?.workbenchSelection as WorkbenchSelectionProof | undefined;
  if (!value || !digest(value.selectionDigest) || value.descriptorRevision !== selection.expectedDescriptorRevision || !Array.isArray(value.items) || value.items.length > 100 || itemsKey(value.items) !== itemsKey(selection.items)) throw new WorkbenchError("WB_BINDING_STALE");
  nativeU64(value.verifiedAtMs); return value;
}
/** This is explanatory data only. session.open re-fetches native proof itself. */
export async function restoredInspection(gateway: WorkbenchProtectionGateway, workItemId: string): Promise<WorkbenchRestoreProof> {
  if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(workItemId)) throw new WorkbenchError("WB_BINDING_UNPROVEN");
  const work = await gateway.work(workItemId, true), proof = work.workbenchRestoreProof;
  if (work.workItemId !== workItemId || work.state !== "succeeded" || !proof || proof.restoreWorkItemId !== workItemId || !["sqlite", "postgresql"].includes(proof.engine) || !proof.connectionProfileId || !proof.databaseName || !proof.actualDatabaseIdentity || BigInt(nativeU64(proof.completedAtMs)) === 0n) throw new WorkbenchError("WB_BINDING_UNPROVEN");
  return proof;
}
export function protectionErrorText(failure: unknown): string {
  if (failure instanceof WorkbenchError) return workbenchErrorText(failure);
  return "백업 서비스의 상태 또는 선택 구조를 확인하지 못했습니다. 기존 작업 근거를 확인한 뒤 명시적으로 다시 검토하세요.";
}

/** Read-only explanatory evidence; this grants no SQL admission or rollback. */
export function backupDependencyEvidence(session: Session, scope: BackupScope, snapshot: Snapshot): void {
  if (scope.databaseTargetId !== session.databaseTargetId || scope.connectionId !== session.connectionProfileId || scope.state !== "active" || scope.sourceDrift || !digest(scope.sourceFingerprint) || !digest(scope.selectorHashBlake3) || !scope.latestRecoveryPointId || snapshot.id !== scope.latestRecoveryPointId || snapshot.sourceTargetIdentity !== scope.sourceFingerprint || snapshot.sourceConnectionId !== session.connectionProfileId || snapshot.engine !== scope.engine || snapshot.state !== "complete" || snapshot.artifactCompleteness !== "complete" || snapshot.signatureStatus !== "valid" || snapshot.requiredPartCount < 1 || snapshot.presentPartCount !== snapshot.requiredPartCount || !Number.isSafeInteger(snapshot.committedAtMs) || snapshot.committedAtMs! <= 0 || snapshot.committedAtMs! > Date.now()) throw new WorkbenchError("WB_BINDING_UNPROVEN");
}
