/** Explicit browser-only mock harness. This file is not an application build entry. */
import React from "react";
import ReactDOM from "react-dom/client";
import "pretendard/dist/web/variable/pretendardvariable-dynamic-subset.css";
import "@fontsource/space-grotesk/600.css";
import "@fontsource/jetbrains-mono/400.css";
import "@fontsource/jetbrains-mono/500.css";
import "../src/styles/tokens.css";
import "../src/styles/global.css";
import "../src/App.css";
import { App } from "../src/App";
import { WorkbenchClient, type WorkbenchTransport } from "../src/lib/workbench";
import { nativeU64, type CatalogItem, type Column, type ConflictObservation, type EditReviewData, type ResultEnvelope, type ResultNotice, type ResultPage, type SavedQuery, type Session, type StreamIdentity, type Transaction, type WorkbenchOperation } from "../src/lib/workbenchTypes";
import type { DatabaseTarget } from "../src/lib/types";
import type { WorkbenchProtectionGateway } from "../src/lib/workbenchProtection";

const schemaVersion = "a".repeat(64);
const editingFixture = new URLSearchParams(window.location.search).get("edit") === "1";
const snapshotExportFixture = new URLSearchParams(window.location.search).get("exportSnapshot") === "1";
let exportReviewSequence = 0, exportSnapshotSequence = 0;
const protectionFixture = new URLSearchParams(window.location.search).get("protection") === "1";
const restoreFixture = new URLSearchParams(window.location.search).get("restore") === "1";
const fixtureEngine = new URLSearchParams(window.location.search).get("engine") ?? (protectionFixture ? "postgresql" : "sqlite");
const serviceRequests: { command: string; payload: unknown; idempotencyKey?: string }[] = [];
let protectedSelection: import("../src/lib/workbenchTypes").WorkbenchSelection;
const object = (index: number) => ({ catalogId: `00000000-0000-4000-8000-${String(index).padStart(12, "0")}`, schemaVersion });
const columns: Column[] = Array.from({ length: 100 }, (_, index) => ({ columnId: object(index + 100).catalogId, name: index === 0 ? "정밀 금액" : `열_${index}`, typeName: "decimal", nullable: true, editable: false, readonlyReason: "읽기 전용 연결" }));
if (editingFixture) columns[0] = { ...columns[0], editable: true, readonlyReason: null };
const requests: { command: string; payload: Record<string, unknown> }[] = [];
const wideSqlFixture = new URLSearchParams(window.location.search).get("wideSql") === "1";
const streamTerminalSequence = wideSqlFixture ? 6n : 3n;
const mockControl = { metadataDelayMs: 0, holdWideFrame: false };
const notices: ResultNotice[] = [];
let session: Session;
let transaction: Transaction | null = null;
const savedQueries = new Map<string, SavedQuery>();
let operation: WorkbenchOperation | null = null;
const operations = new Map<string, WorkbenchOperation>();
const movement = new Map<string, unknown>();
let editReview: EditReviewData;
let conflictObservation: ConflictObservation;
let snapshot: { snapshotId: string; expiresAtMs: ReturnType<typeof nativeU64> } | null = null;
let streamIdentity: StreamIdentity;
let fetchedThrough = 0n;
function mockFrame(sequence: bigint): ResultEnvelope {
  const rowOffset = wideSqlFixture && sequence >= 2n && sequence < streamTerminalSequence ? Number(sequence - 2n) * 50 : 0;
  const page = result(rowOffset);
  const reorderedColumns = columns.map((column) => ({ readonlyReason: column.readonlyReason, editable: column.editable, nullable: column.nullable, typeName: column.typeName, name: column.name, columnId: column.columnId }));
  const event: ResultEnvelope["event"] = sequence === 1n ? { kind: "schema", columns: reorderedColumns, consistency: page.consistency } : sequence < streamTerminalSequence ? { kind: "rows", startRow: nativeU64(String(rowOffset)), rows: page.items } : { kind: "terminal", completeness: "complete", rowsSeen: nativeU64(wideSqlFixture ? "200" : "50"), operationVersion: nativeU64("1"), errorCode: null };
  const frame: ResultEnvelope = { protocolVersion: 1, identity: streamIdentity, sequence: nativeU64(sequence.toString()), encodedBytes: 1, event };
  for (let index = 0; index < 3; index++) frame.encodedBytes = new TextEncoder().encode(JSON.stringify(frame)).byteLength;
  return frame;
}
function result(start = 0): ResultPage {
  return { items: Array.from({ length: 50 }, (_, index) => ({ rowHandle: `row-${start + index}`, cells: columns.map((_, column) => column === 0 ? { kind: "decimal" as const, value: "12345678901234567890.000000001", scale: 9 } : column === 1 ? { kind: "u64" as const, value: "18446744073709551615" } : column === 2 ? { kind: "null" as const } : column === 3 ? { kind: "text" as const, value: "" } : { kind: "text" as const, value: `${start + index}:${column}` }) })), nextCursor: start === 0 ? "opaque-next-page" : null, completeness: start === 0 ? "limited" : "complete", count: { kind: "exact", value: nativeU64("100000") }, asOfMs: nativeU64("1794898800000"), resultSetId: "00000000-0000-4000-8000-000000000080", columns, consistency: snapshot ? { kind: "transaction_snapshot", ...snapshot } : { kind: "live_per_page" }, schemaVersion };
}
const transport: WorkbenchTransport = { available: true, invoke: async (serialized, onResult) => {
  const request = JSON.parse(serialized); requests.push(request); let data: unknown;
  if (request.command.startsWith("catalog.") && mockControl.metadataDelayMs > 0) await new Promise(resolve => setTimeout(resolve, mockControl.metadataDelayMs));
  switch (request.command) {
    case "credential.put": data = { credentialRef: "00000000-0000-4000-8000-000000000090" }; break;
    case "session.open": if (request.payload.restoreWorkItemId && (request.payload.scope.databaseTargetId !== "restored-target" || new URLSearchParams(window.location.search).get("restoreMismatch") === "1")) return { ok: false, requestId: request.requestId, error: { code: "WB_BINDING_UNPROVEN", retryable: false } }; session = { ...request.payload.scope, sessionId: "00000000-0000-4000-8000-000000000091", connectionGeneration: nativeU64("9007199254740993"), descriptorRevision: schemaVersion, executionEpoch: nativeU64("1"), state: "ready", mode: "read_only", actualContext: { endpoint: request.payload.restoreWorkItemId ? "D:\\test-fixture\\restored.sqlite" : protectionFixture ? "owned-fixture.invalid:5432" : "D:\\test-fixture\\workbench.sqlite", database: request.payload.restoreWorkItemId ? "restored-ledger" : "mock-ledger", role: protectionFixture ? "reader" : "", purpose: request.payload.restoreWorkItemId ? "restored" : "source" }, capabilities: { ...(snapshotExportFixture ? { snapshot: { supported: true, reason: null } } : {}), verified_editing: { supported: editingFixture, reason: editingFixture ? null : "codec_key_transaction_certification_required" } } }; data = session; break;
    case "session.reconnect": session = { ...session, connectionGeneration: nativeU64((BigInt(session.connectionGeneration) + 1n).toString()) }; data = session; break;
    case "session.mode.set": session = { ...session, mode: request.payload.mode }; data = session; break;
    case "transaction.get": data = transaction; break;
    case "transaction.begin": transaction = { transactionId: object(96).catalogId, session: request.payload.session, state: "active", isolation: request.payload.isolation, readOnly: request.payload.readOnly, idleExpiresAtMs: nativeU64(String(Date.now() + 30000)) }; data = transaction; break;
    case "transaction.rollback": transaction = { ...transaction!, state: "rolled_back" }; data = transaction; break;
    case "query.review": data = { reviewId: object(97 + (snapshotExportFixture ? exportReviewSequence++ : 0)).catalogId, digest: "d".repeat(64), requiredMode: "read_only", statementCount: 1, warnings: [], expiresAtMs: nativeU64(String(Date.now() + 30000)) }; break;
    case "operation.prepare": data = { attemptToken: "mock-boot.mac/opaque-attempt", payloadDigest: "e".repeat(64), expiresAtMs: nativeU64(String(Date.now() + 30000)) }; break;
    case "query.run": {
      fetchedThrough = 0n;
      streamIdentity = { windowId: "mock-window", workspaceId: session.workspaceId, databaseTargetId: session.databaseTargetId, sessionId: session.sessionId, connectionGeneration: session.connectionGeneration, queryRunId: object(95).catalogId, resultSetId: result().resultSetId, streamGeneration: nativeU64("1") };
      operation = { operationId: object(94).catalogId, version: nativeU64("1"), kind: "query", state: "running", effectSummary: "read", transactionOutcome: "none", resultCompleteness: "none", transactionId: null, statements: [{ ordinal: 0, transactionId: null, digest: "d".repeat(64), outcome: "succeeded", effectClass: "read", effectEvidence: "read_only", rowsMatched: null, rowsChanged: null, version: nativeU64("1"), errorCode: null }], parentOperationId: null, batchIndex: null, acknowledgedAtMs: null, createdAtMs: nativeU64(String(Date.now())), updatedAtMs: nativeU64(String(Date.now())) };
      data = { operationId: operation.operationId, version: operation.version, deduplicated: false, queryRunId: streamIdentity.queryRunId, executionEpoch: nativeU64("9007199254740995") }; break;
    }
    case "query.results": data = { items: [{ identity: streamIdentity, state: "terminal", completeness: "complete" }], nextCursor: null, completeness: "complete", count: { kind: "unknown" }, asOfMs: nativeU64(String(Date.now())) }; break;
    case "query.cancel": operation = { ...operation!, state: "outcome_unknown", effectSummary: "unknown", transactionOutcome: "outcome_unknown" }; data = { cancelRequested: true, settled: false }; break;
    case "operation.get": { const observed = operations.get(request.payload.operationId) ?? operation; data = request.payload.knownVersion === observed?.version && observed?.state !== "outcome_unknown" ? { operation: null, unchanged: true } : { operation: observed, unchanged: false }; break; }
    case "operation.list": data = { items: operation && (!request.payload.state || operation.state === request.payload.state) ? [operation] : [], nextCursor: null, completeness: "complete", count: { kind: "unknown" }, asOfMs: nativeU64(String(Date.now())) }; break;
    case "operation.statements": data = { items: operation?.statements ?? [], nextCursor: null, completeness: "complete", count: { kind: "unknown" }, asOfMs: nativeU64(String(Date.now())) }; break;
    case "operation.acknowledge_unknown": operation = { ...operation!, acknowledgedAtMs: nativeU64(String(Date.now())), version: nativeU64("2") }; data = operation; break;
    case "edit.prepare": editReview = { draftId: object(88).catalogId, reviewId: object(87).catalogId, target: { workspaceId: session.workspaceId, connectionProfileId: session.connectionProfileId, databaseTargetId: session.databaseTargetId, connectionAlias: "테스트 파일", actualEndpoint: session.actualContext.endpoint, databaseName: session.actualContext.database, databaseRole: "", purpose: "source" }, object: request.payload.object, statementPreviews: ["UPDATE verified_mock_table SET amount = ? WHERE native_key = ?"], parameterPreview: request.payload.changes.flatMap((change: { values?: unknown[] }) => change.values?.map((pair: unknown) => (pair as { value: unknown }).value) ?? []), changes: request.payload.changes.map((change: { kind: string; rowHandle?: string; values?: unknown[] }) => ({ kind: change.kind, rowHandle: change.rowHandle ?? null, original: change.kind === "insert" ? [] : [{ columnId: columns[0].columnId, value: result().items[0].cells[0] }], draft: change.values ?? [], omittedDefaultColumnIds: [] })), warnings: [], expiresAtMs: nativeU64(String(Date.now() + 30000)) }; data = editReview; break;
    case "edit.apply": {
      const observedAtMs = nativeU64(String(Date.now()));
      operation = { operationId: object(86).catalogId, version: nativeU64("1"), kind: "verified_apply", state: "rolled_back", effectSummary: "none", transactionOutcome: "rolled_back", resultCompleteness: "none", transactionId: null, statements: [], parentOperationId: null, batchIndex: null, acknowledgedAtMs: null, createdAtMs: observedAtMs, updatedAtMs: observedAtMs };
      conflictObservation = { conflictId: object(85).catalogId, draftId: editReview.draftId, reviewId: editReview.reviewId, operationId: operation.operationId, basisObject: editReview.object, currentSchemaVersion: schemaVersion, observedAtMs, rows: editReview.changes.map((change, changeIndex) => ({ changeIndex, rowHandle: change.rowHandle, current: { kind: "present", values: [{ columnId: columns[0].columnId, value: { kind: "decimal", value: "9.99", scale: 2 } }] } })) };
      return { ok: false, requestId: request.requestId, error: { code: "WB_ROW_CONFLICT", retryable: false, operationId: operation.operationId, details: { conflictId: conflictObservation.conflictId, reason: "row_changed" } } };
    }
    case "edit.conflict.get": data = conflictObservation; break;
    case "edit.discard": data = { discarded: true }; break;
    case "movement.source.pick": data = { source: { sourceHandle: object(74).catalogId, displayName: "대량-테스트.csv", byteLength: nativeU64("9007199254740993"), columns: [{ sourceKey: "source:0", ordinal: 0, name: "금액", typeHint: "decimal" }, { sourceKey: "source:1", ordinal: 1, name: "금액", typeHint: "text" }] } }; break;
    case "movement.destination.pick": data = { destination: { destinationHandle: object(75).catalogId, displayName: "D:\\fixture\\선택범위.csv" } }; break;
    case "movement.file.release": data = { released: true }; break;
    case "import.preview": data = { previewId: object(73).catalogId, columns: [columns[0]], rows: { ...result(), items: result().items.map((row) => ({ ...row, cells: [row.cells[0]] })), nextCursor: null }, warnings: ["data_change"] }; break;
    case "import.start": case "export.start": {
      const importing = request.command === "import.start", operationId = object(importing ? 72 : 71).catalogId;
      const now = nativeU64(String(Date.now()));
      const job: WorkbenchOperation = { operationId, version: nativeU64("1"), kind: importing ? "import_job" : "export_job", state: importing ? "outcome_unknown" : "completed", effectSummary: importing ? "partial" : "read", transactionOutcome: importing ? "outcome_unknown" : "none", resultCompleteness: importing ? "partial" : "complete", transactionId: null, statements: [], parentOperationId: null, batchIndex: null, acknowledgedAtMs: null, createdAtMs: now, updatedAtMs: now };
      operations.set(operationId, job);
      movement.set(operationId, { operation: job, progress: { bytesProcessed: "200", totalBytes: null, rowsProcessed: "4", confirmedCommittedRows: importing ? "2" : "0", confirmedCommittedBatches: importing ? "1" : "0", unresolvedBatches: importing ? "1" : "0", nextBatchIndex: importing ? 2 : null }, children: { items: importing ? [{ operationId: object(70).catalogId, batchIndex: 0, state: "committed", version: "1", sourceByteStart: "0", sourceByteEnd: "100", sourceRowStart: "0", sourceRowEnd: "2", rowsCommitted: "2", errorCode: null }, { operationId: object(69).catalogId, batchIndex: 1, state: "outcome_unknown", version: "1", sourceByteStart: "100", sourceByteEnd: "200", sourceRowStart: "2", sourceRowEnd: "4", rowsCommitted: null, errorCode: null }] : [], nextCursor: null, completeness: "complete", count: { kind: "unknown" }, asOfMs: now }, file: importing ? null : { displayPath: "D:\\fixture\\선택범위.csv", partialDisplayPath: "D:\\fixture\\선택범위.csv.partial", format: "csv", state: "complete", bytesWritten: "200", rowsWritten: "50", sourceCompleteness: "complete", finalized: false } });
      data = { operationId, version: "1", deduplicated: false }; break;
    }
    case "movement.status": data = movement.get(request.payload.operationId); break;
    case "saved_query.list": data = { items: [...savedQueries.values()].map(({ queryId, name }) => ({ queryId, name })), nextCursor: null, completeness: "complete", count: { kind: "unknown" }, asOfMs: nativeU64(String(Date.now())) }; break;
    case "saved_query.put": {
      const queryId = request.payload.queryId ?? object(99).catalogId;
      const saved = { queryId, name: request.payload.name, sql: request.payload.sql, version: nativeU64(String(BigInt(savedQueries.get(queryId)?.version ?? "0") + 1n)) };
      savedQueries.set(queryId, saved); data = { queryId, version: saved.version }; break;
    }
    case "saved_query.get": data = savedQueries.get(request.payload.queryId); break;
    case "saved_query.delete": data = { deleted: savedQueries.delete(request.payload.queryId) }; break;
    case "snapshot.begin": snapshot = { snapshotId: object(92 + (snapshotExportFixture ? exportSnapshotSequence++ : 0)).catalogId, expiresAtMs: nativeU64(String(Date.now() + 60000)) }; data = snapshot; break;
    case "snapshot.end": snapshot = null; data = { ended: true }; break;
    case "session.close": data = { state: "closed", transaction: null }; break;
    case "catalog.page": {
      const root = request.payload.parent === null;
      const start = request.payload.page.cursor === null ? 0 : 100;
      const items: CatalogItem[] = root ? [{ object: object(1), name: "main", kind: "schema" }] : Array.from({ length: 100 }, (_, index) => ({ object: object(index + start + 2), name: `테이블_${String(index + start).padStart(5, "0")}`, kind: "table" }));
      data = { items, nextCursor: root ? null : start === 0 ? "opaque-metadata-next" : null, completeness: root ? "complete" : "limited", count: { kind: "exact", value: nativeU64(root ? "1" : "10000") }, asOfMs: nativeU64("1794898800000") }; break;
    }
    case "catalog.graph": data = { items: [{ nodeId: "node-one", name: "원장", relation: { ...object(2), schemaVersion: "b".repeat(64) }, edges: [{ to: "node-two", name: "고객 참조" }] }, { nodeId: "node-two", name: "고객", relation: { ...object(3), schemaVersion: "c".repeat(64) }, edges: [] }], nextCursor: null, completeness: "complete", count: { kind: "exact", value: nativeU64("2") }, asOfMs: nativeU64("1794898800000") }; break;
    case "catalog.describe": data = { items: request.payload.part === "foreign_keys" ? [{ kind: "foreign_key", keyId: object(98).catalogId, columns: [columns[0].columnId], referencedRelation: { ...object(3), schemaVersion: "c".repeat(64) }, referencedColumns: [columns[0].columnId] }] : columns.map((column, ordinal) => ({ kind: "column", column, ordinal })), nextCursor: null, completeness: "complete", count: { kind: "unknown" }, asOfMs: nativeU64("1794898800000") }; break;
    case "protection.selection.get": protectedSelection = { signatureVersion: 1, expectedDescriptorRevision: session.descriptorRevision, items: request.payload.objects.map((item: { catalogId: string }) => ({ engineObjectId: `native-relation:${item.catalogId}`, schemaName: "main", objectName: "테이블_00000", structureSignature: "b".repeat(64) })) }; data = { scope: { workspaceId: session.workspaceId, connectionProfileId: session.connectionProfileId, databaseTargetId: session.databaseTargetId }, workbenchSelection: protectedSelection }; break;
    case "browse.open": {
      const stream = new URLSearchParams(window.location.search).get("stream") === "1";
      streamIdentity = { windowId: "mock-window", workspaceId: session.workspaceId, databaseTargetId: session.databaseTargetId, sessionId: session.sessionId, connectionGeneration: session.connectionGeneration, queryRunId: object(95).catalogId, resultSetId: result().resultSetId, streamGeneration: nativeU64("1") };
      data = { result: result(), stream: stream ? streamIdentity : null };
      if (stream) { const notice: ResultNotice = { protocolVersion: 1, kind: "frames_available", identity: streamIdentity, availableThroughSequence: nativeU64("3") }; notices.push(notice); onResult?.(notice); }
      break;
    }
    case "result.attach": data = { attached: true, terminal: mockFrame(streamTerminalSequence) }; break;
    case "result.frames": { const next = BigInt(request.payload.afterSequence ?? "0") + 1n; while (wideSqlFixture && next === 3n && mockControl.holdWideFrame) await new Promise(resolve => setTimeout(resolve, 20)); fetchedThrough = next > fetchedThrough ? next : fetchedThrough; data = { frames: next <= streamTerminalSequence ? [mockFrame(next)] : [], hasMore: next < streamTerminalSequence }; break; }
    case "result.ack": {
      if (BigInt(request.payload.highestContiguousSequence) > fetchedThrough) return { ok: false, requestId: request.requestId, error: { code: "WB_ACK_INVALID", retryable: false } };
      data = { acceptedThrough: request.payload.highestContiguousSequence, releasedBytes: nativeU64("0"), availableBytes: nativeU64("8388608") }; break;
    }
    case "browse.page": data = result(50); break;
    default: return { ok: false, requestId: request.requestId, error: { code: "WB_CAPABILITY_UNSUPPORTED", retryable: false } };
  }
  return { ok: true, requestId: request.requestId, data };
} };
const target = { id: "mock-target", connectionId: "mock-profile", connectionName: "테스트 연결", engine: fixtureEngine, displayName: "모의 원장", state: "active", latestSuccessAtMs: null } as DatabaseTarget;
const restoredTarget = { ...target, id: "restored-target", connectionId: "restored-profile", connectionName: "등록한 복원 파일", engine: "sqlite", displayName: "등록한 복원 원장" };
const gateway: WorkbenchProtectionGateway = {
  async scopes(targetId, cursor) { serviceRequests.push({ command: "backup_scope.list", payload: { databaseTargetId: targetId, cursor, limit: 20 } }); return { items: [{ id: object(52).catalogId, displayName: "선택 원장 범위", databaseTargetId: target.id, connectionId: target.connectionId, engine: target.engine, state: "active", sourceDrift: false, sourceFingerprint: "b".repeat(64), selectorHashBlake3: "c".repeat(64), latestRecoveryPointId: object(53).catalogId, latestBackupState: "succeeded" } as import("../src/lib/types").BackupScope], nextCursor: null }; },
  async snapshot(snapshotId) { serviceRequests.push({ command: "snapshot.get", payload: { snapshotId, section: "replicas", cursor: null, limit: 1 } }); return { id: snapshotId, sourceConnectionId: target.connectionId, sourceTargetIdentity: "b".repeat(64), engine: target.engine, state: "complete", artifactCompleteness: "complete", signatureStatus: "valid", requiredPartCount: 1, presentPartCount: 1, committedAtMs: Date.now() - 600000 } as import("../src/lib/types").Snapshot; },
  async discover(targetId, selection, key) { serviceRequests.push({ command: "backup_scope.discovery_run", payload: { databaseTargetId: targetId, workbenchSelection: selection }, idempotencyKey: key }); return { operationRunId: object(50).catalogId, workItemId: object(51).catalogId, state: "queued" }; },
  async work(workItemId, includeRestoreProof) {
    serviceRequests.push({ command: "work.get", payload: { workItemId, ...(includeRestoreProof ? { includeWorkbenchRestoreProof: true } : {}) } });
    if (includeRestoreProof) return { workItemId, state: "succeeded", operationKind: "restore", result: null, workbenchRestoreProof: { restoreWorkItemId: workItemId, restoreRunId: object(61).catalogId, connectionProfileId: "historical-restore-provenance-profile", databaseName: "restored-ledger", engine: "sqlite", serverIdentity: null, actualDatabaseIdentity: "sqlite-file-object-id:owned-restored-fixture", completedAtMs: nativeU64(String(Date.now())) } } as import("../src/lib/types").WorkItemStatus;
    return { workItemId, state: "succeeded", operationRunId: object(50).catalogId, operationKind: "connection_discovery", result: { workbenchSelection: { selectionDigest: "d".repeat(64), descriptorRevision: protectedSelection.expectedDescriptorRevision, items: protectedSelection.items, verifiedAtMs: nativeU64(String(Date.now())) } } } as import("../src/lib/types").WorkItemStatus;
  },
  async save(request, key) { serviceRequests.push({ command: "backup_scope.save", payload: request, idempotencyKey: key }); return { scopeId: object(52).catalogId, databaseTargetId: request.databaseTargetId, mode: request.mode, state: "active", selectedObjectCount: request.engineObjectIds.length, selectorHashBlake3: schemaVersion, sourceFingerprint: schemaVersion }; },
};
Object.assign(window, { workbenchMockRequests: requests, workbenchMockNotices: notices, workbenchMockServiceRequests: serviceRequests });
window.location.hash = restoreFixture ? `/workbench?action=restore-inspect:${object(60).catalogId}` : "/workbench";
ReactDOM.createRoot(document.getElementById("root")!).render(<App workbenchDependencies={{ client: new WorkbenchClient(transport), protection: gateway, loadTargets: async (request) => ({ items: [target, ...(restoreFixture ? [restoredTarget] : [])].filter((item) => !request.connectionId || item.connectionId === request.connectionId), nextCursor: null, totalCount: 1 }) }} />);

Object.assign(window, { workbenchMockControl: mockControl });
