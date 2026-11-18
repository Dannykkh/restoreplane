/** Native counters and SQL numeric values never pass through JavaScript Number. */
export type NativeCounterU64 = string & { readonly __nativeU64: unique symbol };
export function nativeU64(value: unknown): NativeCounterU64 {
  if (typeof value !== "string" || !/^(0|[1-9]\d{0,19})$/.test(value) || BigInt(value) > 18446744073709551615n) {
    throw new Error("WB_PAYLOAD_INVALID");
  }
  return value as NativeCounterU64;
}
export type Scope = { workspaceId: string; connectionProfileId: string; databaseTargetId: string };
export type SessionRef = { sessionId: string; connectionGeneration: NativeCounterU64 };
export type ObjectRef = { catalogId: string; schemaVersion: string };
export type Mode = "read_only" | "read_write" | "full";
export interface Session extends Scope, SessionRef {
  descriptorRevision: string;
  executionEpoch: NativeCounterU64;
  mode: Mode;
  state: "connecting" | "ready" | "busy" | "retiring" | "closed";
  actualContext: { endpoint: string; database: string; role: string; purpose: "source" | "restored" };
  capabilities: Record<string, { supported: boolean; reason: string | null }>;
}
export type Transaction = { transactionId: string; session: SessionRef; state: "opening" | "active" | "failed_needs_rollback" | "commit_requested" | "committed" | "rollback_requested" | "rolled_back" | "outcome_unknown"; isolation: string; readOnly: boolean; idleExpiresAtMs: NativeCounterU64 };
export type Count = { kind: "unknown" } | { kind: "estimate" | "exact"; value: NativeCounterU64 };
export type PageInput = { cursor: string | null; limit: number };
export interface WorkbenchPage<T> { items: T[]; nextCursor: string | null; completeness: "complete" | "limited"; count: Count; asOfMs: NativeCounterU64 }
export type Column = { columnId: string; name: string; typeName: string; nullable: boolean; editable: boolean; readonlyReason: string | null };
export type TypedCell =
  | { kind: "null" }
  | { kind: "bool"; value: boolean }
  | { kind: "i64" | "u64" | "decimal"; value: string; scale?: number }
  | { kind: "float" | "text"; value: string }
  | { kind: "bytes"; base64: string }
  | { kind: "temporal"; value: string; typeName: string; offset: string | null; precision: number }
  | { kind: "unsupported"; typeName: string; reason: string }
  | { kind: "large"; valueHandle: string; typeName: string; byteLength: NativeCounterU64; preview: string };
export type ResultRow = { rowHandle: string; cells: TypedCell[] };
export type Consistency = { kind: "live_per_page" } | { kind: "transaction_snapshot"; snapshotId: string; expiresAtMs: NativeCounterU64 };
export interface ResultPage extends WorkbenchPage<ResultRow> { resultSetId: string; columns: Column[]; consistency: Consistency; schemaVersion: string }
export type CatalogItem = { object: ObjectRef; name: string; kind: string };
export type CatalogDetail =
  | { kind: "column"; column: Column; ordinal: number }
  | { kind: "key"; keyId: string; columns: string[]; primary: boolean; unique: boolean; nullable: boolean; partial: boolean; expression: boolean }
  | { kind: "index"; indexId: string; columns: string[]; unique: boolean; predicate: string | null }
  | { kind: "foreign_key"; keyId: string; columns: string[]; referencedRelation: ObjectRef; referencedColumns: string[] };
export type GraphNode = { nodeId: string; name?: string; relation: ObjectRef; edges: { to: string; name: string }[] };
export interface StatementOutcome { ordinal: number; transactionId: string | null; digest: string; outcome: "not_sent" | "sent" | "succeeded" | "failed" | "cancelled" | "outcome_unknown"; effectClass: "read" | "write" | "ddl" | "transaction" | "unknown"; effectEvidence: "none" | "read_only" | "pending_transaction" | "committed" | "rolled_back" | "outcome_unknown"; rowsMatched: NativeCounterU64 | null; rowsChanged: NativeCounterU64 | null; version: NativeCounterU64; errorCode: string | null }
export interface WorkbenchOperation { operationId: string; version: NativeCounterU64; kind: "query" | "manual_transaction" | "verified_apply" | "import_job" | "import_batch" | "export_job" | "protection_link"; state: "accepted" | "dispatching" | "running" | "commit_requested" | "committed" | "rolled_back" | "failed" | "outcome_unknown" | "completed" | "cancelled" | "interrupted" | "partial"; effectSummary: "none" | "read" | "transactional" | "partial" | "unknown"; transactionOutcome: "none" | "open" | "commit_requested" | "committed" | "rolled_back" | "outcome_unknown"; resultCompleteness: "none" | "complete" | "capped" | "cancelled" | "interrupted" | "partial"; transactionId: string | null; statements: StatementOutcome[]; parentOperationId: string | null; batchIndex: number | null; acknowledgedAtMs: NativeCounterU64 | null; createdAtMs: NativeCounterU64; updatedAtMs: NativeCounterU64 }
export type OperationSnapshot = { unchanged: true; operation: null } | { unchanged: false; operation: WorkbenchOperation };
export type QueryPurpose = "run" | "explain_plan" | "explain_analyze";
export interface QueryReview { reviewId: string; digest: string; requiredMode: Mode; statementCount: number; warnings: string[]; expiresAtMs: NativeCounterU64 }
export interface Admission { operationId: string; version: NativeCounterU64; deduplicated: boolean }
export interface PreparedAttempt { attemptToken: string; expiresAtMs: NativeCounterU64; payloadDigest: string }
export interface QueryRunInput { session: SessionRef; reviewId: string; confirmedWarnings: string[]; limits: Limits }
export interface QueryRunReceipt extends Admission { queryRunId: string; executionEpoch: NativeCounterU64 }
export interface SavedQuery { queryId: string; name: string; sql: string; version: NativeCounterU64 }
export type EditableCell = Exclude<TypedCell, { kind: "unsupported" } | { kind: "large" }>;
export type EditValues = { columnId: string; value: EditableCell }[];
export type EditChange = { kind: "insert"; values: EditValues } | { kind: "update"; rowHandle: string; values: EditValues } | { kind: "delete"; rowHandle: string };
export interface TargetContext extends Scope { connectionAlias: string; actualEndpoint: string; databaseName: string; databaseRole: string; purpose: "source" | "restored" }
export interface EditReviewData { draftId: string; reviewId: string; target: TargetContext; object: ObjectRef; statementPreviews: string[]; parameterPreview: EditableCell[]; changes: { kind: "insert" | "update" | "delete"; rowHandle: string | null; original: EditValues; draft: EditValues; omittedDefaultColumnIds: string[] }[]; warnings: string[]; expiresAtMs: NativeCounterU64 }
export interface EditApplyInput { session: SessionRef; draftId: string; reviewId: string; confirmedWarnings: string[] }
export interface ConflictObservation { conflictId: string; draftId: string; reviewId: string; operationId: string; basisObject: ObjectRef; currentSchemaVersion: string | null; observedAtMs: NativeCounterU64; rows: { changeIndex: number; rowHandle: string | null; current: { kind: "present"; values: EditValues } | { kind: "missing" } | { kind: "unavailable"; reason: string } }[] }
export type MovementFormat = "csv" | "tsv" | "json";
export interface MovementSource { sourceHandle: string; displayName: string; byteLength: NativeCounterU64; columns: { sourceKey: string; ordinal: number; name: string; typeHint: string | null }[] }
export interface MovementDestination { destinationHandle: string; displayName: string }
export interface ImportBatchStatus { operationId: string; batchIndex: number; state: WorkbenchOperation["state"]; version: NativeCounterU64; sourceByteStart: NativeCounterU64; sourceByteEnd: NativeCounterU64; sourceRowStart: NativeCounterU64; sourceRowEnd: NativeCounterU64; rowsCommitted: NativeCounterU64 | null; errorCode: string | null }
export interface MovementProgress { bytesProcessed: NativeCounterU64; totalBytes: NativeCounterU64 | null; rowsProcessed: NativeCounterU64; confirmedCommittedRows: NativeCounterU64; confirmedCommittedBatches: NativeCounterU64; unresolvedBatches: NativeCounterU64; nextBatchIndex: number | null }
export interface FileArtifact { displayPath: string; partialDisplayPath: string | null; format: MovementFormat; state: "not_started" | "partial" | "complete" | "unconfirmed" | "removed"; bytesWritten: NativeCounterU64; rowsWritten: NativeCounterU64; sourceCompleteness: Completion; finalized: boolean }
export interface MovementStatus { operation: WorkbenchOperation; progress: MovementProgress; children: WorkbenchPage<ImportBatchStatus>; file: FileArtifact | null }
export interface ImportContinuationReview { reviewId: string; parentOperationId: string; sourceDigest: string; nextUnsent: { batchIndex: number; byteStart: NativeCounterU64; rowStart: NativeCounterU64 } | null; confirmedCommittedRows: NativeCounterU64; unresolvedBatches: NativeCounterU64; warnings: string[]; expiresAtMs: NativeCounterU64 }
export interface ImportPreview { previewId: string; columns: Column[]; rows: WorkbenchPage<ResultRow>; warnings: string[] }
export interface ImportStartInput { session: SessionRef; previewId: string; confirmedWarnings: string[] }
export interface ImportContinueInput { session: SessionRef; parentOperationId: string; reviewId: string }
export interface ShownResult { identity: StreamIdentity; range: { startRow: NativeCounterU64; rowCount: number } }
export interface ExportStartInput { session: SessionRef; source: ({ kind: "shown" } & ShownResult) | { kind: "fresh"; reviewId: string; snapshotId: string | null }; destinationHandle: string; format: MovementFormat; formulaPolicy: "escape" | "raw_confirmed"; partialPolicy?: "retain" | "remove_on_cancel" }
export interface WorkbenchSelectionItem { engineObjectId: string; schemaName: string; objectName: string; structureSignature: string }
export interface WorkbenchSelection { signatureVersion: 1; expectedDescriptorRevision: string; items: WorkbenchSelectionItem[] }
export interface WorkbenchSelectionProof { selectionDigest: string; descriptorRevision: string; items: WorkbenchSelectionItem[]; verifiedAtMs: NativeCounterU64 }
export interface WorkbenchRestoreProof { restoreWorkItemId: string; restoreRunId: string; connectionProfileId: string; databaseName: string; engine: "postgresql" | "mysql" | "mariadb" | "sqlite"; serverIdentity: string | null; actualDatabaseIdentity: string; completedAtMs: NativeCounterU64 }
export type Filter = { kind: "and" | "or"; items: Filter[] } | { kind: "compare"; columnId: string; operator: "eq" | "ne" | "lt" | "le" | "gt" | "ge" | "is_null" | "is_not_null" | "contains"; value: TypedCell | null };
export type Order = { columnId: string; direction: "asc" | "desc"; nulls: "first" | "last" };
export type Limits = { maxRows: number; maxBytes: number; statementTimeoutMs: number; lockWaitMs: number };
export interface StreamIdentity { windowId: string; workspaceId: string; databaseTargetId: string; sessionId: string; connectionGeneration: NativeCounterU64; queryRunId: string; resultSetId: string; streamGeneration: NativeCounterU64 }
export type Completion = "complete" | "capped" | "cancelled" | "interrupted" | "partial" | "failed";
export interface ResultEnvelope {
  protocolVersion: 1; identity: StreamIdentity; sequence: NativeCounterU64; encodedBytes: number;
  event: { kind: "schema"; columns: Column[]; consistency: Consistency } | { kind: "rows"; startRow: NativeCounterU64; rows: ResultRow[] } | { kind: "terminal"; completeness: Completion; rowsSeen: NativeCounterU64; operationVersion: NativeCounterU64; errorCode: string | null };
}
export interface ResultNotice { protocolVersion: 1; kind: "frames_available"; identity: StreamIdentity; availableThroughSequence: NativeCounterU64 }
export type ResultTerminalStatus = Extract<ResultEnvelope["event"], { kind: "terminal" }>;
export interface WorkbenchCommands {
  "session.mode.set": [{ session: SessionRef; mode: Mode }, Session];
  "query.review": [{ session: SessionRef; sql: string; purpose: QueryPurpose }, QueryReview];
  "query.run": [QueryRunInput, QueryRunReceipt];
  "query.cancel": [{ session: SessionRef; queryRunId: string; executionEpoch: NativeCounterU64 }, { cancelRequested: boolean; settled: boolean }];
  "query.results": [{ session: SessionRef; queryRunId: string; page: PageInput }, WorkbenchPage<{ identity: StreamIdentity; state: "streaming" | "terminal"; completeness: Completion | null }>];
  "operation.prepare": [{ intent: { command: "query.run"; payload: QueryRunInput } | { command: "transaction.commit"; payload: { session: SessionRef; transactionId: string } } | { command: "edit.apply"; payload: EditApplyInput } | { command: "import.start"; payload: ImportStartInput } | { command: "import.continue"; payload: ImportContinueInput } }, PreparedAttempt];
  "edit.prepare": [{ session: SessionRef; object: ObjectRef; changes: EditChange[] }, EditReviewData];
  "edit.apply": [EditApplyInput, Admission];
  "edit.discard": [{ draftId: string }, { discarded: boolean }];
  "edit.conflict.get": [{ session: SessionRef; conflictId: string }, ConflictObservation];
  "movement.source.pick": [{ session: SessionRef; format: MovementFormat }, { source: MovementSource | null }];
  "movement.destination.pick": [{ session: SessionRef; format: MovementFormat }, { destination: MovementDestination | null }];
  "movement.file.release": [{ handle: string }, { released: boolean }];
  "movement.status": [{ operationId: string; page: PageInput }, MovementStatus];
  "movement.partial.remove": [{ operationId: string }, { removed: boolean }];
  "import.preview": [{ session: SessionRef; object: ObjectRef; sourceHandle: string; format: MovementFormat; mapping: Record<string, string>; page: PageInput }, ImportPreview];
  "import.start": [ImportStartInput, Admission];
  "import.continue.review": [{ session: SessionRef; parentOperationId: string; previewId: string }, ImportContinuationReview];
  "import.continue": [ImportContinueInput, Admission];
  "import.cancel": [{ parentOperationId: string }, { cancelRequested: boolean }];
  "export.start": [ExportStartInput, Admission];
  "export.cancel": [{ operationId: string }, { cancelRequested: boolean }];
  "transaction.begin": [{ session: SessionRef; isolation: string; readOnly: boolean }, Transaction];
  "transaction.commit": [{ session: SessionRef; transactionId: string }, Admission];
  "transaction.rollback": [{ session: SessionRef; transactionId: string }, Transaction];
  "saved_query.list": [{ scope: Scope; page: PageInput }, WorkbenchPage<{ queryId: string; name: string }>];
  "saved_query.get": [{ queryId: string }, SavedQuery];
  "saved_query.put": [{ scope: Scope; queryId: string | null; name: string; sql: string; expectedVersion: NativeCounterU64 | null }, { queryId: string; version: NativeCounterU64 }];
  "saved_query.delete": [{ queryId: string; expectedVersion: NativeCounterU64 }, { deleted: boolean }];
  "operation.get": [{ operationId: string; knownVersion: NativeCounterU64 | null }, OperationSnapshot];
  "operation.list": [{ scope: Scope; state: string | null; page: PageInput }, WorkbenchPage<WorkbenchOperation>];
  "operation.statements": [{ operationId: string; page: PageInput }, WorkbenchPage<StatementOutcome>];
  "operation.acknowledge_unknown": [{ operationId: string; expectedVersion: NativeCounterU64; acknowledgment: "uncertainty_understood" }, WorkbenchOperation];
  "credential.list": [{ scope: Scope; page: PageInput }, WorkbenchPage<{ credentialRef: string; label: string; username: string }>];
  "credential.put": [{ scope: Scope; credentialRef: string | null; label: string; username: string; secret: string }, { credentialRef: string }];
  "session.open": [{ scope: Scope; credentialRef: string; expectedDescriptorRevision: string | null; mode: Mode; restoreWorkItemId?: string }, Session];
  "protection.selection.get": [{ session: SessionRef; objects: ObjectRef[] }, { scope: Scope; workbenchSelection: WorkbenchSelection }];
  "session.list": [{ scope: Scope; page: PageInput }, WorkbenchPage<Session>];
  "session.reconnect": [{ session: SessionRef; expectedDescriptorRevision: string }, Session];
  "session.close": [{ session: SessionRef; disposition: "rollback_request" | "disconnect" }, { state: "closed" | "retiring"; transaction: Transaction | null }];
  "transaction.get": [{ session: SessionRef }, Transaction | null];
  "catalog.page": [{ session: SessionRef; parent: ObjectRef | null; kind: "schemas" | "relations" | "routines"; search: string; page: PageInput }, WorkbenchPage<CatalogItem>];
  "catalog.describe": [{ session: SessionRef; object: ObjectRef; part: "columns" | "keys" | "indexes" | "foreign_keys"; page: PageInput }, WorkbenchPage<CatalogDetail>];
  "catalog.graph": [{ session: SessionRef; schema: ObjectRef; page: PageInput }, WorkbenchPage<GraphNode>];
  "browse.open": [{ session: SessionRef; object: ObjectRef; filter: Filter; order: Order[]; snapshotId: string | null; limits: Limits }, { result: ResultPage; stream: StreamIdentity | null }];
  "browse.page": [{ session: SessionRef; cursor: string; limit: number }, ResultPage];
  "snapshot.begin": [{ session: SessionRef; isolation: "repeatable_read" | "serializable"; maxLifetimeMs: number }, { snapshotId: string; expiresAtMs: NativeCounterU64 }];
  "snapshot.end": [{ session: SessionRef; snapshotId: string }, { ended: boolean }];
  "result.attach": [{ identity: StreamIdentity; afterSequence: NativeCounterU64 | null }, { attached: boolean; terminal: ResultEnvelope | null }];
  "result.frames": [{ identity: StreamIdentity; afterSequence: NativeCounterU64 | null }, { frames: ResultEnvelope[]; hasMore: boolean }];
  "result.ack": [{ identity: StreamIdentity; highestContiguousSequence: NativeCounterU64 }, { acceptedThrough: NativeCounterU64; releasedBytes: NativeCounterU64; availableBytes: NativeCounterU64 }];
  "result.page": [{ identity: StreamIdentity; startRow: NativeCounterU64; limit: number }, ResultPage];
  "result.value.get": [{ identity: StreamIdentity; valueHandle: string; offset: NativeCounterU64; maxBytes: number }, { base64: string; nextOffset: NativeCounterU64 | null }];
}
