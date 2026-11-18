import { afterEach, describe, expect, it, vi } from "vitest";
import { activeWorkspaceId, getWorkItem, getWorkbenchRestoreWorkItem, runBackupScopeDiscovery, saveBackupScope } from "./api";
import { WorkbenchClient } from "./workbench";
import { getProtectionSelection, restoredInspection, selectionProof, workbenchProtection, type WorkbenchProtectionGateway } from "./workbenchProtection";
import { nativeU64, type Session, type WorkbenchSelection } from "./workbenchTypes";
import type { WorkItemStatus } from "./types";

const id = (value: number) => `00000000-0000-4000-8000-${String(value).padStart(12, "0")}`;
const hash = "a".repeat(64);
const session: Session = { workspaceId: activeWorkspaceId(), connectionProfileId: id(2), databaseTargetId: id(3), sessionId: id(4), connectionGeneration: nativeU64("9007199254740993"), descriptorRevision: hash, executionEpoch: nativeU64("1"), mode: "read_only", state: "ready", actualContext: { endpoint: "fixture.invalid:5432", database: "fixture", role: "reader", purpose: "source" }, capabilities: {} };
const selection: WorkbenchSelection = { signatureVersion: 1, expectedDescriptorRevision: hash, items: [{ engineObjectId: "relation:one", schemaName: "public", objectName: "one", structureSignature: "b".repeat(64) }, { engineObjectId: "relation:two", schemaName: "public", objectName: "two", structureSignature: "c".repeat(64) }] };
const originalNative = Object.getOwnPropertyDescriptor(window, "__TAURI_INTERNALS__");
const originalNativeFlag = Object.getOwnPropertyDescriptor(globalThis, "isTauri");
afterEach(() => { if (originalNative) Object.defineProperty(window, "__TAURI_INTERNALS__", originalNative); else Reflect.deleteProperty(window, "__TAURI_INTERNALS__"); if (originalNativeFlag) Object.defineProperty(globalThis, "isTauri", originalNativeFlag); else Reflect.deleteProperty(globalThis, "isTauri"); });
describe("protection metadata and native service wire with explicit Tauri stub; no authorization proof", () => {
  it("reuses scoped paginated service projection and exact snapshot detail without losing native fingerprints", async () => {
    const invoke = vi.fn(async (_command: string, args: { command: string; payload: Record<string, unknown> }) => ({ success: true, result: args.command === "backup_scope.list" ? { items: [{ id: id(30), databaseTargetId: session.databaseTargetId, connectionId: session.connectionProfileId, mode: "whole_database", state: "active", sourceFingerprint: hash, selectorHashBlake3: "b".repeat(64), latestRecoveryPointId: id(31) }], nextCursor: "opaque-next", totalCount: 1 } : { snapshot: { id: id(31) }, items: [], nextCursor: null } }));
    Object.defineProperty(window, "__TAURI_INTERNALS__", { configurable: true, value: { invoke } });
    Object.defineProperty(globalThis, "isTauri", { configurable: true, value: true });
    const scopes = await workbenchProtection.scopes!(session.databaseTargetId, null);
    expect(scopes.items[0].sourceFingerprint).toBe(hash); expect(scopes.items[0].selectorHashBlake3).toBe("b".repeat(64));
    await workbenchProtection.snapshot!(id(31));
    expect(invoke.mock.calls.map((call) => call[1].command)).toEqual(["backup_scope.list", "snapshot.get"]);
    expect(invoke.mock.calls[0][1].payload).toMatchObject({ databaseTargetId: session.databaseTargetId, cursor: null, limit: 20 });
    expect(invoke.mock.calls[1][1].payload).toMatchObject({ snapshotId: id(31), section: "replicas", cursor: null, limit: 1 });
  });
  it("refuses browser default service previews and never invents backup success", async () => {
    Reflect.deleteProperty(window, "__TAURI_INTERNALS__");
    await expect(workbenchProtection.work(id(5), true)).rejects.toMatchObject({ code: "WB_NATIVE_UNAVAILABLE" });
  });
  it("preserves ordinary getWorkItem array-map calls and only explicitly requests restore proof", async () => {
    const invoke = vi.fn(async (_command: string, args: { command: string; payload: Record<string, unknown> }) => ({ success: true, result: { workItemId: args.payload.workItemId, state: "succeeded" } }));
    Object.defineProperty(window, "__TAURI_INTERNALS__", { configurable: true, value: { invoke } });
    await Promise.all([id(5), id(6)].map(getWorkItem)); await getWorkbenchRestoreWorkItem(id(7));
    expect(invoke.mock.calls[0][1].payload).toEqual({ workItemId: id(5), workspaceId: activeWorkspaceId() }); expect(invoke.mock.calls[1][1].payload).toEqual({ workItemId: id(6), workspaceId: activeWorkspaceId() }); expect(invoke.mock.calls[2][1].payload).toEqual({ workItemId: id(7), includeWorkbenchRestoreProof: true, workspaceId: activeWorkspaceId() });
  });
  it("forwards exact optional selection/digest while retaining legacy default bodies and keys", async () => {
    const invoke = vi.fn(async (_command: string, args: { command: string; payload: Record<string, unknown>; idempotencyKey: string }) => ({ success: true, result: args.command === "backup_scope.discovery_run" ? { operationRunId: id(8), workItemId: id(9), state: "queued" } : { scopeId: id(10), databaseTargetId: session.databaseTargetId, mode: "explicit_tables", state: "active", selectedObjectCount: 2, selectorHashBlake3: hash, sourceFingerprint: hash } }));
    Object.defineProperty(window, "__TAURI_INTERNALS__", { configurable: true, value: { invoke } });
    await runBackupScopeDiscovery(session.databaseTargetId, "legacy-key"); await runBackupScopeDiscovery(session.databaseTargetId, "selection-key", selection);
    await saveBackupScope({ databaseTargetId: session.databaseTargetId, displayName: "scope", mode: "explicit_tables", discoveryRunId: id(8), engineObjectIds: selection.items.map((item) => item.engineObjectId), expectedWorkbenchSelectionDigest: hash }, "save-key");
    expect(invoke.mock.calls[0][1].payload).toEqual({ databaseTargetId: session.databaseTargetId, workspaceId: activeWorkspaceId() }); expect(invoke.mock.calls[0][1].idempotencyKey).toBe("legacy-key");
    expect(invoke.mock.calls[1][1].payload.workbenchSelection).toEqual(selection); expect(invoke.mock.calls[2][1].payload.expectedWorkbenchSelectionDigest).toBe(hash); expect(invoke.mock.calls[2][1].idempotencyKey).toBe("save-key");
  });
  it("keeps per-table structure hashes distinct and refuses retargeted/mismatched selection proof", async () => {
    const objects = [{ catalogId: id(11), schemaVersion: "b".repeat(64) }, { catalogId: id(12), schemaVersion: "c".repeat(64) }];
    const client = new WorkbenchClient({ available: true, invoke: async (serialized) => ({ ok: true, requestId: JSON.parse(serialized).requestId, data: { scope: { workspaceId: session.workspaceId, connectionProfileId: session.connectionProfileId, databaseTargetId: session.databaseTargetId }, workbenchSelection: selection } }) });
    await expect(getProtectionSelection(client, session, objects)).resolves.toEqual(selection);
    const work = { state: "succeeded", operationRunId: id(8), result: { workbenchSelection: { selectionDigest: hash, descriptorRevision: hash, items: selection.items, verifiedAtMs: nativeU64(String(Date.now())) } } } as unknown as WorkItemStatus;
    expect(selectionProof(work, selection, id(8)).items).toEqual(selection.items);
    expect(() => selectionProof(work, { ...selection, items: [{ ...selection.items[0], structureSignature: "d".repeat(64) }] }, id(8))).toThrow();
  });
  it("requires completed actual restore proof without falling back to an ordinary target", async () => {
    const gateway = { work: vi.fn(async () => ({ workItemId: id(20), state: "succeeded", workbenchRestoreProof: { restoreWorkItemId: id(20), restoreRunId: id(21), connectionProfileId: id(22), databaseName: "restored", engine: "sqlite", serverIdentity: null, actualDatabaseIdentity: "sqlite-file-object-id:owned-fixture", completedAtMs: nativeU64("9007199254740993") } } as WorkItemStatus)) } as unknown as WorkbenchProtectionGateway;
    const proof = await restoredInspection(gateway, id(20)); expect(proof.completedAtMs).toBe("9007199254740993"); expect(gateway.work).toHaveBeenCalledWith(id(20), true);
    await expect(restoredInspection({ ...gateway, work: async () => ({ workItemId: id(20), state: "failed", workbenchRestoreProof: proof } as WorkItemStatus) }, id(20))).rejects.toMatchObject({ code: "WB_BINDING_UNPROVEN" });
    await expect(restoredInspection({ ...gateway, work: async () => ({ workItemId: id(20), state: "succeeded", workbenchRestoreProof: null } as WorkItemStatus) }, id(20))).rejects.toMatchObject({ code: "WB_BINDING_UNPROVEN" });
  });
});
