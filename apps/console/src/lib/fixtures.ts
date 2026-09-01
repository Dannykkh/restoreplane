import type {
  ActivityEvent,
  BackupScope,
  BackupScopeDiscoveryRun,
  BackupScopeObjectPage,
  BackupScopeSaveResult,
  BackupPlan,
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
  DiscoveryResult,
  EngineCapability,
  EngineMatrix,
  EngineMatrixRow,
  RestoreOverwriteConfirmation,
  RestorePreflightResult,
  ServiceHealth,
  SettingsData,
  Snapshot,
  SnapshotPart,
  SnapshotReplica,
  VerificationEvidence,
  RestoreDrillEvidence,
  TargetSelectionResult,
  WorkItemRef,
} from "./types";
import type {
  SnapshotDetailSection,
  SnapshotSectionPage,
} from "./snapshotInventory";
import { ServiceError } from "./errors";

const minute = 60_000;
const hour = 60 * minute;
const day = 24 * hour;
const ACTIVITY_TOTAL = 100_000;
const ACTIVITY_OBSERVED_AT_MS = Date.now();

function observedNow(): number {
  return Date.now();
}

function target(
  id: string,
  connectionId: string,
  connectionName: string,
  engine: string,
  displayName: string,
  compliance: "compliant" | "non_compliant" | "not_configured" | "unknown",
  options: {
    successAge?: number;
    failureAge?: number;
    rpoViolation?: boolean;
    verificationOverdue?: boolean;
    verificationState?: "not_configured" | "unknown" | "pending" | "current";
    durability?: "healthy" | "degraded" | "unavailable" | "unknown";
    state?: "active" | "attention" | "disabled" | "retired";
    protectionScope?: "whole_database" | "partial" | "none" | "unknown";
  } = {},
) {
  const now = observedNow();
  return {
    id,
    connectionId,
    connectionName,
    engine,
    displayName,
    state: options.state ?? "active",
    missingSinceMs: null,
    sourceFingerprint: `${engine}:${connectionId}:${displayName}`,
    latestRecoveryPointId: options.successAge ? `rp-${id}-latest` : null,
    latestSuccessAtMs: options.successAge ? now - options.successAge : null,
    latestFailureAtMs: options.failureAge ? now - options.failureAge : null,
    artifactCompleteness: options.successAge ? ("complete" as const) : ("unknown" as const),
    replicaDurability: options.durability ?? (options.successAge ? ("healthy" as const) : ("unknown" as const)),
    protectionScope: options.protectionScope ?? (compliance === "not_configured" ? "none" as const : "whole_database" as const),
    protectionCompliance: compliance,
    rpoViolation: options.rpoViolation ?? false,
    verificationState: options.verificationState ?? (
      options.successAge
        ? "current"
        : compliance === "not_configured"
          ? "not_configured"
          : "unknown"
    ),
    verificationOverdue: options.verificationOverdue ?? false,
    evidenceAtMs: options.successAge ? now - Math.min(options.successAge, 30 * minute) : null,
  };
}

const previewConnections: ConnectionProfile[] = [
  {
    id: "connection-prod-pg",
    displayName: "Production PostgreSQL",
    engine: "postgresql",
    transport: "tcp",
    endpoint: "pg-primary.internal:5432",
    state: "ready",
    lastProbeStatus: "passed",
    lastProbeAtMs: observedNow() - 12 * minute,
    restoreTestTarget: false,
    updatedAtMs: observedNow() - 12 * minute,
    targetCount: 3,
    targets: [
      target(
        "target-orders",
        "connection-prod-pg",
        "Production PostgreSQL",
        "postgresql",
        "orders",
        "compliant",
        { successAge: 42 * minute, verificationState: "pending" },
      ),
      target(
        "target-customer-analytics",
        "connection-prod-pg",
        "Production PostgreSQL",
        "postgresql",
        "고객 행동 분석 장기 보존 데이터베이스",
        "non_compliant",
        {
          successAge: 27 * hour,
          failureAge: 3 * hour,
          rpoViolation: true,
          verificationState: "unknown",
          durability: "degraded",
          state: "attention",
          protectionScope: "partial",
        },
      ),
      target(
        "target-ledger",
        "connection-prod-pg",
        "Production PostgreSQL",
        "postgresql",
        "ledger",
        "compliant",
        { successAge: 2 * hour, verificationOverdue: true },
      ),
    ],
  },
  {
    id: "connection-billing-mysql",
    displayName: "Billing MySQL",
    engine: "mysql",
    transport: "tcp",
    endpoint: "mysql-billing.internal:3306",
    state: "attention",
    lastProbeStatus: "failed",
    lastProbeAtMs: observedNow() - 48 * minute,
    restoreTestTarget: false,
    updatedAtMs: observedNow() - 48 * minute,
    targetCount: 2,
    targets: [
      target(
        "target-billing",
        "connection-billing-mysql",
        "Billing MySQL",
        "mysql",
        "billing",
        "compliant",
        { successAge: 3 * hour },
      ),
      target(
        "target-invoice-archive",
        "connection-billing-mysql",
        "Billing MySQL",
        "mysql",
        "invoice_archive",
        "not_configured",
      ),
    ],
  },
  {
    id: "connection-reporting-maria",
    displayName: "Reporting MariaDB",
    engine: "mariadb",
    transport: "tcp",
    endpoint: "maria-reporting.internal:3306",
    state: "draft",
    lastProbeStatus: null,
    lastProbeAtMs: null,
    restoreTestTarget: false,
    updatedAtMs: observedNow() - day,
    targetCount: 0,
    targets: [],
  },
  {
    id: "connection-restore-lab-pg",
    displayName: "PostgreSQL Restore Lab",
    engine: "postgresql",
    transport: "tcp",
    endpoint: "pg-restore-lab.internal:5432",
    state: "ready",
    lastProbeStatus: "passed",
    lastProbeAtMs: observedNow() - 2 * minute,
    restoreTestTarget: true,
    updatedAtMs: observedNow() - 2 * minute,
    targetCount: 0,
    targets: [],
  },
  {
    id: "connection-finance-sqlserver",
    displayName: "Finance SQL Server",
    engine: "sqlserver",
    transport: "tcp",
    endpoint: "sql-finance.internal:1433",
    state: "ready",
    lastProbeStatus: "passed",
    lastProbeAtMs: observedNow() - 8 * minute,
    restoreTestTarget: false,
    updatedAtMs: observedNow() - 8 * minute,
    targetCount: 1,
    targets: [
      target(
        "target-finance-warehouse",
        "connection-finance-sqlserver",
        "Finance SQL Server",
        "sqlserver",
        "finance_warehouse",
        "not_configured",
      ),
    ],
  },
  {
    id: "connection-local-sqlite",
    displayName: "Local Operations SQLite",
    engine: "sqlite",
    transport: "file",
    endpoint: "D:\\Data\\operations.sqlite3",
    state: "ready",
    lastProbeStatus: "passed",
    lastProbeAtMs: observedNow() - 3 * minute,
    restoreTestTarget: false,
    updatedAtMs: observedNow() - 3 * minute,
    targetCount: 1,
    targets: [
      target(
        "target-local-operations",
        "connection-local-sqlite",
        "Local Operations SQLite",
        "sqlite",
        "operations.sqlite3",
        "not_configured",
      ),
    ],
  },
];

const previewBackupScopes: BackupScope[] = [
  backupScopeFromTarget("target-orders", {
    id: "target-orders",
    displayName: "orders 전체 백업",
    mode: "whole_database",
    selectedObjectCount: null,
    eligibleObjectCount: 3,
    newObjectCount: 0,
    missingObjectCount: 0,
    inventoryHashBlake3: "7".repeat(64),
    inventoryObservedAtMs: observedNow() - 8 * minute,
    planCount: 1,
    retentionPolicyCount: 1,
  }),
  backupScopeFromTarget("target-orders", {
    id: "scope-orders-iwrs",
    displayName: "orders IWRS 선택 백업",
    mode: "explicit_tables",
    state: "attention",
    selectedObjectCount: 1,
    eligibleObjectCount: 3,
    newObjectCount: 1,
    missingObjectCount: 0,
    changedObjectCount: 0,
    driftState: "drifted",
    discoveryRunId: "discovery-orders-latest",
    inventoryHashBlake3: "8".repeat(64),
    inventoryObservedAtMs: observedNow() - 8 * minute,
    protectionCompliance: "non_compliant",
    planCount: 0,
    retentionPolicyCount: 0,
  }),
  backupScopeFromTarget("target-billing", {
    id: "target-billing",
    displayName: "billing 전체 백업",
    mode: "whole_database",
    selectedObjectCount: null,
    eligibleObjectCount: 12,
    newObjectCount: 0,
    missingObjectCount: 0,
    inventoryHashBlake3: "6".repeat(64),
    inventoryObservedAtMs: observedNow() - 12 * minute,
    planCount: 1,
    retentionPolicyCount: 1,
  }),
];

function backupScopeFromTarget(
  targetId: string,
  overrides: Pick<
    BackupScope,
    | "id"
    | "displayName"
    | "mode"
    | "selectedObjectCount"
    | "eligibleObjectCount"
    | "newObjectCount"
    | "missingObjectCount"
    | "planCount"
    | "retentionPolicyCount"
  > & Partial<BackupScope>,
): BackupScope {
  const databaseTarget = previewConnections
    .flatMap((connection) => connection.targets)
    .find((candidate) => candidate.id === targetId);
  if (!databaseTarget) throw new Error(`Unknown fixture database target: ${targetId}`);
  return {
    id: overrides.id,
    databaseTargetId: databaseTarget.id,
    databaseName: databaseTarget.displayName,
    connectionId: databaseTarget.connectionId,
    connectionName: databaseTarget.connectionName,
    engine: databaseTarget.engine,
    displayName: overrides.displayName,
    mode: overrides.mode,
    state: overrides.state ?? databaseTarget.state,
    selectedObjectCount: overrides.selectedObjectCount,
    eligibleObjectCount: overrides.eligibleObjectCount,
    newObjectCount: overrides.newObjectCount,
    missingObjectCount: overrides.missingObjectCount,
    changedObjectCount: overrides.changedObjectCount ?? 0,
    driftState: overrides.driftState ?? (overrides.inventoryObservedAtMs ? "current" : "unknown"),
    sourceDrift: overrides.sourceDrift ?? false,
    discoveryRunId: overrides.discoveryRunId ?? (overrides.inventoryObservedAtMs ? `discovery-${targetId}` : null),
    inventoryHashBlake3: overrides.inventoryHashBlake3 ?? null,
    inventoryObservedAtMs: overrides.inventoryObservedAtMs ?? null,
    latestBackupState: overrides.latestBackupState ?? (databaseTarget.latestSuccessAtMs ? "succeeded" : null),
    latestRecoveryPointId: overrides.latestRecoveryPointId ?? databaseTarget.latestRecoveryPointId,
    latestSuccessAtMs: overrides.latestSuccessAtMs ?? databaseTarget.latestSuccessAtMs,
    latestFailureAtMs: overrides.latestFailureAtMs ?? databaseTarget.latestFailureAtMs,
    artifactCompleteness: overrides.artifactCompleteness ?? databaseTarget.artifactCompleteness,
    replicaDurability: overrides.replicaDurability ?? databaseTarget.replicaDurability,
    protectionCompliance: overrides.protectionCompliance ?? (
      databaseTarget.protectionCompliance === "partial"
        ? "unknown"
        : databaseTarget.protectionCompliance
    ),
    restoreEligibility: overrides.restoreEligibility ?? (databaseTarget.latestRecoveryPointId ? "eligible" : "unknown"),
    latestVerifiedLevel: overrides.latestVerifiedLevel ?? (databaseTarget.latestRecoveryPointId ? 2 : null),
    lastVerifiedAtMs: overrides.lastVerifiedAtMs ?? databaseTarget.evidenceAtMs,
    recoveryEvidenceAtMs: overrides.recoveryEvidenceAtMs ?? databaseTarget.evidenceAtMs,
    rpoMs: overrides.rpoMs ?? 2 * hour,
    rpoState: overrides.rpoState ?? (databaseTarget.rpoViolation ? "violated" : databaseTarget.latestSuccessAtMs ? "current" : "unknown"),
    rpoViolation: overrides.rpoViolation ?? databaseTarget.rpoViolation,
    verificationRequestedLevel: overrides.verificationRequestedLevel ?? 2,
    verificationState: overrides.verificationState ?? databaseTarget.verificationState,
    verificationOverdue: overrides.verificationOverdue ?? databaseTarget.verificationOverdue,
    planCount: overrides.planCount,
    activePlanCount: overrides.activePlanCount ?? overrides.planCount,
    retentionPolicyCount: overrides.retentionPolicyCount,
    evidenceAtMs: overrides.evidenceAtMs ?? databaseTarget.evidenceAtMs,
  };
}

function ensureCanonicalWholeDatabaseScope(databaseTarget: DatabaseTarget): void {
  const alreadyExists = previewBackupScopes.some(
    (scope) =>
      scope.id === databaseTarget.id &&
      scope.databaseTargetId === databaseTarget.id &&
      scope.mode === "whole_database",
  );
  if (alreadyExists) return;

  previewBackupScopes.push(backupScopeFromTarget(databaseTarget.id, {
    id: databaseTarget.id,
    displayName: `${databaseTarget.displayName} 전체 백업`,
    mode: "whole_database",
    selectedObjectCount: null,
    eligibleObjectCount: null,
    newObjectCount: null,
    missingObjectCount: null,
    planCount: 0,
    retentionPolicyCount: 0,
  }));
}

previewConnections
  .flatMap((connection) => connection.targets)
  .forEach(ensureCanonicalWholeDatabaseScope);

const previewDestinations: Destination[] = [
  {
    id: "destination-s3-worm",
    displayName: "S3 Compliance Vault",
    kind: "s3",
    endpoint: "company-backups/production",
    state: "ready",
    immutabilityCapability: "compliance",
    lastProbeStatus: "passed",
    lastProbeAtMs: observedNow() - 18 * minute,
    updatedAtMs: observedNow() - 18 * minute,
    roles: [
      { role: "backup_writer", credentialBound: true, probeStatus: "passed", probedAtMs: observedNow() - 18 * minute },
      { role: "restore_reader", credentialBound: true, probeStatus: "passed", probedAtMs: observedNow() - 17 * minute },
      { role: "delete_operator", credentialBound: false, probeStatus: null, probedAtMs: null },
    ],
  },
  {
    id: "destination-dr-sftp",
    displayName: "DR SFTP Vault",
    kind: "ssh_sftp",
    endpoint: "dr-vault.internal:/srv/dbbackup",
    state: "attention",
    immutabilityCapability: "server_append_only",
    lastProbeStatus: "failed",
    lastProbeAtMs: observedNow() - 2 * hour,
    updatedAtMs: observedNow() - 2 * hour,
    roles: [
      { role: "backup_writer", credentialBound: true, probeStatus: "passed", probedAtMs: observedNow() - 2 * hour },
      { role: "restore_reader", credentialBound: true, probeStatus: "failed", probedAtMs: observedNow() - 2 * hour },
      { role: "delete_operator", credentialBound: false, probeStatus: null, probedAtMs: null },
    ],
  },
  {
    id: "destination-local-spool",
    displayName: "Local Recovery Cache",
    kind: "local",
    endpoint: "E:\\DbBackup\\cache",
    state: "ready",
    immutabilityCapability: "none",
    lastProbeStatus: "passed",
    lastProbeAtMs: observedNow() - 9 * minute,
    updatedAtMs: observedNow() - 9 * minute,
    roles: [
      { role: "backup_writer", credentialBound: false, probeStatus: "passed", probedAtMs: observedNow() - 9 * minute },
      { role: "restore_reader", credentialBound: false, probeStatus: "passed", probedAtMs: observedNow() - 9 * minute },
      { role: "delete_operator", credentialBound: false, probeStatus: "passed", probedAtMs: observedNow() - 9 * minute },
    ],
  },
];

const previewPlans: BackupPlan[] = [
  {
    id: "plan-production-hourly",
    displayName: "Production hourly",
    state: "enabled",
    revisionId: "revision-production-7",
    revisionNo: 7,
    scheduleKind: "cron",
    scheduleExpression: "15 * * * *",
    timezone: "Asia/Seoul",
    rpoMs: 2 * hour,
    publishedAtMs: observedNow() - 4 * day,
    targetCount: 2,
    destinationCount: 2,
    requiredDestinationCount: 1,
    updatedAtMs: observedNow() - 4 * day,
  },
  {
    id: "plan-ledger-nightly",
    displayName: "Ledger nightly",
    state: "attention",
    revisionId: "revision-ledger-3",
    revisionNo: 3,
    scheduleKind: "cron",
    scheduleExpression: "0 2 * * *",
    timezone: "Asia/Seoul",
    rpoMs: day,
    publishedAtMs: observedNow() - 20 * day,
    targetCount: 1,
    destinationCount: 1,
    requiredDestinationCount: 1,
    updatedAtMs: observedNow() - 20 * day,
  },
];

interface PreviewSnapshot extends Snapshot {
  parts: SnapshotPart[];
  replicas: SnapshotReplica[];
  verificationEvidence: VerificationEvidence[];
  restoreDrills: RestoreDrillEvidence[];
}

const previewSnapshots: PreviewSnapshot[] = [
  {
    id: "rp-target-orders-latest",
    sourceTargetIdentity: "postgresql:connection-prod-pg:orders",
    sourceDisplayName: "orders",
    sourceConnectionId: "connection-prod-pg",
    sourceConnectionName: "Production PostgreSQL",
    engine: "postgresql",
    state: "complete",
    committedAtMs: observedNow() - 42 * minute,
    manifestSchemaVersion: 2,
    setManifestChecksumBlake3: "c".repeat(64),
    signingKeyId: "manifest-key-2026-07",
    artifactCompleteness: "complete",
    replicaDurability: "healthy",
    protectionCompliance: "compliant",
    restoreEligibility: "eligible",
    highestVerifiedLevel: 3,
    lastVerifiedAtMs: observedNow() - 30 * minute,
    signatureStatus: "valid",
    requiredPartCount: 2,
    presentPartCount: 2,
    plaintextBytes: 8_140_291_070,
    cipherBytes: 3_711_250_000,
    firstIngestedAtMs: observedNow() - 41 * minute,
    lastIngestedAtMs: observedNow() - 40 * minute,
    diagnosticReason: null,
    partCount: 2,
    replicaCount: 2,
    verificationEvidenceCount: 1,
    restoreDrillCount: 1,
    verificationEvidence: [
      {
        id: "verification-orders-l3",
        level: 3,
        result: "passed",
        verifiedAtMs: observedNow() - 30 * minute,
        toolName: "pg_restore",
        toolVersion: "18.4",
        evidenceChecksumBlake3: "d".repeat(64),
        evidenceLocator: "browser-fixture://verification/orders-l3",
        evidenceAvailable: true,
      },
    ],
    restoreDrills: [
      {
        restoreRunId: "restore-drill-orders-01",
        verificationRunId: "verification-orders-l3",
        targetConnectionId: "connection-restore-lab-pg",
        targetConnectionName: "PostgreSQL Restore Lab",
        targetName: "orders_drill_202607",
        state: "succeeded",
        measuredRtoMs: 48_200,
        startedAtMs: observedNow() - 31 * minute,
        completedAtMs: observedNow() - 30 * minute,
        errorCode: null,
        evidenceAvailable: true,
      },
    ],
    parts: [
      {
        id: "part-orders-dump",
        role: "database_dump",
        logicalName: "orders",
        required: true,
        dumpFormat: "custom",
        plaintextBytes: 8_139_000_000,
        checksumBlake3: "a".repeat(64),
      },
      {
        id: "part-orders-globals",
        role: "globals",
        logicalName: "cluster-globals",
        required: true,
        dumpFormat: "plain",
        plaintextBytes: 1_291_070,
        checksumBlake3: "b".repeat(64),
      },
    ],
    replicas: [
      {
        id: "replica-orders-s3",
        destinationFingerprint: "s3:destination-s3-worm",
        destinationKind: "s3",
        requirement: "required",
        state: "committed",
        immutabilityCapability: "compliance",
        retentionUntilMs: observedNow() + 29 * day,
        readerStatus: "eligible",
        observedAtMs: observedNow() - 17 * minute,
      },
      {
        id: "replica-orders-local",
        destinationFingerprint: "local:destination-local-spool",
        destinationKind: "local",
        requirement: "best_effort",
        state: "committed",
        immutabilityCapability: "none",
        retentionUntilMs: null,
        readerStatus: "eligible",
        observedAtMs: observedNow() - 9 * minute,
      },
    ],
  },
  {
    id: "rp-customer-analytics-degraded",
    sourceTargetIdentity: "postgresql:connection-prod-pg:고객-분석",
    sourceDisplayName: "고객 행동 분석 장기 보존 데이터베이스",
    sourceConnectionId: "connection-prod-pg",
    sourceConnectionName: "Production PostgreSQL",
    engine: "postgresql",
    state: "complete",
    committedAtMs: observedNow() - 27 * hour,
    manifestSchemaVersion: 2,
    setManifestChecksumBlake3: "e".repeat(64),
    signingKeyId: "manifest-key-2026-07",
    artifactCompleteness: "complete",
    replicaDurability: "degraded",
    protectionCompliance: "non_compliant",
    restoreEligibility: "conditional",
    highestVerifiedLevel: 1,
    lastVerifiedAtMs: observedNow() - 26 * hour,
    signatureStatus: "valid",
    requiredPartCount: 2,
    presentPartCount: 2,
    plaintextBytes: 42_400_000_000,
    cipherBytes: 19_730_000_000,
    firstIngestedAtMs: observedNow() - 27 * hour,
    lastIngestedAtMs: observedNow() - 26 * hour,
    diagnosticReason: null,
    partCount: 0,
    replicaCount: 2,
    verificationEvidenceCount: 1,
    restoreDrillCount: 0,
    verificationEvidence: [
      {
        id: "verification-analytics-l1",
        level: 1,
        result: "passed",
        verifiedAtMs: observedNow() - 26 * hour,
        toolName: null,
        toolVersion: null,
        evidenceChecksumBlake3: "f".repeat(64),
        evidenceLocator: "browser-fixture://verification/analytics-l1",
        evidenceAvailable: true,
      },
    ],
    restoreDrills: [],
    parts: [],
    replicas: [
      {
        id: "replica-analytics-local",
        destinationFingerprint: "local:destination-local-spool",
        destinationKind: "local",
        requirement: "best_effort",
        state: "committed",
        immutabilityCapability: "none",
        retentionUntilMs: null,
        readerStatus: "eligible",
        observedAtMs: observedNow() - 3 * hour,
      },
      {
        id: "replica-analytics-sftp",
        destinationFingerprint: "ssh_sftp:destination-dr-sftp",
        destinationKind: "ssh_sftp",
        requirement: "required",
        state: "degraded",
        immutabilityCapability: "server_append_only",
        retentionUntilMs: null,
        readerStatus: "unavailable",
        observedAtMs: observedNow() - 2 * hour,
      },
    ],
  },
  {
    id: "rp-orders-incomplete",
    sourceTargetIdentity: "postgresql:connection-prod-pg:orders",
    sourceDisplayName: "orders",
    sourceConnectionId: "connection-prod-pg",
    sourceConnectionName: "Production PostgreSQL",
    engine: "postgresql",
    state: "incomplete",
    committedAtMs: null,
    manifestSchemaVersion: 2,
    setManifestChecksumBlake3: "0".repeat(64),
    signingKeyId: "unknown-key",
    artifactCompleteness: "incomplete",
    replicaDurability: "unavailable",
    protectionCompliance: "non_compliant",
    restoreEligibility: "blocked",
    highestVerifiedLevel: null,
    lastVerifiedAtMs: null,
    signatureStatus: "unknown_key",
    requiredPartCount: 2,
    presentPartCount: 1,
    plaintextBytes: null,
    cipherBytes: null,
    firstIngestedAtMs: observedNow() - 8 * minute,
    lastIngestedAtMs: observedNow() - 8 * minute,
    diagnosticReason: "manifest_unknown_key",
    partCount: 0,
    replicaCount: 0,
    verificationEvidenceCount: 0,
    restoreDrillCount: 0,
    replicas: [],
    parts: [],
    verificationEvidence: [],
    restoreDrills: [],
  },
];

const engineCapabilityNames: EngineCapability["capability"][] = [
  "probe",
  "discovery",
  "tool_preflight",
  "backup",
  "verify_l1",
  "verify_l2",
  "restore_create_new",
  "validation",
  "restore_l3",
];

function previewCapabilities(
  evidenceLocator: string | null,
  supportStatus: EngineCapability["supportStatus"] = "passed",
): EngineCapability[] {
  return engineCapabilityNames.map((capability) => ({
    capability,
    supportStatus,
    limitation: null,
    evidenceLocator,
  }));
}

function unavailableEngine(engine: string): EngineMatrixRow {
  return {
    certificationId: null,
    engine,
    serverVersionRange: null,
    toolName: null,
    toolVersionRange: null,
    operatingSystem: null,
    architecture: null,
    dumpFormat: null,
    consistencyGrade: null,
    supportStatus: "unavailable",
    evidenceLocator: null,
    evidenceChecksumBlake3: null,
    certifiedAtMs: null,
    expiresAtMs: null,
    capabilities: previewCapabilities(null, "not_run"),
  };
}

export function previewEngineMatrix(): EngineMatrix {
  const generatedAtMs = observedNow();
  const currentEvidence = "browser-fixture://engine-matrix/current-build";
  const supported = (engine: string, toolName: string): EngineMatrixRow => ({
    certificationId: `fixture-${engine}-current`,
    engine,
    serverVersionRange: "fixture-certified",
    toolName,
    toolVersionRange: "fixture-locked",
    operatingSystem: "windows",
    architecture: "x86_64",
    dumpFormat: engine === "sqlite" ? "sqlite3" : "native-dump",
    consistencyGrade: engine === "sqlite" ? "online-snapshot" : "G2",
    supportStatus: "supported",
    evidenceLocator: currentEvidence,
    evidenceChecksumBlake3: "a".repeat(64),
    certifiedAtMs: generatedAtMs - day,
    expiresAtMs: generatedAtMs + 29 * day,
    capabilities: previewCapabilities(currentEvidence),
  });
  const sqlServerCapabilities = previewCapabilities(null, "not_run").map(
    (item): EngineCapability =>
      item.capability === "tool_preflight"
        ? {
            ...item,
            supportStatus: "passed",
            evidenceLocator: "browser-fixture://engine-matrix/sqlpackage-policy",
          }
        : {
            ...item,
            limitation:
              item.capability === "backup" ||
              item.capability === "restore_create_new" ||
              item.capability === "restore_l3"
                ? "실제 SQL Server BACPAC export/import 왕복 인증 근거가 없습니다."
                : null,
          },
  );
  return {
    generatedAtMs,
    certificationTotalCount: 5,
    complete: true,
    rows: [
      supported("postgresql", "pg_dump/pg_restore"),
      supported("mysql", "mysqldump/mysql"),
      supported("mariadb", "mariadb-dump/mariadb"),
      supported("sqlite", "rusqlite"),
      {
        certificationId: "fixture-sqlserver-candidate",
        engine: "sqlserver",
        serverVersionRange: null,
        toolName: "SqlPackage",
        toolVersionRange: "=170.4.83",
        operatingSystem: "windows",
        architecture: "x86_64",
        dumpFormat: "bacpac",
        consistencyGrade: "candidate",
        supportStatus: "candidate",
        evidenceLocator: "browser-fixture://engine-matrix/sqlserver-candidate",
        evidenceChecksumBlake3: null,
        certifiedAtMs: generatedAtMs,
        expiresAtMs: null,
        capabilities: sqlServerCapabilities,
      },
      unavailableEngine("mongodb"),
      unavailableEngine("cockroachdb"),
      unavailableEngine("oracle"),
      unavailableEngine("tidb"),
    ],
  };
}

export function previewServiceHealth(): ServiceHealth {
  return {
    bootId: "browser-preview-fixture",
    phase: "rehearsal_ready",
    schemaVersion: 4,
    applicationTableCount: 67,
    executionAuthority: "legacy_direct",
    cutoverMode: "rehearsal_only",
    cutoverMarker: null,
    recoveryDisposition: null,
    claimsEnabled: false,
    acceptsIpc: true,
    diagnosticCode: null,
    scheduler: {
      state: "observed",
      configuredPlanCount: 2,
      observedCursorCount: 2,
      activeWorkCount: 1,
      errorPlanCount: 0,
      lastTickAtMs: observedNow() - minute,
      lastMaterializedForMs: observedNow() - 2 * minute,
      lastErrorCode: null,
      lastErrorAtMs: null,
      evidenceAtMs: observedNow() - minute,
    },
    retention: {
      state: "observed",
      configuredPolicyCount: 2,
      activeTombstoneCount: 1,
      queuedDeleteCount: 0,
      failedOperationCount: 0,
      lastScanAtMs: observedNow() - 4 * minute,
      lastErrorCode: null,
      lastErrorAtMs: null,
      evidenceAtMs: observedNow() - 4 * minute,
    },
  };
}

export function previewConnectionPage(): CursorPage<ConnectionProfile> {
  return page(previewConnections.map((connection) => ({
    ...connection,
    targets: [],
  })));
}

export function previewTargetPage(
  payload: Record<string, unknown> = {},
): CursorPage<DatabaseTarget> {
  const connectionId = typeof payload.connectionId === "string" ? payload.connectionId : null;
  const state = typeof payload.state === "string" ? payload.state : null;
  const search = typeof payload.search === "string"
    ? payload.search.trim().toLocaleLowerCase()
    : "";
  const limit = boundedNumber(payload.limit, 50, 1, 100);
  const start = typeof payload.cursor === "string" ? Number(payload.cursor) || 0 : 0;
  const filtered = previewConnections
    .flatMap((connection) => connection.targets)
    .filter((target) =>
      (!connectionId || target.connectionId === connectionId) &&
      (!state || target.state === state) &&
      (!search || [
        target.displayName,
        target.connectionName,
        target.engine,
      ].some((value) => value.toLocaleLowerCase().includes(search)))
    );
  const items = filtered.slice(start, start + limit).map((target) => ({ ...target }));
  const next = start + items.length;
  return {
    items,
    nextCursor: next < filtered.length ? String(next) : null,
    totalCount: filtered.length,
  };
}

export function previewDiscoveryResultPage(
  payload: Record<string, unknown> = {},
): CursorPage<DiscoveredDatabase> {
  const search = typeof payload.search === "string"
    ? payload.search.trim().toLocaleLowerCase()
    : "";
  const eligibility = typeof payload.eligibility === "string" ? payload.eligibility : null;
  const limit = boundedNumber(payload.limit, 50, 1, 100);
  const start = typeof payload.cursor === "string" ? Number(payload.cursor) || 0 : 0;
  const filtered = previewDiscovery().discovered.filter((item) =>
    (!eligibility || item.eligibility === eligibility) &&
    (!search || [item.databaseName, item.engineObjectId]
      .some((value) => value.toLocaleLowerCase().includes(search)))
  );
  const items = filtered.slice(start, start + limit).map((item) => ({ ...item }));
  const next = start + items.length;
  return {
    items,
    nextCursor: next < filtered.length ? String(next) : null,
    totalCount: filtered.length,
  };
}

export function previewBackupScopePage(payload: Record<string, unknown> = {}): {
  items: BackupScope[];
  nextCursor: string | null;
  totalCount: number;
} {
  const search = typeof payload.search === "string" ? payload.search.trim().toLocaleLowerCase() : "";
  const state = typeof payload.state === "string" ? payload.state : null;
  const limit = boundedNumber(payload.limit, 50, 1, 100);
  const start = typeof payload.cursor === "string" ? Number(payload.cursor) || 0 : 0;
  const filtered = previewBackupScopes.filter((scope) => {
    const matchesSearch = !search || [
      scope.displayName,
      scope.databaseName,
      scope.connectionName,
      scope.engine,
    ].some((value) => value.toLocaleLowerCase().includes(search));
    return matchesSearch && (!state || scope.state === state);
  });
  const items = filtered.slice(start, start + limit).map((scope) => ({ ...scope }));
  const next = start + items.length;
  return {
    items,
    nextCursor: next < filtered.length ? String(next) : null,
    totalCount: filtered.length,
  };
}

export function previewBackupScopeDiscovery(
  payload: Record<string, unknown>,
): BackupScopeDiscoveryRun {
  const databaseTargetId = typeof payload.databaseTargetId === "string"
    ? payload.databaseTargetId
    : "unknown";
  const suffix = crypto.randomUUID();
  return {
    operationRunId: `scope-discovery-${databaseTargetId}-${suffix}`,
    workItemId: `work-scope-discovery-${databaseTargetId}-${suffix}`,
    state: "queued",
  };
}

export function previewBackupScopeObjectPage(
  payload: Record<string, unknown>,
): BackupScopeObjectPage {
  const discoveryRunId = typeof payload.discoveryRunId === "string"
    ? payload.discoveryRunId
    : "scope-discovery-preview";
  const databaseTargetId = typeof payload.databaseTargetId === "string"
    ? payload.databaseTargetId
    : "target-orders";
  const base = previewDatabaseObjects(databaseTargetId, discoveryRunId);
  const search = typeof payload.search === "string" ? payload.search.trim().toLocaleLowerCase() : "";
  const schemaName = typeof payload.schemaName === "string" ? payload.schemaName : null;
  const eligibility = typeof payload.eligibility === "string" ? payload.eligibility : null;
  const filtered = base.filter((item) => {
    const qualifiedName = `${item.schemaName ?? ""}.${item.objectName}`.toLocaleLowerCase();
    return (!search || qualifiedName.includes(search)) &&
      (!schemaName || item.schemaName === schemaName) &&
      (!eligibility || item.eligibility === eligibility);
  });
  const limit = boundedNumber(payload.limit, 50, 1, 100);
  const start = typeof payload.cursor === "string" ? Number(payload.cursor) || 0 : 0;
  const items = filtered.slice(start, start + limit);
  const next = start + items.length;
  return {
    items,
    nextCursor: next < filtered.length ? String(next) : null,
    totalCount: filtered.length,
    eligibleCount: base.filter((item) => item.eligibility === "eligible").length,
    discoveryRunId,
    inventoryHashBlake3: "8".repeat(64),
    observedAtMs: observedNow(),
  };
}

export function previewSaveBackupScope(
  payload: Record<string, unknown>,
): BackupScopeSaveResult {
  const databaseTargetId = String(payload.databaseTargetId ?? "");
  if (payload.mode !== "whole_database" && payload.mode !== "explicit_tables") {
    throw new ServiceError("APPLICATION_PAYLOAD_INVALID");
  }
  const mode = payload.mode;
  const engineObjectIds = Array.isArray(payload.engineObjectIds)
    ? payload.engineObjectIds.filter((value): value is string => typeof value === "string")
    : [];
  const scopeId = mode === "whole_database"
    ? databaseTargetId
    : `scope-preview-${crypto.randomUUID()}`;
  const databaseTarget = previewConnections
    .flatMap((connection) => connection.targets)
    .find((candidate) => candidate.id === databaseTargetId);
  if (!databaseTarget) throw new Error("BACKUP_SCOPE_DATABASE_TARGET_UNKNOWN");
  const replacedScopeId = typeof payload.replacesScopeId === "string"
    ? payload.replacesScopeId
    : undefined;
  const result: BackupScopeSaveResult = {
    scopeId,
    databaseTargetId,
    mode,
    state: "active",
    selectedObjectCount: mode === "whole_database" ? null : engineObjectIds.length,
    selectorHashBlake3: "9".repeat(64),
    sourceFingerprint: databaseTarget.sourceFingerprint,
    ...(replacedScopeId ? { replacedScopeId } : {}),
  };
  if (mode === "explicit_tables") {
    const existing = previewBackupScopes.findIndex((scope) => scope.id === scopeId);
    const savedScope = backupScopeFromTarget(databaseTargetId, {
      id: scopeId,
      displayName: String(payload.displayName ?? `${databaseTarget.displayName} 선택 백업`),
      mode,
      selectedObjectCount: engineObjectIds.length,
      eligibleObjectCount: previewDatabaseObjects(
        databaseTargetId,
        String(payload.discoveryRunId ?? "scope-discovery-preview"),
      ).filter((object) => object.eligibility === "eligible").length,
      newObjectCount: 0,
      missingObjectCount: 0,
      discoveryRunId: String(payload.discoveryRunId ?? "scope-discovery-preview"),
      inventoryHashBlake3: "8".repeat(64),
      inventoryObservedAtMs: observedNow(),
      protectionCompliance: "not_configured",
      planCount: 0,
      retentionPolicyCount: 0,
    });
    if (existing >= 0) previewBackupScopes[existing] = savedScope;
    else previewBackupScopes.push(savedScope);
  }
  return result;
}

function previewDatabaseObjects(databaseTargetId: string, discoveryRunId: string) {
  const enginePrefix = databaseTargetId.includes("billing") ? "mysql" : "pg";
  return [
    {
      engineObjectId: `opaque:${enginePrefix}:orders/public/iwrs`,
      schemaName: "public",
      objectName: "iwrs",
      objectType: "table" as const,
      eligibility: "eligible" as const,
      exclusionReason: null,
      fingerprint: "fixture-object-iwrs",
      discoveryRunId,
    },
    {
      engineObjectId: `opaque:${enginePrefix}:orders/public/audit_log`,
      schemaName: "public",
      objectName: "audit_log",
      objectType: "table" as const,
      eligibility: "eligible" as const,
      exclusionReason: null,
      fingerprint: "fixture-object-audit-log",
      discoveryRunId,
    },
    {
      engineObjectId: `opaque:${enginePrefix}:orders/public/new_events_2026`,
      schemaName: "public",
      objectName: "new_events_2026",
      objectType: "table" as const,
      eligibility: "eligible" as const,
      exclusionReason: null,
      fingerprint: "fixture-object-new-events",
      discoveryRunId,
    },
    {
      engineObjectId: `opaque:${enginePrefix}:orders/pg_catalog/pg_statistic`,
      schemaName: "pg_catalog",
      objectName: "pg_statistic",
      objectType: "table" as const,
      eligibility: "excluded" as const,
      exclusionReason: "system table",
      fingerprint: "fixture-object-system",
      discoveryRunId,
    },
  ];
}

export function previewDestinationPage(): CursorPage<Destination> {
  return page(previewDestinations);
}

export function previewPlanPage(): CursorPage<BackupPlan> {
  return page(previewPlans);
}

export function previewSnapshotPage(
  payload: Record<string, unknown> = {},
): CursorPage<Snapshot> {
  const scope = typeof payload.scope === "string" ? payload.scope : "complete";
  const items =
    scope === "diagnostics"
      ? previewSnapshots.filter((snapshot) => snapshot.state !== "complete")
      : scope === "all"
        ? previewSnapshots
        : previewSnapshots.filter((snapshot) => snapshot.state === "complete");
  return page(items.map(snapshotSummary));
}

export function previewSnapshotDetailPage(
  payload: Record<string, unknown> = {},
): SnapshotSectionPage<
  SnapshotPart | SnapshotReplica | VerificationEvidence | RestoreDrillEvidence,
  SnapshotDetailSection
> {
  const snapshotId = typeof payload.snapshotId === "string"
    ? payload.snapshotId
    : "";
  const section = payload.section;
  if (
    section !== "parts" &&
    section !== "replicas" &&
    section !== "verification_evidence" &&
    section !== "restore_drills"
  ) {
    throw new Error("SNAPSHOT_DETAIL_SECTION_INVALID");
  }
  const snapshot = previewSnapshots.find((item) => item.id === snapshotId);
  if (!snapshot) throw new Error("OPERATIONAL_REPOSITORY_NOT_FOUND");
  const source = section === "parts"
    ? snapshot.parts
    : section === "replicas"
      ? snapshot.replicas
      : section === "verification_evidence"
        ? snapshot.verificationEvidence
        : snapshot.restoreDrills;
  const limit = boundedNumber(payload.limit, 50, 1, 100);
  const start = typeof payload.cursor === "string"
    ? Number(payload.cursor) || 0
    : 0;
  const items = source.slice(start, start + limit);
  const next = start + items.length;
  return {
    snapshot: snapshotSummary(snapshot),
    section,
    items,
    nextCursor: next < source.length ? String(next) : null,
    totalCount: source.length,
    complete: next >= source.length,
  };
}

export function previewActivityPage(payload: Record<string, unknown>): CursorPage<ActivityEvent> {
  const limit = boundedNumber(payload.limit, 50, 1, 100);
  const cursor = typeof payload.cursor === "string" ? payload.cursor : null;
  const cursorParts = cursor?.split(":") ?? [];
  const cursorSeq = cursor
    ? Number(cursorParts[cursorParts.length - 1])
    : ACTIVITY_TOTAL + 1;
  const severity = typeof payload.severity === "string" ? payload.severity : null;
  const items: ActivityEvent[] = [];
  let seq = Math.min(cursorSeq - 1, ACTIVITY_TOTAL);
  while (seq > 0 && items.length < limit) {
    const event = previewActivityEvent(seq);
    if (!severity || event.severity === severity) items.push(event);
    seq -= 1;
  }
  const last = items.length ? items[items.length - 1] : undefined;
  return {
    items,
    nextCursor: last && last.seq > 1 ? `${last.occurredAtMs}:${last.seq}` : null,
    totalCount: severity ? Math.floor(ACTIVITY_TOTAL / 20) : ACTIVITY_TOTAL,
  };
}

export function previewSettings(): SettingsData {
  return {
    resourcePools: [
      { id: "global", resourceType: "global_job", resourceKey: "*", capacity: 4, inUse: 1, enabled: true },
      { id: "restore", resourceType: "restore", resourceKey: "*", capacity: 1, inUse: 0, enabled: true },
      { id: "s3", resourceType: "destination", resourceKey: "destination-s3-worm", capacity: 2, inUse: 1, enabled: true },
    ],
    spoolVolumes: [
      {
        id: "spool-e",
        rootPath: "E:\\DbBackup\\spool",
        state: "ready",
        hardLimitBytes: 500 * 1024 ** 3,
        softLimitBytes: 425 * 1024 ** 3,
        freeBytes: 312 * 1024 ** 3,
        probedAtMs: observedNow() - 4 * minute,
      },
    ],
    tools: [
      { id: "pg-dump-18", name: "pg_dump", version: "18.4", architecture: "x86_64", trustStatus: "trusted", signatureStatus: "valid", verifiedAtMs: observedNow() - day },
      { id: "mysql-dump", name: "mysqldump", version: "9.4", architecture: "x86_64", trustStatus: "trusted", signatureStatus: "valid", verifiedAtMs: observedNow() - day },
      { id: "maria-dump", name: "mariadb-dump", version: "11.8", architecture: "x86_64", trustStatus: "trusted", signatureStatus: "valid", verifiedAtMs: observedNow() - day },
    ],
    notificationChannels: [
      { id: "ops-webhook", displayName: "Operations webhook", kind: "webhook", state: "ready", lastTestStatus: "delivered", lastTestAtMs: observedNow() - 6 * hour },
    ],
    controlBackup: {
      lastCompletedAtMs: observedNow() - day,
      lastStatus: "succeeded",
    },
    inventoryCounts: {
      resourcePools: 3,
      spoolVolumes: 1,
      tools: 3,
      notificationChannels: 1,
    },
    complete: true,
  };
}

export function previewSaveConnection(payload: Record<string, unknown>): ConnectionSaveResult {
  const draft = payload as unknown as ConnectionDraft;
  const connectionId = `connection-preview-${previewConnections.length + 1}`;
  previewConnections.push({
    id: connectionId,
    displayName: draft.displayName,
    engine: draft.engine,
    transport: draft.transport,
    endpoint: draft.transport === "file" ? draft.filePath : `${draft.host}:${draft.port}`,
    state: "draft",
    lastProbeStatus: null,
    lastProbeAtMs: null,
    restoreTestTarget: draft.restoreTestTarget,
    updatedAtMs: observedNow(),
    targetCount: 0,
    targets: [],
  });
  return { connectionId, state: "draft" };
}

export function previewRestorePreflight(
  payload: Record<string, unknown>,
): RestorePreflightResult {
  const snapshot = previewSnapshots.find(
    (item) => item.id === payload.snapshotId,
  );
  const connection = previewConnections.find(
    (item) => item.id === payload.targetConnectionId,
  );
  const mode = payload.mode === "overwrite" ? "overwrite" : "create_new";
  const purpose =
    payload.purpose === "restore_drill"
      ? "restore_drill"
      : "incident_recovery";
  const targetName =
    typeof payload.targetName === "string" ? payload.targetName : "";
  const targetExists =
    connection?.targets.some(
      (target) =>
        target.displayName.toLocaleLowerCase() === targetName.toLocaleLowerCase(),
    ) ?? false;
  const sourceEligible =
    snapshot?.state === "complete" &&
    snapshot.signatureStatus === "valid" &&
    snapshot.restoreEligibility !== "blocked";
  const sourceReplica =
    snapshot?.replicas.find(
      (replica) =>
        replica.state === "committed" && replica.readerStatus === "eligible",
    ) ?? null;
  const checks: RestorePreflightResult["checks"] = [
    check(
      "source_snapshot",
      sourceEligible ? "passed" : "failed",
      "서명, 전체성, restore eligibility를 서비스 catalog에서 재확인했습니다.",
      "유효한 complete Recovery Point를 선택하세요.",
    ),
    check(
      "source_replica",
      sourceReplica ? "passed" : "failed",
      "committed restore-reader Replica를 확인했습니다.",
      "Replica reader를 probe하거나 repair하세요.",
    ),
    check(
      "tool_and_validation",
      "passed",
      "고정된 restore tool과 validation certification이 유효합니다.",
      "도구 inventory와 Engine Matrix를 다시 인증하세요.",
    ),
    check(
      "space",
      "passed",
      "ready spool에 필요한 용량이 있습니다.",
      "spool 공간을 확보하세요.",
    ),
    check(
      "permission",
      connection ? "passed" : "failed",
      "target credential binding을 확인했습니다.",
      "대상 Connection credential을 다시 저장하세요.",
    ),
    check(
      "connection",
      connection?.state === "ready" && connection.lastProbeStatus === "passed"
        ? "passed"
        : "failed",
      "최근 service-owned probe를 확인했습니다.",
      "대상 Connection probe를 다시 실행하세요.",
    ),
    check(
      "server_version",
      connection ? "passed" : "failed",
      "인증된 서버 버전과 engine mapping입니다.",
      "인증된 adapter로 probe하세요.",
    ),
    check(
      "target_existence",
      (mode === "overwrite" && targetExists) ||
        (mode === "create_new" && !targetExists)
        ? "passed"
        : "failed",
      targetExists ? "대상이 존재합니다." : "대상이 존재하지 않습니다.",
      mode === "overwrite"
        ? "존재하는 정확한 대상 이름을 선택하세요."
        : "충돌하지 않는 새 이름을 사용하세요.",
    ),
    check(
      "engine_policy",
      snapshot && connection?.engine === snapshot.engine ? "passed" : "failed",
      "source와 target engine이 일치합니다.",
      "같은 engine의 Connection을 선택하세요.",
    ),
    check(
      "restore_target_role",
      purpose !== "restore_drill" || connection?.restoreTestTarget
        ? "passed"
        : "failed",
      connection?.restoreTestTarget
        ? "격리 restore-test target으로 등록됐습니다."
        : "일반 Connection입니다.",
      "격리된 Connection을 restore-test target으로 등록하세요.",
    ),
  ];
  const eligible = checks.every(
    (item) => item.state === "passed" || item.state === "warning",
  );
  return {
    schemaVersion: 1,
    preflightVersion: "preview-preflight".padEnd(64, "0"),
    evaluatedAtMs: observedNow(),
    expiresAtMs: observedNow() + 5 * minute,
    eligible,
    mode,
    purpose,
    source: {
      snapshotId: snapshot?.id ?? "",
      replicaId: sourceReplica?.id ?? "",
      displayName: snapshot?.sourceDisplayName ?? "",
      engine: snapshot?.engine ?? "",
      restoreEligibility: snapshot?.restoreEligibility ?? "blocked",
    },
    targetConnectionId: connection?.id ?? "",
    targetConnectionName: connection?.displayName ?? "",
    targetName,
    targetFingerprint: `preview:${connection?.id ?? "missing"}:${targetName}`,
    targetExists,
    restoreTestTarget: connection?.restoreTestTarget ?? false,
    targetRole: connection?.restoreTestTarget ? "restore_test" : "general",
    serverVersion: connection ? "17.4" : null,
    requiredSpoolBytes: snapshot?.plaintextBytes
      ? snapshot.plaintextBytes * 2
      : null,
    availableSpoolBytes: 312 * 1024 ** 3,
    checks,
  };
}

export function previewRestoreOverwriteConfirmation(
  payload: Record<string, unknown>,
): RestoreOverwriteConfirmation {
  const tokenId = `confirmation-${crypto.randomUUID()}`;
  const token = `preview-memory-token-${crypto.randomUUID()}`;
  const targetName =
    typeof payload.targetName === "string" ? payload.targetName : "";
  const overwriteRequest = {
    confirmationTokenId: tokenId,
    confirmationToken: token,
    snapshotId: payload.snapshotId,
    sourceReplicaId: payload.sourceReplicaId,
    targetConnectionId: payload.targetConnectionId,
    targetName,
    confirmedTargetName: payload.confirmedTargetName,
    preflightVersion: payload.preflightVersion,
    requiredVerificationLevel: payload.requiredVerificationLevel,
  };
  return {
    schemaVersion: 1,
    confirmationTokenId: tokenId,
    confirmationToken: token,
    action: "restore_overwrite",
    issuedAtMs: observedNow(),
    expiresAtMs: observedNow() + minute,
    tokenAvailable: true,
    payloadDigestBlake3: "confirmation-payload".padEnd(64, "0"),
    finalSummary: {
      source: {
        snapshotId: String(payload.snapshotId ?? ""),
        replicaId: String(payload.sourceReplicaId ?? ""),
      },
      target: {
        connectionId: String(payload.targetConnectionId ?? ""),
        name: targetName,
        fingerprint: `preview:${payload.targetConnectionId}:${targetName}`,
      },
      mode: "overwrite",
      destructiveEffect: "기존 target database가 교체됩니다.",
      cleanupPolicy:
        "실패 또는 취소 시 target을 보존하고 명시적 cleanup을 요구합니다.",
      preflightVersion: String(payload.preflightVersion ?? ""),
      requiredVerificationLevel: Number(
        payload.requiredVerificationLevel ?? 2,
      ),
    },
    overwriteRequest,
  };
}

export function previewCatalogRebuild(
  payload: Record<string, unknown>,
): CatalogRebuildPreview {
  return {
    schemaVersion: 1,
    available: true,
    destinationId: String(payload.destinationId ?? ""),
    destinationFingerprint: `preview:${payload.destinationId ?? ""}`,
    observedAtMs: observedNow(),
    totalCount: 4,
    acceptedCount: 1,
    rejectedCount: 1,
    duplicateCount: 1,
    unknownKeyCount: 1,
    reasonCode: null,
    candidates: [
      {
        objectKey: "v2/accepted/set-manifest.json",
        disposition: "accepted",
        reasonCode: null,
        signingKeyId: "manifest-key-2026-07",
        recoveryPointId: "rp-target-orders-latest",
        manifestChecksumBlake3: "a".repeat(64),
      },
      {
        objectKey: "v2/rejected/set-manifest.json",
        disposition: "rejected",
        reasonCode: "SIGNATURE_INVALID",
        signingKeyId: "manifest-key-2026-07",
        recoveryPointId: null,
        manifestChecksumBlake3: "b".repeat(64),
      },
      {
        objectKey: "v2/duplicate/set-manifest.json",
        disposition: "duplicate",
        reasonCode: "DUPLICATE_RECOVERY_POINT",
        signingKeyId: "manifest-key-2026-07",
        recoveryPointId: "rp-target-orders-latest",
        manifestChecksumBlake3: "c".repeat(64),
      },
      {
        objectKey: "v2/unknown/set-manifest.json",
        disposition: "unknown_key",
        reasonCode: null,
        signingKeyId: "retired-key",
        recoveryPointId: "rp-unknown",
        manifestChecksumBlake3: "d".repeat(64),
      },
    ],
  };
}

function check(
  key: string,
  state: RestorePreflightResult["checks"][number]["state"],
  detail: string,
  remediation: string,
): RestorePreflightResult["checks"][number] {
  return { key, state, detail, remediation };
}

export function previewSaveDestination(
  payload: Record<string, unknown>,
): DestinationSaveResult {
  const draft = payload as unknown as DestinationDraft;
  const destinationId = `destination-preview-${previewDestinations.length + 1}`;
  const endpoint =
    draft.kind === "local"
      ? draft.endpoint.path
      : draft.kind === "s3"
        ? [draft.endpoint.bucket, draft.endpoint.prefix]
            .filter(Boolean)
            .join("/")
        : `${draft.endpoint.host}:${draft.endpoint.port}:${draft.endpoint.path}`;
  const deleteBound =
    draft.kind !== "local" && Boolean(draft.credentials.deleteOperator);
  previewDestinations.push({
    id: destinationId,
    displayName: draft.displayName,
    kind: draft.kind,
    endpoint,
    state: "draft",
    immutabilityCapability: "none",
    lastProbeStatus: null,
    lastProbeAtMs: null,
    updatedAtMs: observedNow(),
    roles: [
      {
        role: "backup_writer",
        credentialBound: draft.kind !== "local",
        probeStatus: null,
        probedAtMs: null,
      },
      {
        role: "restore_reader",
        credentialBound: draft.kind !== "local",
        probeStatus: null,
        probedAtMs: null,
      },
      {
        role: "delete_operator",
        credentialBound: draft.kind === "local" || deleteBound,
        probeStatus: null,
        probedAtMs: null,
      },
    ],
  });
  return { destinationId, state: "draft" };
}

export function previewProbeDestination(
  payload: Record<string, unknown>,
): WorkItemRef {
  const destinationId =
    typeof payload.destinationId === "string" ? payload.destinationId : "";
  const destination = previewDestinations.find(
    (item) => item.id === destinationId,
  );
  if (destination) {
    const probedAtMs = observedNow();
    destination.state = "ready";
    destination.lastProbeStatus = "passed";
    destination.lastProbeAtMs = probedAtMs;
    destination.updatedAtMs = probedAtMs;
    destination.roles = destination.roles.map((role) => ({
      ...role,
      probeStatus:
        role.role === "delete_operator" &&
        destination.kind !== "local" &&
        !role.credentialBound
          ? null
          : "passed",
      probedAtMs:
        role.role === "delete_operator" &&
        destination.kind !== "local" &&
        !role.credentialBound
          ? null
          : probedAtMs,
    }));
  }
  return {
    workItemId: `work-destination-${crypto.randomUUID()}`,
    state: "succeeded",
    activityAfterSeq: ACTIVITY_TOTAL,
  };
}

export function previewProbeConnection(): WorkItemRef {
  return {
    workItemId: `work-probe-${crypto.randomUUID()}`,
    state: "succeeded",
    activityAfterSeq: ACTIVITY_TOTAL,
  };
}

export function previewDiscovery(): DiscoveryResult {
  return {
    ...previewProbeConnection(),
    discoveryRunId: `discovery-preview-${crypto.randomUUID()}`,
    discovered: [
      { engineObjectId: "database:orders", databaseName: "orders", eligibility: "eligible", exclusionReason: null },
      { engineObjectId: "database:ledger", databaseName: "ledger", eligibility: "eligible", exclusionReason: null },
      { engineObjectId: "database:template0", databaseName: "template0", eligibility: "excluded", exclusionReason: "system database" },
    ],
  };
}

export function previewSelectTargets(
  payload: Record<string, unknown>,
): TargetSelectionResult {
  const connectionId =
    typeof payload.connectionId === "string" ? payload.connectionId : "";
  const connection = previewConnections.find((item) => item.id === connectionId);
  const engineObjectIds = Array.isArray(payload.engineObjectIds)
    ? payload.engineObjectIds.filter(
        (value): value is string => typeof value === "string",
      )
    : [];
  if (!connection || engineObjectIds.length === 0) {
    return { targets: [] };
  }
  const selected = engineObjectIds.map((engineObjectId) => {
    const displayName = engineObjectId.split(":").slice(1).join(":");
    const fingerprint = `${connection.engine}:${connection.id}:${displayName}`;
    const existing = connection.targets.find(
      (item) => item.sourceFingerprint === fingerprint,
    );
    return (
      existing ??
      target(
        `target-${connection.id}-${displayName.replace(/[^a-zA-Z0-9_-]+/g, "-")}`,
        connection.id,
        connection.displayName,
        connection.engine,
        displayName,
        "not_configured",
      )
    );
  });
  connection.targets = selected;
  connection.targetCount = selected.length;
  connection.state = "ready";
  connection.updatedAtMs = observedNow();
  selected.forEach(ensureCanonicalWholeDatabaseScope);
  return { targets: selected };
}

function page<T>(items: T[]): CursorPage<T> {
  return { items: [...items], nextCursor: null, totalCount: items.length };
}

function snapshotSummary(snapshot: PreviewSnapshot): Snapshot {
  const {
    parts: _parts,
    replicas: _replicas,
    verificationEvidence: _verificationEvidence,
    restoreDrills: _restoreDrills,
    ...summary
  } = snapshot;
  return { ...summary };
}

function previewActivityEvent(seq: number): ActivityEvent {
  const occurredAtMs = ACTIVITY_OBSERVED_AT_MS - (ACTIVITY_TOTAL - seq) * 1_500;
  if (seq % 97 === 0) {
    return {
      seq,
      id: `event-${seq}`,
      occurredAtMs,
      severity: "error",
      eventCode: "DESTINATION_READER_UNAVAILABLE",
      phase: "replica_commit",
      entityType: "destination",
      entityId: "destination-dr-sftp",
      correlationId: `correlation-${Math.floor(seq / 20)}`,
      workItemId: `work-${Math.floor(seq / 20)}`,
      attemptId: `attempt-${Math.floor(seq / 20)}-1`,
      retryability: "retryable",
      message: "복구 reader probe가 인증 단계에서 실패했습니다.",
    };
  }
  if (seq % 29 === 0) {
    return {
      seq,
      id: `event-${seq}`,
      occurredAtMs,
      severity: "warning",
      eventCode: "WORK_WAITING_FOR_PERMIT",
      phase: "queued",
      entityType: "work_item",
      entityId: `work-${Math.floor(seq / 20)}`,
      correlationId: `correlation-${Math.floor(seq / 20)}`,
      workItemId: `work-${Math.floor(seq / 20)}`,
      attemptId: null,
      retryability: "unknown",
      message: "source permit을 기다리고 있습니다. 다음 재평가까지 약 18초입니다.",
    };
  }
  return {
    seq,
    id: `event-${seq}`,
    occurredAtMs,
    severity: "info",
    eventCode: seq % 5 === 0 ? "VERIFICATION_L2_PASSED" : "BACKUP_PHASE_COMPLETED",
    phase: seq % 5 === 0 ? "verification" : "upload",
    entityType: "work_item",
    entityId: `work-${Math.floor(seq / 20)}`,
    correlationId: `correlation-${Math.floor(seq / 20)}`,
    workItemId: `work-${Math.floor(seq / 20)}`,
    attemptId: `attempt-${Math.floor(seq / 20)}-1`,
    retryability: null,
    message: seq % 5 === 0 ? "L2 native restore 검증이 통과했습니다." : "암호화된 객체를 목적지에 commit했습니다.",
  };
}

function boundedNumber(
  value: unknown,
  fallback: number,
  minimum: number,
  maximum: number,
): number {
  return typeof value === "number" && Number.isInteger(value)
    ? Math.min(maximum, Math.max(minimum, value))
    : fallback;
}
