export type PageId =
  | "workspaces"
  | "overview"
  | "connections"
  | "databases"
  | "destinations"
  | "backup-plans"
  | "activity"
  | "snapshots"
  | "recovery"
  | "settings";

export interface RouteState {
  page: PageId;
  filter?: string;
  cursor?: string;
  selected?: string;
  target?: string;
  scope?: string;
  action?: string;
}

export interface CursorPage<T> {
  items: T[];
  nextCursor: string | null;
  totalCount: number;
}

export interface Workspace {
  id: string;
  displayName: string;
  state: "active" | "archived";
  isDefault: boolean;
  createdAtMs: number;
  updatedAtMs: number;
  connectionCount: number;
  databaseTargetCount: number;
  destinationCount: number;
  planCount: number;
  activeWorkCount: number;
  latestActivityAtMs: number | null;
}

export interface ServiceHealth {
  bootId: string;
  phase:
    | "starting"
    | "rehearsal_ready"
    | "production_ready"
    | "diagnostics_only"
    | "draining"
    | "stopped";
  schemaVersion: number | null;
  applicationTableCount: number | null;
  executionAuthority: "legacy_direct" | "service";
  cutoverMode: "rehearsal_only" | "production";
  cutoverMarker: string | null;
  recoveryDisposition: "rollback_eligible" | "forward_fix_required" | null;
  claimsEnabled: boolean;
  acceptsIpc: boolean;
  diagnosticCode: string | null;
  scheduler: SchedulerRuntimeHealth;
  retention: RetentionRuntimeHealth;
}

export interface SchedulerRuntimeHealth {
  state: "unknown" | "not_configured" | "awaiting_first_tick" | "partial" | "attention" | "observed";
  configuredPlanCount: number;
  observedCursorCount: number;
  activeWorkCount: number;
  errorPlanCount: number;
  lastTickAtMs: number | null;
  lastMaterializedForMs: number | null;
  lastErrorCode: string | null;
  lastErrorAtMs: number | null;
  evidenceAtMs: number | null;
}

export interface RetentionRuntimeHealth {
  state: "unknown" | "not_configured" | "awaiting_first_scan" | "attention" | "observed";
  configuredPolicyCount: number;
  activeTombstoneCount: number;
  queuedDeleteCount: number;
  failedOperationCount: number;
  lastScanAtMs: number | null;
  lastErrorCode: string | null;
  lastErrorAtMs: number | null;
  evidenceAtMs: number | null;
}

export interface EngineCapability {
  capability:
    | "probe"
    | "discovery"
    | "tool_preflight"
    | "backup"
    | "verify_l1"
    | "verify_l2"
    | "restore_create_new"
    | "validation"
    | "restore_l3";
  supportStatus: "passed" | "limited" | "failed" | "not_applicable" | "not_run";
  limitation: string | null;
  evidenceLocator: string | null;
}

export interface EngineMatrixRow {
  certificationId: string | null;
  engine: string;
  serverVersionRange: string | null;
  toolName: string | null;
  toolVersionRange: string | null;
  operatingSystem: string | null;
  architecture: string | null;
  dumpFormat: string | null;
  consistencyGrade: string | null;
  supportStatus:
    | "supported"
    | "limited"
    | "candidate"
    | "experimental"
    | "unsupported"
    | "suspended"
    | "expired"
    | "unavailable";
  evidenceLocator: string | null;
  evidenceChecksumBlake3: string | null;
  certifiedAtMs: number | null;
  expiresAtMs: number | null;
  capabilities: EngineCapability[];
}

export interface EngineMatrix {
  generatedAtMs: number;
  certificationTotalCount: number;
  complete: boolean;
  rows: EngineMatrixRow[];
}

export interface DatabaseTarget {
  id: string;
  connectionId: string;
  connectionName: string;
  engine: string;
  displayName: string;
  state: "active" | "attention" | "disabled" | "retired";
  missingSinceMs: number | null;
  sourceFingerprint: string;
  latestRecoveryPointId: string | null;
  latestSuccessAtMs: number | null;
  latestFailureAtMs: number | null;
  artifactCompleteness: "complete" | "incomplete" | "unknown";
  replicaDurability: "healthy" | "degraded" | "unavailable" | "unknown";
  protectionScope: "whole_database" | "partial" | "none" | "unknown";
  protectionCompliance: "compliant" | "partial" | "non_compliant" | "not_configured" | "unknown";
  rpoViolation: boolean;
  verificationState: "not_configured" | "unknown" | "pending" | "current";
  verificationOverdue: boolean;
  evidenceAtMs: number | null;
}

export type BackupScopeMode = "whole_database" | "explicit_tables";

export interface RetentionPolicy {
  daily: number;
  weekly: number;
  monthly: number;
}

export interface BackupScope {
  id: string;
  databaseTargetId: string;
  databaseName: string;
  connectionId: string;
  connectionName: string;
  engine: string;
  displayName: string;
  mode: BackupScopeMode;
  state: "active" | "attention" | "disabled" | "retired";
  selectedObjectCount: number | null;
  eligibleObjectCount: number | null;
  newObjectCount: number | null;
  missingObjectCount: number | null;
  changedObjectCount: number | null;
  driftState: "unknown" | "current" | "drifted";
  sourceDrift: boolean;
  discoveryRunId: string | null;
  inventoryHashBlake3: string | null;
  inventoryObservedAtMs: number | null;
  latestBackupState: string | null;
  latestRecoveryPointId: string | null;
  latestSuccessAtMs: number | null;
  latestFailureAtMs: number | null;
  artifactCompleteness: "complete" | "incomplete" | "unknown";
  replicaDurability: "healthy" | "degraded" | "unavailable" | "unknown";
  protectionCompliance: "compliant" | "non_compliant" | "not_configured" | "unknown";
  restoreEligibility: "eligible" | "conditional" | "blocked" | "unknown";
  latestVerifiedLevel: number | null;
  lastVerifiedAtMs: number | null;
  recoveryEvidenceAtMs: number | null;
  rpoMs: number | null;
  rpoState: "not_configured" | "unknown" | "current" | "violated";
  rpoViolation: boolean;
  verificationRequestedLevel: number | null;
  verificationState: "not_configured" | "unknown" | "pending" | "current";
  verificationOverdue: boolean;
  planCount: number | null;
  activePlanCount: number | null;
  retentionPolicyCount: number | null;
  evidenceAtMs: number | null;
}

export interface DiscoveredDatabaseObject {
  engineObjectId: string;
  schemaName: string | null;
  objectName: string;
  objectType:
    | "table"
    | "partitioned_table"
    | "view"
    | "materialized_view"
    | "foreign_table"
    | "unknown";
  eligibility: "eligible" | "excluded" | "unsupported";
  exclusionReason: string | null;
  fingerprint: string;
  discoveryRunId: string;
}

export interface BackupScopeObjectPage {
  items: DiscoveredDatabaseObject[];
  nextCursor: string | null;
  totalCount: number;
  eligibleCount: number;
  discoveryRunId: string;
  inventoryHashBlake3: string;
  observedAtMs: number;
}

export interface BackupScopeDiscoveryRun {
  operationRunId: string;
  workItemId: string;
  state: "queued" | "running" | "succeeded" | "failed" | "cancelled";
}

export interface BackupScopeSaveRequest {
  databaseTargetId: string;
  displayName: string;
  mode: BackupScopeMode;
  discoveryRunId?: string;
  engineObjectIds: string[];
  replacesScopeId?: string;
}

export interface BackupScopeSaveResult {
  scopeId: string;
  databaseTargetId: string;
  mode: BackupScopeMode;
  state: "active" | "attention" | "disabled" | "retired";
  selectedObjectCount: number | null;
  selectorHashBlake3: string;
  sourceFingerprint: string;
  replacedScopeId?: string;
}

export interface ConnectionProfile {
  id: string;
  displayName: string;
  engine: string;
  transport: "tcp" | "unix_socket" | "file";
  endpoint: string;
  state: "draft" | "ready" | "attention" | "disabled";
  lastProbeStatus: "passed" | "failed" | "cancelled" | null;
  lastProbeAtMs: number | null;
  restoreTestTarget: boolean;
  updatedAtMs: number;
  targetCount: number;
  /** Compatibility-only. Production inventory is loaded with target.list. */
  targets: DatabaseTarget[];
}

export interface DestinationRole {
  role: "backup_writer" | "restore_reader" | "delete_operator";
  credentialBound: boolean;
  probeStatus: "running" | "passed" | "failed" | "cancelled" | null;
  probedAtMs: number | null;
}

export interface Destination {
  id: string;
  displayName: string;
  kind: "local" | "s3" | "ssh_sftp" | "azure_blob" | "smb" | "rclone";
  endpoint: string;
  state: "draft" | "ready" | "attention" | "disabled";
  immutabilityCapability: "none" | "governance" | "compliance" | "server_append_only";
  lastProbeStatus: "passed" | "failed" | "cancelled" | null;
  lastProbeAtMs: number | null;
  updatedAtMs: number;
  roles: DestinationRole[];
}

export interface S3RoleCredential {
  schemaVersion: 1;
  accessKeyId: string;
  secretAccessKey: string;
}

export interface SshRoleCredential {
  schemaVersion: 1;
  auth: {
    kind: "password";
    password: string;
  };
}

export type DestinationDraft =
  | {
      displayName: string;
      kind: "local";
      endpoint: { path: string };
      credentials: Record<string, never>;
    }
  | {
      displayName: string;
      kind: "s3";
      endpoint: {
        bucket: string;
        prefix: string;
        region: string;
        endpointUrl?: string;
        forcePathStyle: boolean;
        provider: "aws" | "minio" | "backblaze_b2" | "cloudflare_r2";
        objectLockMode: "none" | "governance" | "compliance";
        retentionDays: number;
        minDwellDays: number;
        checksumAlgo?: string;
        complianceOptIn: boolean;
      };
      credentials: {
        backupWriter: S3RoleCredential;
        restoreReader: S3RoleCredential;
        deleteOperator?: S3RoleCredential;
      };
    }
  | {
      displayName: string;
      kind: "ssh_sftp";
      endpoint: {
        host: string;
        port: number;
        user: string;
        path: string;
        hostKeyFingerprint: string;
        appendOnly: boolean;
      };
      credentials: {
        backupWriter: SshRoleCredential;
        restoreReader: SshRoleCredential;
        deleteOperator?: SshRoleCredential;
      };
    };

export interface DestinationSaveResult {
  destinationId: string;
  state: "draft";
}

export interface BackupPlan {
  id: string;
  displayName: string;
  state: "draft" | "enabled" | "paused" | "attention" | "retired";
  revisionId: string | null;
  revisionNo: number | null;
  scheduleKind: "cron" | "interval" | "manual" | null;
  scheduleExpression: string | null;
  timezone: string | null;
  rpoMs: number | null;
  publishedAtMs: number | null;
  targetCount: number;
  destinationCount: number;
  requiredDestinationCount: number;
  updatedAtMs: number;
}

export interface BackupPlanSaveRequest {
  displayName: string;
  scopeIds: string[];
  destinations: Array<{
    destinationId: string;
    requirement: "required" | "best_effort";
  }>;
  schedule: {
    kind: "cron";
    expression: string;
    timezone: string;
  };
  rpoMs: number;
  retention: RetentionPolicy;
  verification: {
    requestedLevel: 1 | 2 | 3;
  };
}

export interface ActivityEvent {
  seq: number;
  id: string;
  occurredAtMs: number;
  severity: "debug" | "info" | "warning" | "error" | "critical";
  eventCode: string;
  phase: string | null;
  entityType: string;
  entityId: string;
  correlationId: string | null;
  workItemId: string | null;
  attemptId: string | null;
  retryability: "retryable" | "terminal" | "unknown" | null;
  message: string;
}

export interface WorkItemRef {
  workItemId: string;
  state: string;
  activityAfterSeq: number;
}

export interface BackupEnqueueWorkItem {
  backupRunId: string;
  workItemId: string;
  targetId: string;
  targetName: string;
  engine: string;
  state: "queued";
}

export interface BackupEnqueueResult {
  schemaVersion: 1;
  planId: string;
  planRevisionId: string;
  planOccurrenceId: string;
  planName: string;
  state: "queued";
  workItems: BackupEnqueueWorkItem[];
  activityAfterSeq: number;
}

export interface WorkItemStatus extends WorkItemRef {
  kind: string;
  currentAttemptNo: number;
  fencingToken: number;
  waitReason: string | null;
  operationRunId: string | null;
  operationKind: string | null;
  result: Record<string, unknown> | null;
  errorCode: string | null;
  discoveredCount: number;
  eligibleDiscoveredCount: number;
  /** Browser-fixture compatibility; work.get never returns discovery rows in production. */
  discovered?: DiscoveredDatabase[];
  subject?: WorkItemSubject | null;
  progress?: WorkItemProgress | null;
  createdAtMs?: number | null;
  updatedAtMs?: number | null;
}

export interface WorkItemSubject {
  planId: string | null;
  planName: string | null;
  targetId: string | null;
  targetName: string | null;
  engine: string | null;
}

export interface WorkItemProgress {
  phase: string;
  completedUnits: number | null;
  totalUnits: number | null;
  unit: "bytes" | "rows" | "objects" | "steps" | "mib";
  currentObject: string | null;
  updatedAtMs: number | null;
}

export interface DiscoveredDatabase {
  engineObjectId: string;
  databaseName: string;
  eligibility: "eligible" | "excluded" | "offline" | "unknown";
  exclusionReason: string | null;
}

export interface ConnectionDraft {
  displayName: string;
  engine: string;
  transport: "tcp" | "file";
  host: string;
  port: number;
  filePath: string;
  tlsMode: "disabled" | "preferred" | "required" | "verify_ca" | "verify_full";
  username: string;
  password: string;
  restoreTestTarget: boolean;
}

export interface ConnectionSaveResult {
  connectionId: string;
  state: "draft";
}

export interface DiscoveryResult extends WorkItemRef {
  discoveryRunId: string;
  discovered: DiscoveredDatabase[];
}

export interface TargetSelectionResult {
  targets: DatabaseTarget[];
}

export interface PlanPublishResult {
  planId: string;
  revisionId: string;
  state: "enabled";
  publishedAtMs: number;
}

export interface RuntimePool {
  id: string;
  resourceType: string;
  resourceKey: string;
  capacity: number;
  inUse: number;
  enabled: boolean;
}

export interface SpoolVolume {
  id: string;
  rootPath: string;
  state: "ready" | "throttled" | "full" | "offline";
  hardLimitBytes: number;
  softLimitBytes: number;
  freeBytes: number | null;
  probedAtMs: number | null;
}

export interface ToolInventoryItem {
  id: string;
  name: string;
  version: string;
  architecture: string;
  trustStatus: "trusted" | "blocked" | "unreviewed";
  signatureStatus: "valid" | "invalid" | "unsigned" | "not_checked";
  verifiedAtMs: number | null;
}

export interface NotificationChannel {
  id: string;
  displayName: string;
  kind: string;
  state: string;
  lastTestStatus: string | null;
  lastTestAtMs: number | null;
}

export interface SettingsData {
  resourcePools: RuntimePool[];
  spoolVolumes: SpoolVolume[];
  tools: ToolInventoryItem[];
  notificationChannels: NotificationChannel[];
  controlBackup: {
    lastCompletedAtMs: number | null;
    lastStatus: "succeeded" | "failed" | "unknown";
  };
  inventoryCounts: {
    resourcePools: number;
    spoolVolumes: number;
    tools: number;
    notificationChannels: number;
  };
  complete: boolean;
}

export interface Snapshot {
  id: string;
  sourceTargetIdentity: string;
  sourceDisplayName: string;
  sourceConnectionId: string | null;
  sourceConnectionName: string | null;
  engine: string;
  state: "complete" | "incomplete" | "orphaned" | "quarantined";
  committedAtMs: number | null;
  manifestSchemaVersion: number;
  setManifestChecksumBlake3: string;
  signingKeyId: string;
  artifactCompleteness: "complete" | "incomplete";
  replicaDurability: "healthy" | "degraded" | "unavailable";
  protectionCompliance: "compliant" | "non_compliant" | "unknown";
  restoreEligibility: "eligible" | "conditional" | "blocked";
  highestVerifiedLevel: 1 | 2 | 3 | null;
  lastVerifiedAtMs: number | null;
  signatureStatus: "valid" | "invalid" | "unknown_key" | "not_checked";
  requiredPartCount: number;
  presentPartCount: number;
  plaintextBytes: number | null;
  cipherBytes: number | null;
  firstIngestedAtMs: number;
  lastIngestedAtMs: number;
  diagnosticReason: string | null;
  partCount: number;
  replicaCount: number;
  verificationEvidenceCount: number;
  restoreDrillCount: number;
}

export interface SnapshotReplica {
  id: string;
  destinationFingerprint: string;
  destinationKind: string;
  requirement: "required" | "best_effort" | "unknown";
  state: "committed" | "degraded" | "missing" | "corrupt" | "quarantined";
  immutabilityCapability: string;
  retentionUntilMs: number | null;
  readerStatus: "eligible" | "unavailable" | "unverified";
  observedAtMs: number;
}

export interface SnapshotPart {
  id: string;
  role: string;
  logicalName: string;
  required: boolean;
  dumpFormat: string;
  plaintextBytes: number | null;
  checksumBlake3: string | null;
}

export interface VerificationEvidence {
  id: string;
  level: number;
  result: string;
  verifiedAtMs: number;
  toolName: string | null;
  toolVersion: string | null;
  evidenceChecksumBlake3: string | null;
  evidenceLocator: string | null;
  evidenceAvailable: boolean;
}

export interface RestoreDrillEvidence {
  restoreRunId: string;
  verificationRunId: string;
  targetConnectionId: string;
  targetConnectionName: string;
  targetName: string;
  state: string;
  measuredRtoMs: number | null;
  startedAtMs: number | null;
  completedAtMs: number | null;
  errorCode: string | null;
  evidenceAvailable: boolean;
}

export type RestoreMode = "create_new" | "overwrite";
export type RestorePurpose = "incident_recovery" | "restore_drill";

export interface RestorePreflightRequest {
  snapshotId: string;
  mode: RestoreMode;
  purpose: RestorePurpose;
  targetConnectionId: string;
  targetName: string;
  sourceReplicaId?: string;
}

export interface RestorePreflightCheck {
  key: string;
  state: "passed" | "warning" | "failed" | "unknown";
  detail: string;
  remediation: string;
}

export interface RestorePreflightResult {
  schemaVersion: 1;
  preflightVersion: string;
  evaluatedAtMs: number;
  expiresAtMs: number;
  eligible: boolean;
  mode: RestoreMode;
  purpose: RestorePurpose;
  source: {
    snapshotId: string;
    replicaId: string;
    displayName: string;
    engine: string;
    restoreEligibility: Snapshot["restoreEligibility"];
  };
  targetConnectionId: string;
  targetConnectionName: string;
  targetName: string;
  targetFingerprint: string;
  targetExists: boolean | null;
  restoreTestTarget: boolean;
  targetRole: "restore_test" | "general";
  serverVersion: string | null;
  requiredSpoolBytes: number | null;
  availableSpoolBytes: number | null;
  checks: RestorePreflightCheck[];
}

export interface RestoreEnqueueRequest {
  snapshotId: string;
  sourceReplicaId: string;
  targetConnectionId: string;
  targetName: string;
  preflightVersion: string;
  requiredVerificationLevel: 1 | 2 | 3;
  purpose: RestorePurpose;
}

export interface RestoreEnqueueResult extends WorkItemRef {
  restoreRunId: string;
  mode: RestoreMode;
  purpose?: RestorePurpose;
  verificationRunId?: string | null;
  targetFingerprint: string;
  preflightVersion: string;
  cleanupPolicy?: string;
}

export interface RestoreOverwritePrepareRequest {
  action: "restore_overwrite";
  snapshotId: string;
  sourceReplicaId: string;
  targetConnectionId: string;
  targetName: string;
  confirmedTargetName: string;
  preflightVersion: string;
  requiredVerificationLevel: 1 | 2 | 3;
  ttlMs: number;
}

export interface RestoreOverwriteConfirmation {
  schemaVersion: 1;
  confirmationTokenId: string;
  confirmationToken: string | null;
  action: "restore_overwrite";
  issuedAtMs: number;
  expiresAtMs: number;
  tokenAvailable: boolean;
  payloadDigestBlake3: string;
  finalSummary: {
    source: { snapshotId: string; replicaId: string };
    target: { connectionId: string; name: string; fingerprint: string };
    mode: "overwrite";
    destructiveEffect: string;
    cleanupPolicy: string;
    preflightVersion: string;
    requiredVerificationLevel: number;
  };
  overwriteRequest: Record<string, unknown> | null;
}

export interface VerificationEnqueueResult {
  schemaVersion: 1;
  verificationRunId: string;
  workItemId: string | null;
  state: "queued" | "unsupported";
  requestedLevel: 1 | 2 | 3;
  sourceReplicaId: string;
  supportStatus: string | null;
  limitation: string | null;
  errorCode: string | null;
  activityAfterSeq: number;
}

export interface ReplicaRepairResult extends WorkItemRef {
  schemaVersion: 1;
  operationRunId: string;
  operationKind: "replica_repair";
}

export interface CatalogRebuildPreviewCandidate {
  objectKey: string;
  disposition: "accepted" | "rejected" | "duplicate" | "unknown_key";
  reasonCode: string | null;
  signingKeyId: string | null;
  recoveryPointId: string | null;
  manifestChecksumBlake3: string | null;
}

export interface CatalogRebuildPreview {
  schemaVersion: 1;
  available: boolean;
  destinationId: string;
  destinationFingerprint?: string;
  observedAtMs?: number;
  totalCount?: number;
  acceptedCount?: number;
  rejectedCount?: number;
  duplicateCount?: number;
  unknownKeyCount?: number;
  reasonCode: string | null;
  candidates: CatalogRebuildPreviewCandidate[];
}
