import { ServiceError, asServiceError } from "./errors";
import {
  previewActivityPage,
  previewBackupScopeDiscovery,
  previewBackupScopeObjectPage,
  previewBackupScopePage,
  previewConnectionPage,
  previewTargetPage,
  previewDiscoveryResultPage,
  previewDestinationPage,
  previewDiscovery,
  previewEngineMatrix,
  previewPlanPage,
  previewProbeConnection,
  previewProbeDestination,
  previewCatalogRebuild,
  previewRestorePreflight,
  previewRestoreOverwriteConfirmation,
  previewSaveConnection,
  previewSaveBackupScope,
  previewSaveDestination,
  previewSelectTargets,
  previewServiceHealth,
  previewSettings,
  previewSnapshotDetailPage,
  previewSnapshotPage,
} from "./fixtures";
import { collectCursorPages, INVENTORY_PAGE_LIMIT } from "./pagination";
import type {
  SnapshotDetailSection,
  SnapshotSectionPage,
} from "./snapshotInventory";
import type {
  ActivityEvent,
  BackupEnqueueResult,
  BackupPlan,
  BackupPlanSaveRequest,
  BackupScope,
  BackupScopeDiscoveryRun,
  BackupScopeObjectPage,
  BackupScopeSaveRequest,
  BackupScopeSaveResult,
  CatalogRebuildPreview,
  ConnectionDraft,
  ConnectionProfile,
  ConnectionSaveResult,
  CursorPage,
  DatabaseTarget,
  Destination,
  DestinationDraft,
  DestinationSaveResult,
  DiscoveredDatabase,
  DiscoveredDatabaseObject,
  DiscoveryResult,
  EngineMatrix,
  PlanPublishResult,
  ReplicaRepairResult,
  RestoreEnqueueRequest,
  RestoreEnqueueResult,
  RestoreOverwriteConfirmation,
  RestoreOverwritePrepareRequest,
  RestorePreflightRequest,
  RestorePreflightResult,
  ServiceHealth,
  SettingsData,
  Snapshot,
  SnapshotPart,
  SnapshotReplica,
  VerificationEvidence,
  RestoreDrillEvidence,
  VerificationEnqueueResult,
  TargetSelectionResult,
  WorkItemRef,
  WorkItemStatus,
  Workspace,
} from "./types";

type ReadCommand =
  | "service.health"
  | "schema.status"
  | "workspace.list"
  | "connection.list"
  | "target.list"
  | "discovery.result_list"
  | "backup_scope.list"
  | "backup_scope.object_list"
  | "destination.list"
  | "plan.list"
  | "engine.matrix"
  | "activity.list"
  | "snapshot.list"
  | "snapshot.get"
  | "catalog.rebuild_preview"
  | "restore.preflight"
  | "work.get"
  | "settings.read";

type MutationCommand =
  | "workspace.create"
  | "connection.save"
  | "connection.probe"
  | "backup_scope.discovery_run"
  | "backup_scope.save"
  | "discovery.run"
  | "target.select"
  | "destination.save"
  | "destination.probe"
  | "plan.save"
  | "plan.publish"
  | "settings.update"
  | "backup.enqueue"
  | "verification.enqueue"
  | "confirmation.prepare"
  | "restore.enqueue_safe"
  | "restore.enqueue_overwrite"
  | "repair.enqueue"
  | "work.cancel"
  | "tool.rescan";

export type ServiceCommand = ReadCommand | MutationCommand;

export const DEFAULT_WORKSPACE_ID = "00000000-0000-4000-8000-000000000001";
const WORKSPACE_STORAGE_KEY = "restoreplane.workspaceId";
const WORKSPACE_SCOPED_COMMANDS = new Set<ServiceCommand>([
  "connection.list",
  "target.list",
  "discovery.result_list",
  "backup_scope.list",
  "backup_scope.object_list",
  "destination.list",
  "plan.list",
  "activity.list",
  "snapshot.list",
  "snapshot.get",
  "catalog.rebuild_preview",
  "restore.preflight",
  "work.get",
  "connection.save",
  "connection.probe",
  "backup_scope.discovery_run",
  "backup_scope.save",
  "discovery.run",
  "target.select",
  "destination.save",
  "destination.probe",
  "plan.save",
  "plan.publish",
  "backup.enqueue",
  "verification.enqueue",
  "confirmation.prepare",
  "restore.enqueue_safe",
  "restore.enqueue_overwrite",
  "repair.enqueue",
  "work.cancel",
]);
let selectedWorkspaceId = storedWorkspaceId();

interface IpcOutcome<T> {
  requestId: string;
  success: boolean;
  deduplicated: boolean;
  code: string;
  result: T;
  error: { code?: string; message?: string; retryable?: boolean } | null;
}

export interface PageRequest {
  cursor: string | null;
  limit: number;
}

export interface ActivityPageRequest extends PageRequest {
  severity: ActivityEvent["severity"] | null;
}

export interface BackupScopeListRequest extends PageRequest {
  databaseTargetId?: string | null;
  search?: string | null;
  state?: BackupScope["state"] | null;
}

export interface TargetListRequest extends PageRequest {
  connectionId?: string | null;
  search?: string | null;
  state?: DatabaseTarget["state"] | null;
}

export interface DiscoveryResultListRequest extends PageRequest {
  discoveryRunId: string;
  search?: string | null;
  eligibility?: DiscoveredDatabase["eligibility"] | null;
}

export interface BackupScopeObjectListRequest extends PageRequest {
  databaseTargetId: string;
  discoveryRunId: string;
  search?: string | null;
  schemaName?: string | null;
  eligibility?: DiscoveredDatabaseObject["eligibility"] | null;
}

export interface SnapshotPageRequest extends PageRequest {
  scope?: "complete" | "diagnostics" | "all";
  search?: string | null;
  connectionId?: string | null;
  engine?: string | null;
  fromMs?: number | null;
  toMs?: number | null;
  artifactCompleteness?: string | null;
  replicaDurability?: string | null;
  protectionCompliance?: string | null;
  minVerificationLevel?: number | null;
  restoreEligibility?: string | null;
  signatureStatus?: string | null;
}

export type SnapshotDetailItemBySection = {
  parts: SnapshotPart;
  replicas: SnapshotReplica;
  verification_evidence: VerificationEvidence;
  restore_drills: RestoreDrillEvidence;
};

export interface SnapshotDetailPageRequest<S extends SnapshotDetailSection>
  extends PageRequest {
  snapshotId: string;
  section: S;
}

export const runtimeEvidenceMode: "service" | "browser_fixture" = inTauri()
  ? "service"
  : "browser_fixture";

export async function getServiceHealth(): Promise<ServiceHealth> {
  return serviceRequest<ServiceHealth>("service.health", {});
}

export async function listWorkspaces(): Promise<Workspace[]> {
  const inventory = await collectCursorPages((cursor) =>
    serviceRequest<unknown>("workspace.list", {
      cursor,
      limit: INVENTORY_PAGE_LIMIT,
    }).then(normalizeCursorPageEnvelope<Workspace>),
  );
  if (!inventory.complete) {
    throw new ServiceError("WORKSPACE_INVENTORY_INCOMPLETE", {
      retryable: true,
    });
  }
  return inventory.items;
}

export async function createWorkspace(
  displayName: string,
  idempotencyKey?: string,
): Promise<Workspace> {
  return serviceMutation("workspace.create", { displayName }, idempotencyKey);
}

export function activeWorkspaceId(): string {
  return selectedWorkspaceId;
}

export function setActiveWorkspaceId(workspaceId: string): void {
  if (!isCanonicalWorkspaceId(workspaceId)) {
    throw new Error("WORKSPACE_ID_INVALID");
  }
  selectedWorkspaceId = workspaceId;
  if (typeof window !== "undefined") {
    window.localStorage.setItem(WORKSPACE_STORAGE_KEY, workspaceId);
  }
}

export function scopeServicePayload(
  command: ServiceCommand,
  payload: object,
): object {
  return WORKSPACE_SCOPED_COMMANDS.has(command)
    ? { ...payload, workspaceId: selectedWorkspaceId }
    : payload;
}

export async function listConnections(
  request: PageRequest = { cursor: null, limit: 50 },
): Promise<CursorPage<ConnectionProfile>> {
  const result = await serviceRequest<unknown>("connection.list", request);
  return normalizeConnectionPageResult(result);
}

export async function listDatabaseTargets(
  request: TargetListRequest = { cursor: null, limit: 50 },
): Promise<CursorPage<DatabaseTarget>> {
  const result = await serviceRequest<unknown>("target.list", request);
  return normalizeTargetPageResult(result);
}

export async function listDiscoveryResults(
  request: DiscoveryResultListRequest,
): Promise<CursorPage<DiscoveredDatabase>> {
  const result = await serviceRequest<unknown>("discovery.result_list", request);
  return normalizeDiscoveryResultPage(result);
}

export async function listBackupScopes(
  request: BackupScopeListRequest = { cursor: null, limit: 50 },
): Promise<{ items: BackupScope[]; nextCursor: string | null; totalCount: number }> {
  const result = await serviceRequest<unknown>("backup_scope.list", request);
  return normalizeBackupScopeListResult(result, request);
}

export async function runBackupScopeDiscovery(
  databaseTargetId: string,
  idempotencyKey?: string,
  workbenchSelection?: import("./workbenchTypes").WorkbenchSelection,
): Promise<BackupScopeDiscoveryRun> {
  const result = await serviceMutation<unknown>("backup_scope.discovery_run", {
    databaseTargetId,
    ...(workbenchSelection ? { workbenchSelection } : {}),
  }, idempotencyKey);
  return normalizeBackupScopeDiscoveryResult(result);
}

export async function listBackupScopeObjects(
  request: BackupScopeObjectListRequest,
): Promise<BackupScopeObjectPage> {
  const result = normalizeBackupScopeObjectPageResult(
    await serviceRequest<unknown>("backup_scope.object_list", request),
  );
  if (result.discoveryRunId !== request.discoveryRunId) {
    throw new ServiceError("BACKUP_SCOPE_DISCOVERY_STALE", { retryable: true });
  }
  return result;
}

export async function saveBackupScope(
  request: BackupScopeSaveRequest,
  idempotencyKey?: string,
): Promise<BackupScopeSaveResult> {
  const payload = inTauri()
    ? {
        databaseTargetId: request.databaseTargetId,
        displayName: request.displayName,
        mode: request.mode,
        discoveryRunId: request.discoveryRunId,
        engineObjectIds: request.engineObjectIds,
        ...(request.expectedWorkbenchSelectionDigest ? { expectedWorkbenchSelectionDigest: request.expectedWorkbenchSelectionDigest } : {}),
      }
    : request;
  return normalizeBackupScopeSaveResult(
    await serviceMutation<unknown>("backup_scope.save", payload, idempotencyKey),
  );
}

export async function listDestinations(
  request: PageRequest = { cursor: null, limit: 50 },
): Promise<CursorPage<Destination>> {
  return normalizeCursorPageEnvelope<Destination>(
    await serviceRequest<unknown>("destination.list", request),
  );
}

export async function listPlans(
  request: PageRequest = { cursor: null, limit: 50 },
): Promise<CursorPage<BackupPlan>> {
  return normalizeCursorPageEnvelope<BackupPlan>(
    await serviceRequest<unknown>("plan.list", request),
  );
}

export async function getEngineMatrix(): Promise<EngineMatrix> {
  return serviceRequest("engine.matrix", {});
}

export async function listActivity(
  request: ActivityPageRequest = { cursor: null, limit: 50, severity: null },
): Promise<CursorPage<ActivityEvent>> {
  return normalizeCursorPageEnvelope<ActivityEvent>(
    await serviceRequest<unknown>("activity.list", request),
  );
}

export async function listSnapshots(
  request: SnapshotPageRequest = { cursor: null, limit: 50, scope: "complete" },
): Promise<CursorPage<Snapshot>> {
  return normalizeCursorPageEnvelope<Snapshot>(
    await serviceRequest<unknown>("snapshot.list", request),
  );
}

export async function getSnapshotDetailPage<S extends SnapshotDetailSection>(
  request: SnapshotDetailPageRequest<S>,
): Promise<SnapshotSectionPage<SnapshotDetailItemBySection[S], S>> {
  return serviceRequest("snapshot.get", request);
}

export async function getCatalogRebuildPreview(
  destinationId: string,
  jobFilter?: string,
): Promise<CatalogRebuildPreview> {
  return serviceRequest("catalog.rebuild_preview", {
    destinationId,
    jobFilter,
  });
}

export async function readSettings(): Promise<SettingsData> {
  return serviceRequest("settings.read", {});
}

export async function saveConnection(
  draft: ConnectionDraft,
  idempotencyKey?: string,
): Promise<ConnectionSaveResult> {
  return serviceMutation("connection.save", draft, idempotencyKey);
}

export async function probeConnection(
  connectionId: string,
  idempotencyKey?: string,
): Promise<WorkItemRef> {
  return serviceMutation("connection.probe", { connectionId }, idempotencyKey);
}

export async function discoverConnection(
  connectionId: string,
  idempotencyKey?: string,
): Promise<DiscoveryResult> {
  return serviceMutation("discovery.run", { connectionId }, idempotencyKey);
}

export async function selectConnectionTargets(
  connectionId: string,
  discoveryRunId: string,
  engineObjectIds: readonly string[],
  idempotencyKey?: string,
): Promise<TargetSelectionResult> {
  return serviceMutation("target.select", {
    connectionId,
    discoveryRunId,
    engineObjectIds,
  }, idempotencyKey);
}

export async function saveDestination(
  draft: DestinationDraft,
  idempotencyKey?: string,
): Promise<DestinationSaveResult> {
  return serviceMutation("destination.save", draft, idempotencyKey);
}

export async function probeDestination(
  destinationId: string,
  idempotencyKey?: string,
): Promise<WorkItemRef> {
  return serviceMutation("destination.probe", { destinationId }, idempotencyKey);
}

export async function savePlan(payload: BackupPlanSaveRequest, idempotencyKey?: string): Promise<{
  planId: string;
  revisionId: string;
}> {
  return serviceMutation("plan.save", payload, idempotencyKey);
}

export async function publishPlan(
  planId: string,
  revisionId: string,
  idempotencyKey?: string,
): Promise<PlanPublishResult> {
  return serviceMutation("plan.publish", { planId, revisionId }, idempotencyKey);
}

export async function getWorkItem(workItemId: string): Promise<WorkItemStatus> {
  return serviceRequest("work.get", { workItemId });
}

export async function getWorkbenchRestoreWorkItem(workItemId: string): Promise<WorkItemStatus> {
  return serviceRequest("work.get", { workItemId, includeWorkbenchRestoreProof: true });
}

export async function enqueueBackup(
  planId: string,
  idempotencyKey?: string,
): Promise<BackupEnqueueResult> {
  return serviceMutation("backup.enqueue", { planId }, idempotencyKey);
}

export async function enqueueVerification(
  snapshotId: string,
  level: 1 | 2 | 3,
  sourceReplicaId?: string,
  idempotencyKey?: string,
): Promise<VerificationEnqueueResult> {
  return serviceMutation("verification.enqueue", {
    snapshotId,
    level,
    purpose: "manual",
    sourceReplicaId,
  }, idempotencyKey);
}

export async function enqueueRepair(
  snapshotId: string,
  sourceReplicaId: string,
  destinationId: string,
  idempotencyKey?: string,
): Promise<ReplicaRepairResult> {
  return serviceMutation("repair.enqueue", {
    snapshotId,
    sourceReplicaId,
    targetDestinationId: destinationId,
  }, idempotencyKey);
}

export async function preflightRestore(
  payload: RestorePreflightRequest,
): Promise<RestorePreflightResult> {
  return serviceRequest("restore.preflight", payload);
}

export async function enqueueSafeRestore(
  payload: RestoreEnqueueRequest,
  idempotencyKey?: string,
): Promise<RestoreEnqueueResult> {
  return serviceMutation("restore.enqueue_safe", payload, idempotencyKey);
}

export async function prepareRestoreOverwrite(
  payload: RestoreOverwritePrepareRequest,
  idempotencyKey?: string,
): Promise<RestoreOverwriteConfirmation> {
  return serviceMutation("confirmation.prepare", payload, idempotencyKey);
}

export async function enqueueOverwriteRestore(
  payload: Record<string, unknown>,
  idempotencyKey?: string,
): Promise<RestoreEnqueueResult> {
  return serviceMutation("restore.enqueue_overwrite", payload, idempotencyKey);
}

export function workCancelPayload(workItemId: string, fencingToken: number) {
  if (!Number.isSafeInteger(fencingToken) || fencingToken < 0) {
    throw new Error("WORK_FENCING_TOKEN_INVALID");
  }
  return { workItemId, fencingToken };
}

export async function cancelWork(
  workItemId: string,
  fencingToken: number,
  idempotencyKey?: string,
): Promise<WorkItemStatus> {
  return serviceMutation(
    "work.cancel",
    workCancelPayload(workItemId, fencingToken),
    idempotencyKey,
  );
}

export async function subscribeActivity(
  afterSequence: number,
  onEvent: (event: ActivityEvent) => void,
): Promise<() => void> {
  if (!inTauri()) return () => undefined;
  const { listen } = await import("@tauri-apps/api/event");
  return listen<ActivityEvent>("service:activity", (event) => {
    if (event.payload.seq > afterSequence) onEvent(event.payload);
  });
}

export function normalizeBackupScopeListResult(
  result: unknown,
  request: BackupScopeListRequest = { cursor: null, limit: 50 },
): { items: BackupScope[]; nextCursor: string | null; totalCount: number } {
  const resultRecord = requiredRecord(result);
  const wireItems = requiredArray(resultRecord.items);
  const search = request.search?.trim().toLocaleLowerCase() ?? "";
  const items = wireItems
    .map(normalizeBackupScope)
    .filter((scope) =>
      (!request.databaseTargetId || scope.databaseTargetId === request.databaseTargetId) &&
      (!request.state || scope.state === request.state) &&
      (!search || [
        scope.displayName,
        scope.databaseName,
        scope.connectionName,
        scope.engine,
      ].some((value) => value.toLocaleLowerCase().includes(search))),
    );
  return {
    items,
    nextCursor: requiredCursor(resultRecord.nextCursor),
    totalCount: requiredCount(resultRecord.totalCount),
  };
}

export function normalizeConnectionPageResult(
  result: unknown,
): CursorPage<ConnectionProfile> {
  const page = requiredRecord(result);
  const items = requiredArray(page.items)
    .map((value): ConnectionProfile => {
        const record = requiredRecord(value);
        const id = stringOrNull(record.id);
        if (!id) throw new ServiceError("APPLICATION_PAYLOAD_INVALID");
        const targets = Array.isArray(record.targets)
          ? record.targets.map(normalizeDatabaseTarget)
          : [];
        const lastProbeStatus = oneOf(
          record.lastProbeStatus,
          ["passed", "failed", "cancelled"] as const,
          "cancelled",
        );
        return {
          id,
          displayName: stringOrNull(record.displayName) ?? id,
          engine: stringOrNull(record.engine) ?? "unknown",
          transport: oneOf(
            record.transport,
            ["tcp", "unix_socket", "file"] as const,
            "tcp",
          ),
          endpoint: stringOrNull(record.endpoint) ?? "endpoint unavailable",
          state: oneOf(
            record.state,
            ["draft", "ready", "attention", "disabled"] as const,
            "attention",
          ),
          lastProbeStatus: record.lastProbeStatus === null ? null : lastProbeStatus,
          lastProbeAtMs: nullableNumber(record.lastProbeAtMs),
          restoreTestTarget: record.restoreTestTarget === true,
          updatedAtMs: numberOr(record.updatedAtMs, 0),
          targetCount: numberOr(record.targetCount, targets.length),
          targets,
        };
      });
  return {
    items,
    nextCursor: requiredCursor(page.nextCursor),
    totalCount: requiredCount(page.totalCount),
  };
}

export function normalizeTargetPageResult(
  result: unknown,
): CursorPage<DatabaseTarget> {
  const page = requiredRecord(result);
  const items = requiredArray(page.items).map(normalizeDatabaseTarget);
  return {
    items,
    nextCursor: requiredCursor(page.nextCursor),
    totalCount: requiredCount(page.totalCount),
  };
}

export function normalizeDiscoveryResultPage(
  result: unknown,
): CursorPage<DiscoveredDatabase> {
  const page = requiredRecord(result);
  const items = requiredArray(page.items)
    .map((value): DiscoveredDatabase => {
        const record = requiredRecord(value);
        const engineObjectId = stringOrNull(record.engineObjectId);
        const databaseName = stringOrNull(record.databaseName);
        if (!engineObjectId || !databaseName) {
          throw new ServiceError("APPLICATION_PAYLOAD_INVALID");
        }
        return {
          engineObjectId,
          databaseName,
          eligibility: oneOf(
            record.eligibility,
            ["eligible", "excluded", "offline", "unknown"] as const,
            "unknown",
          ),
          exclusionReason: stringOrNull(record.exclusionReason),
        };
      });
  return {
    items,
    nextCursor: requiredCursor(page.nextCursor),
    totalCount: requiredCount(page.totalCount),
  };
}

function normalizeDatabaseTarget(value: unknown): DatabaseTarget {
  const record = requiredRecord(value);
  const id = stringOrNull(record.id);
  const connectionId = stringOrNull(record.connectionId);
  if (!id || !connectionId) throw new ServiceError("APPLICATION_PAYLOAD_INVALID");
  return {
    id,
    connectionId,
    connectionName: stringOrNull(record.connectionName) ?? connectionId,
    engine: stringOrNull(record.engine) ?? "unknown",
    displayName: stringOrNull(record.displayName) ?? id,
    state: oneOf(
      record.state,
      ["active", "attention", "disabled", "retired"] as const,
      "attention",
    ),
    missingSinceMs: nullableNumber(record.missingSinceMs),
    sourceFingerprint: stringOrNull(record.sourceFingerprint) ?? "",
    latestRecoveryPointId: stringOrNull(record.latestRecoveryPointId),
    latestSuccessAtMs: nullableNumber(record.latestSuccessAtMs),
    latestFailureAtMs: nullableNumber(record.latestFailureAtMs),
    artifactCompleteness: oneOf(
      record.artifactCompleteness,
      ["complete", "incomplete", "unknown"] as const,
      "unknown",
    ),
    replicaDurability: oneOf(
      record.replicaDurability,
      ["healthy", "degraded", "unavailable", "unknown"] as const,
      "unknown",
    ),
    protectionScope: oneOf(
      record.protectionScope,
      ["whole_database", "partial", "none", "unknown"] as const,
      "unknown",
    ),
    protectionCompliance: oneOf(
      record.protectionCompliance,
      ["compliant", "partial", "non_compliant", "not_configured", "unknown"] as const,
      "unknown",
    ),
    rpoViolation: record.rpoViolation === true,
    verificationState: oneOf(
      record.verificationState,
      ["not_configured", "unknown", "pending", "current"] as const,
      "unknown",
    ),
    verificationOverdue: record.verificationOverdue === true,
    evidenceAtMs: nullableNumber(record.evidenceAtMs),
  };
}

export function normalizeBackupScopeDiscoveryResult(
  result: unknown,
): BackupScopeDiscoveryRun {
  const record = requiredRecord(result);
  const operationRunId = stringOrNull(record.operationRunId) ??
    stringOrNull(record.discoveryRunId);
  const workItemId = stringOrNull(record.workItemId);
  if (!operationRunId || !workItemId) {
    throw new ServiceError("APPLICATION_PAYLOAD_INVALID");
  }
  return {
    operationRunId,
    workItemId,
    state: scopeDiscoveryState(record.state),
  };
}

export function normalizeBackupScopeObjectPageResult(
  result: unknown,
): BackupScopeObjectPage {
  const record = requiredRecord(result);
  const discoveryRunId = stringOrNull(record.discoveryRunId);
  if (!discoveryRunId) throw new ServiceError("APPLICATION_PAYLOAD_INVALID");
  const items = requiredArray(record.items)
    .map((item) => normalizeDatabaseObject(item, discoveryRunId));
  return {
    items,
    nextCursor: requiredCursor(record.nextCursor),
    totalCount: requiredCount(record.totalCount),
    eligibleCount: requiredCount(record.eligibleCount),
    discoveryRunId,
    inventoryHashBlake3: stringOrNull(record.inventoryHashBlake3) ?? "",
    observedAtMs: numberOr(record.observedAtMs, 0),
  };
}

export function normalizeBackupScopeSaveResult(
  result: unknown,
): BackupScopeSaveResult {
  const record = requiredRecord(result);
  const scopeId = stringOrNull(record.scopeId) ?? stringOrNull(record.id);
  const databaseTargetId = stringOrNull(record.databaseTargetId);
  if (!scopeId || !databaseTargetId) {
    throw new ServiceError("APPLICATION_PAYLOAD_INVALID");
  }
  const mode = backupScopeMode(record.mode);
  const replacedScopeId = stringOrNull(record.replacedScopeId);
  return {
    scopeId,
    databaseTargetId,
    mode,
    state: backupScopeState(record.state),
    selectedObjectCount:
      mode === "whole_database"
        ? null
        : nullableNumber(record.selectedObjectCount ?? record.resolvedObjectCount),
    selectorHashBlake3: stringOrNull(record.selectorHashBlake3) ?? "",
    sourceFingerprint: stringOrNull(record.sourceFingerprint) ?? "",
    ...(replacedScopeId ? { replacedScopeId } : {}),
  };
}

function normalizeBackupScope(value: unknown): BackupScope {
  const record = requiredRecord(value);
  const id = stringOrNull(record.id) ?? stringOrNull(record.scopeId);
  const databaseTargetId = stringOrNull(record.databaseTargetId);
  if (!id || !databaseTargetId) {
    throw new ServiceError("APPLICATION_PAYLOAD_INVALID");
  }
  const mode = backupScopeMode(record.mode);
  return {
    id,
    databaseTargetId,
    sourceFingerprint: stringOrNull(record.sourceFingerprint),
    selectorHashBlake3: stringOrNull(record.selectorHashBlake3),
    databaseName: stringOrNull(record.databaseName) ?? "",
    connectionId: stringOrNull(record.connectionId) ?? "",
    connectionName: stringOrNull(record.connectionName) ?? "",
    engine: stringOrNull(record.engine) ?? "",
    displayName: stringOrNull(record.displayName) ?? id,
    mode,
    state: backupScopeState(record.state),
    selectedObjectCount:
      mode === "whole_database"
        ? null
        : nullableNumber(record.selectedObjectCount ?? record.resolvedObjectCount),
    eligibleObjectCount: nullableNumber(record.eligibleObjectCount),
    newObjectCount: nullableNumber(record.newObjectCount),
    missingObjectCount: nullableNumber(record.missingObjectCount),
    changedObjectCount: nullableNumber(record.changedObjectCount),
    driftState: oneOf(
      record.driftState,
      ["unknown", "current", "drifted"] as const,
      "unknown",
    ),
    sourceDrift: record.sourceDrift === true,
    discoveryRunId: stringOrNull(record.discoveryRunId),
    inventoryHashBlake3: stringOrNull(record.inventoryHashBlake3),
    inventoryObservedAtMs: nullableNumber(record.inventoryObservedAtMs),
    latestBackupState: stringOrNull(record.latestBackupState),
    latestRecoveryPointId: stringOrNull(record.latestRecoveryPointId),
    latestSuccessAtMs: nullableNumber(record.latestSuccessAtMs),
    latestFailureAtMs: nullableNumber(record.latestFailureAtMs),
    artifactCompleteness: oneOf(
      record.artifactCompleteness,
      ["complete", "incomplete", "unknown"] as const,
      "unknown",
    ),
    replicaDurability: oneOf(
      record.replicaDurability,
      ["healthy", "degraded", "unavailable", "unknown"] as const,
      "unknown",
    ),
    protectionCompliance: oneOf(
      record.protectionCompliance,
      ["compliant", "non_compliant", "not_configured", "unknown"] as const,
      "unknown",
    ),
    restoreEligibility: oneOf(
      record.restoreEligibility,
      ["eligible", "conditional", "blocked", "unknown"] as const,
      "unknown",
    ),
    latestVerifiedLevel: nullableNumber(record.latestVerifiedLevel),
    lastVerifiedAtMs: nullableNumber(record.lastVerifiedAtMs),
    recoveryEvidenceAtMs: nullableNumber(record.recoveryEvidenceAtMs),
    rpoMs: nullableNumber(record.rpoMs),
    rpoState: oneOf(
      record.rpoState,
      ["not_configured", "unknown", "current", "violated"] as const,
      "unknown",
    ),
    rpoViolation: record.rpoState === "violated" || record.rpoViolation === true,
    verificationRequestedLevel: nullableNumber(record.verificationRequestedLevel),
    verificationState: oneOf(
      record.verificationState,
      ["not_configured", "unknown", "pending", "current"] as const,
      "unknown",
    ),
    verificationOverdue: record.verificationOverdue === true,
    planCount: nullableNumber(record.planCount),
    activePlanCount: nullableNumber(record.activePlanCount),
    retentionPolicyCount: nullableNumber(record.retentionPolicyCount),
    evidenceAtMs: nullableNumber(record.evidenceAtMs),
  };
}

function normalizeDatabaseObject(
  value: unknown,
  fallbackDiscoveryRunId: string,
): BackupScopeObjectPage["items"][number] {
  const record = requiredRecord(value);
  const engineObjectId = stringOrNull(record.engineObjectId);
  const objectName = stringOrNull(record.objectName);
  if (!engineObjectId || !objectName) {
    throw new ServiceError("APPLICATION_PAYLOAD_INVALID");
  }
  const rawEligibility = record.eligibility;
  if (
    rawEligibility !== "eligible" &&
    rawEligibility !== "excluded" &&
    rawEligibility !== "unsupported"
  ) {
    throw new ServiceError("APPLICATION_PAYLOAD_INVALID");
  }
  return {
    engineObjectId,
    schemaName: stringOrNull(record.schemaName),
    objectName,
    objectType: oneOf(
      record.objectType,
      [
        "table",
        "partitioned_table",
        "view",
        "materialized_view",
        "foreign_table",
        "unknown",
      ] as const,
      "unknown",
    ),
    eligibility: rawEligibility,
    exclusionReason: stringOrNull(record.exclusionReason),
    fingerprint: stringOrNull(record.fingerprint) ??
      stringOrNull(record.objectFingerprintBlake3) ??
      "",
    discoveryRunId: stringOrNull(record.discoveryRunId) ?? fallbackDiscoveryRunId,
  };
}

function requiredRecord(value: unknown): Record<string, unknown> {
  const record = recordOf(value);
  if (!record) throw new ServiceError("APPLICATION_PAYLOAD_INVALID");
  return record;
}

function normalizeCursorPageEnvelope<T>(value: unknown): CursorPage<T> {
  const record = requiredRecord(value);
  return {
    items: requiredArray(record.items) as T[],
    nextCursor: requiredCursor(record.nextCursor),
    totalCount: requiredCount(record.totalCount),
  };
}

function requiredArray(value: unknown): unknown[] {
  if (!Array.isArray(value)) {
    throw new ServiceError("APPLICATION_PAYLOAD_INVALID");
  }
  return value;
}

function requiredCursor(value: unknown): string | null {
  if (value === null) return null;
  if (typeof value === "string" && value.length > 0) return value;
  throw new ServiceError("APPLICATION_PAYLOAD_INVALID");
}

function requiredCount(value: unknown): number {
  if (!Number.isSafeInteger(value) || (value as number) < 0) {
    throw new ServiceError("APPLICATION_PAYLOAD_INVALID");
  }
  return value as number;
}

function recordOf(value: unknown): Record<string, unknown> | null {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? value as Record<string, unknown>
    : null;
}

function stringOrNull(value: unknown): string | null {
  return typeof value === "string" && value.length > 0 ? value : null;
}

function nullableNumber(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

function numberOr(value: unknown, fallback: number): number {
  return nullableNumber(value) ?? fallback;
}

function oneOf<const T extends readonly string[]>(
  value: unknown,
  allowed: T,
  fallback: T[number],
): T[number] {
  return typeof value === "string" && allowed.includes(value)
    ? value as T[number]
    : fallback;
}

function backupScopeState(value: unknown): BackupScope["state"] {
  return oneOf(
    value,
    ["active", "attention", "disabled", "retired"] as const,
    "attention",
  );
}

function backupScopeMode(value: unknown): BackupScope["mode"] {
  if (value === "whole_database" || value === "explicit_tables") return value;
  throw new ServiceError("APPLICATION_PAYLOAD_INVALID");
}

function scopeDiscoveryState(value: unknown): BackupScopeDiscoveryRun["state"] {
  return oneOf(
    value,
    ["queued", "running", "succeeded", "failed", "cancelled"] as const,
    "failed",
  );
}

async function serviceMutation<T>(
  command: MutationCommand,
  payload: object,
  idempotencyKey: string = createIdempotencyKey(),
): Promise<T> {
  return serviceRequest<T>(command, payload, idempotencyKey);
}

async function serviceRequest<T>(
  command: ServiceCommand,
  payload: object,
  idempotencyKey: string | null = null,
): Promise<T> {
  const scopedPayload = scopeServicePayload(command, payload);
  if (!inTauri()) return previewRequest<T>(command, scopedPayload);
  try {
    const outcome = await tauriInvoke<IpcOutcome<T>>("service_request", {
      command,
      payload: scopedPayload,
      idempotencyKey,
    });
    if (!outcome.success) {
      throw new ServiceError(outcome.error?.code ?? outcome.code, {
        requestId: outcome.requestId,
        retryable: outcome.error?.retryable ?? false,
      });
    }
    return outcome.result;
  } catch (error) {
    throw asServiceError(error);
  }
}

async function tauriInvoke<T>(
  command: string,
  args: Record<string, unknown>,
): Promise<T> {
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke<T>(command, args);
}

function inTauri(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

export function createIdempotencyKey(): string {
  if (typeof crypto !== "undefined" && "randomUUID" in crypto) {
    return crypto.randomUUID();
  }
  return `request-${Date.now()}-${Math.random().toString(16).slice(2)}`;
}

function storedWorkspaceId(): string {
  if (typeof window === "undefined") return DEFAULT_WORKSPACE_ID;
  const stored = window.localStorage.getItem(WORKSPACE_STORAGE_KEY);
  return stored && isCanonicalWorkspaceId(stored) ? stored : DEFAULT_WORKSPACE_ID;
}

function isCanonicalWorkspaceId(value: string): boolean {
  return /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(
    value,
  );
}

const previewWorkspaces: Workspace[] = [
  {
    id: DEFAULT_WORKSPACE_ID,
    displayName: "Default Workspace",
    state: "active",
    isDefault: true,
    createdAtMs: 0,
    updatedAtMs: 0,
    connectionCount: 2,
    databaseTargetCount: 3,
    destinationCount: 2,
    planCount: 1,
    activeWorkCount: 0,
    latestActivityAtMs: null,
  },
  {
    id: "11111111-1111-4111-8111-111111111111",
    displayName: "Finance Operations",
    state: "active",
    isDefault: false,
    createdAtMs: Date.now() - 86_400_000 * 24,
    updatedAtMs: Date.now() - 120_000,
    connectionCount: 3,
    databaseTargetCount: 7,
    destinationCount: 2,
    planCount: 3,
    activeWorkCount: 2,
    latestActivityAtMs: Date.now() - 120_000,
  },
  {
    id: "22222222-2222-4222-8222-222222222222",
    displayName: "Analytics Lab",
    state: "active",
    isDefault: false,
    createdAtMs: Date.now() - 86_400_000 * 5,
    updatedAtMs: Date.now() - 7_200_000,
    connectionCount: 1,
    databaseTargetCount: 2,
    destinationCount: 1,
    planCount: 0,
    activeWorkCount: 0,
    latestActivityAtMs: Date.now() - 7_200_000,
  },
];

async function previewRequest<T>(
  command: ServiceCommand,
  payload: object,
): Promise<T> {
  await Promise.resolve();
  const record = payload as Record<string, unknown>;
  let result: unknown;
  switch (command) {
    case "workspace.list":
      {
        const start = typeof record.cursor === "string"
          ? Number(record.cursor) || 0
          : 0;
        const limit = Math.min(
          100,
          Math.max(1, Number(record.limit) || INVENTORY_PAGE_LIMIT),
        );
        const items = previewWorkspaces
          .slice(start, start + limit)
          .map((workspace) => ({ ...workspace }));
        const next = start + items.length;
        result = {
          items,
          nextCursor: next < previewWorkspaces.length ? String(next) : null,
          totalCount: previewWorkspaces.length,
          complete: next >= previewWorkspaces.length,
        };
      }
      break;
    case "workspace.create": {
      const displayName = String(record.displayName ?? "").trim();
      const workspace: Workspace = {
        id: crypto.randomUUID(),
        displayName,
        state: "active",
        isDefault: false,
        createdAtMs: Date.now(),
        updatedAtMs: Date.now(),
        connectionCount: 0,
        databaseTargetCount: 0,
        destinationCount: 0,
        planCount: 0,
        activeWorkCount: 0,
        latestActivityAtMs: null,
      };
      previewWorkspaces.push(workspace);
      result = workspace;
      break;
    }
    case "service.health":
      result = previewServiceHealth();
      break;
    case "connection.list":
      result = previewConnectionPage();
      break;
    case "target.list":
      result = previewTargetPage(record);
      break;
    case "discovery.result_list":
      result = previewDiscoveryResultPage(record);
      break;
    case "backup_scope.list":
      result = previewBackupScopePage(record);
      break;
    case "backup_scope.object_list":
      result = previewBackupScopeObjectPage(record);
      break;
    case "destination.list":
      result = previewDestinationPage();
      break;
    case "plan.list":
      result = previewPlanPage();
      break;
    case "engine.matrix":
      result = previewEngineMatrix();
      break;
    case "activity.list":
      result = previewActivityPage(record);
      break;
    case "snapshot.list":
      result = previewSnapshotPage(record);
      break;
    case "snapshot.get":
      result = previewSnapshotDetailPage(record);
      break;
    case "catalog.rebuild_preview":
      result = previewCatalogRebuild(record);
      break;
    case "restore.preflight":
      result = previewRestorePreflight(record);
      break;
    case "settings.read":
      result = previewSettings();
      break;
    case "connection.save":
      result = previewSaveConnection(record);
      break;
    case "connection.probe":
      result = previewProbeConnection();
      break;
    case "backup_scope.discovery_run":
      result = previewBackupScopeDiscovery(record);
      break;
    case "backup_scope.save":
      result = previewSaveBackupScope(record);
      break;
    case "discovery.run":
      result = previewDiscovery();
      break;
    case "target.select":
      result = previewSelectTargets(record);
      break;
    case "destination.save":
      result = previewSaveDestination(record);
      break;
    case "destination.probe":
      result = previewProbeDestination(record);
      break;
    case "plan.save":
      result = {
        planId: `plan-preview-${Date.now()}`,
        revisionId: `revision-preview-${Date.now()}`,
      };
      break;
    case "plan.publish":
      result = {
        planId: String(record.planId),
        revisionId: String(record.revisionId),
        state: "enabled",
        publishedAtMs: Date.now(),
      };
      break;
    case "backup.enqueue":
      {
        const planId = String(record.planId);
        const occurrenceId = `occurrence-${crypto.randomUUID()}`;
        const requestSuffix = crypto.randomUUID();
      result = {
        schemaVersion: 1,
        planId,
        planRevisionId: "revision-production-hourly-7",
        planOccurrenceId: occurrenceId,
        planName: "Production hourly",
        state: "queued",
        workItems: [
          {
            backupRunId: `backup-run-orders-${requestSuffix}`,
            workItemId: `work-backup-${planId}-orders-${requestSuffix}`,
            targetId: "target-orders",
            targetName: "orders",
            engine: "postgresql",
            state: "queued",
          },
          {
            backupRunId: `backup-run-analytics-${requestSuffix}`,
            workItemId: `work-backup-${planId}-analytics-${requestSuffix}`,
            targetId: "target-analytics",
            targetName: "analytics",
            engine: "mysql",
            state: "queued",
          },
        ],
        activityAfterSeq: 100_000,
      };
      }
      break;
    case "work.cancel":
      result = previewBackupWorkStatus(
        String(record.workItemId),
        "cancelled",
        typeof record.fencingToken === "number" ? record.fencingToken : 0,
      );
      break;
    case "tool.rescan":
      result = previewProbeConnection();
      break;
    case "verification.enqueue":
      result = {
        schemaVersion: 1,
        verificationRunId: `verification-${crypto.randomUUID()}`,
        workItemId:
          record.level === 3 ? null : `work-verification-${crypto.randomUUID()}`,
        state: record.level === 3 ? "unsupported" : "queued",
        requestedLevel: record.level,
        sourceReplicaId: record.sourceReplicaId ?? "replica-orders-s3",
        supportStatus: record.level === 3 ? null : "passed",
        limitation:
          record.level === 3
            ? "L3는 등록된 restore-test target의 restore drill로 실행합니다."
            : null,
        errorCode:
          record.level === 3 ? "VERIFICATION_RESTORE_RUN_REQUIRED" : null,
        activityAfterSeq: 100_000,
      };
      break;
    case "confirmation.prepare":
      result = previewRestoreOverwriteConfirmation(record);
      break;
    case "restore.enqueue_safe":
    case "restore.enqueue_overwrite":
      result = {
        workItemId: `work-restore-${crypto.randomUUID()}`,
        state: "queued",
        activityAfterSeq: 100_000,
        restoreRunId: `restore-${crypto.randomUUID()}`,
        mode:
          command === "restore.enqueue_overwrite" ? "overwrite" : "create_new",
        purpose: record.purpose ?? "incident_recovery",
        verificationRunId:
          record.purpose === "restore_drill"
            ? `verification-${crypto.randomUUID()}`
            : null,
        targetFingerprint: "preview-target-fingerprint",
        preflightVersion: record.preflightVersion,
        cleanupPolicy: "preserve_target_and_require_explicit_cleanup",
      };
      break;
    case "repair.enqueue":
      result = {
        ...previewProbeConnection(),
        schemaVersion: 1,
        operationRunId: `operation-repair-${crypto.randomUUID()}`,
        operationKind: "replica_repair",
      };
      break;
    case "schema.status":
      result = {
        userVersion: 4,
        applicationTableCount: 67,
        journalMode: "wal",
        synchronous: "full",
        foreignKeys: true,
        busyTimeoutMs: 5_000,
        diagnosticCode: null,
      };
      break;
    case "work.get": {
      const workItemId =
        typeof record.workItemId === "string"
          ? record.workItemId
          : previewProbeConnection().workItemId;
      if (workItemId.startsWith("work-backup-")) {
        result = previewBackupWorkStatus(workItemId, "running", 7);
        break;
      }
      result = {
        workItemId,
        state: "succeeded",
        activityAfterSeq: 100_000,
        kind: "operation",
        currentAttemptNo: 1,
        fencingToken: 1,
        waitReason: null,
        operationRunId: `operation-${workItemId}`,
        operationKind: workItemId.includes("restore")
          ? "restore"
          : "connection_discovery",
        result: workItemId.includes("restore")
          ? {
              validation: "passed",
              cleanupRequired: false,
              measuredRtoMs: 48_200,
            }
          : null,
        errorCode: null,
        discoveredCount: previewDiscovery().discovered.length,
        eligibleDiscoveredCount: previewDiscovery().discovered.filter(
          (item) => item.eligibility === "eligible",
        ).length,
      };
      break;
    }
    case "settings.update":
      result = { updated: true };
      break;
    default:
      throw new ServiceError("APPLICATION_COMMAND_UNKNOWN");
  }
  return result as T;
}

function previewBackupWorkStatus(
  workItemId: string,
  state: "running" | "cancelled",
  fencingToken: number,
): WorkItemStatus {
  const now = Date.now();
  const analytics = workItemId.includes("-analytics-");
  return {
    workItemId,
    state,
    activityAfterSeq: 100_000,
    kind: "backup",
    currentAttemptNo: 1,
    fencingToken,
    waitReason: null,
    operationRunId: null,
    operationKind: null,
    result: null,
    errorCode: null,
    discoveredCount: 0,
    eligibleDiscoveredCount: 0,
    discovered: [],
    subject: {
      planId: "plan-production-hourly",
      planName: "Production hourly",
      targetId: analytics ? "target-analytics" : "target-orders",
      targetName: analytics ? "analytics" : "orders",
      engine: analytics ? "mysql" : "postgresql",
    },
    progress:
      state === "running"
        ? analytics
          ? {
              phase: "replicating",
              completedUnits: null,
              totalUnits: null,
              unit: "objects",
              currentObject: null,
              updatedAtMs: now,
            }
          : {
            phase: "dumping",
            completedUnits: 384,
            totalUnits: 1_024,
            unit: "mib",
            currentObject: "public.orders",
            updatedAtMs: now,
          }
        : {
            phase: "cancelled",
            completedUnits: null,
            totalUnits: null,
            unit: "steps",
            currentObject: null,
            updatedAtMs: now,
          },
    createdAtMs: now - 12_000,
    updatedAtMs: now,
  };
}
