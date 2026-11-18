// Staged candidate only. Final path: src/screens/Workbench.layout.test.tsx.
// NOT TYPECHECKED / NOT RUN. Real Workbench/EditDraft/OperationPanel; mock native transport.
// SQL editor, relation detail and protection children are outside this layout regression.
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { Workbench } from "./Workbench";
import { WorkbenchClient, workbenchPages } from "../lib/workbench";
import { nativeU64, type CatalogItem, type Column, type EditReviewData, type Session } from "../lib/workbenchTypes";
import type { DatabaseTarget } from "../lib/types";

vi.mock("../lib/api", async (importOriginal) => ({ ...await importOriginal<typeof import("../lib/api")>(), activeWorkspaceId: () => "workspace" }));
vi.mock("../features/workbench/SqlWorkspace", () => ({ SqlWorkspace: () => <section aria-label="SQL 편집기 격리 fixture" /> }));
vi.mock("../features/workbench/RelationMetadata", () => ({ RelationMetadata: () => null }));
vi.mock("../features/workbench/ProtectionPanel", () => ({ ProtectionPanel: () => null }));

const version = "a".repeat(64), operationId = "00000000-0000-4000-8000-000000000071";
const opened: Session = { workspaceId: "workspace", connectionProfileId: "profile", databaseTargetId: "target", sessionId: "session", connectionGeneration: nativeU64("1"), descriptorRevision: version, executionEpoch: nativeU64("1"), mode: "read_only", state: "ready", actualContext: { endpoint: "D:\\fixture.sqlite", database: "fixture", role: "", purpose: "source" }, capabilities: { verified_editing: { supported: true, reason: null } } };
const writable: Session = { ...opened, mode: "read_write", connectionGeneration: nativeU64("2") };
const target: DatabaseTarget = { id: "target", connectionId: "profile", connectionName: "격리 fixture", engine: "sqlite", displayName: "fixture", state: "active", missingSinceMs: null, sourceFingerprint: version, latestRecoveryPointId: null, latestSuccessAtMs: null, latestFailureAtMs: null, artifactCompleteness: "unknown", replicaDurability: "unknown", protectionScope: "none", protectionCompliance: "not_configured", rpoViolation: false, verificationState: "not_configured", verificationOverdue: false, evidenceAtMs: null };
const column: Column = { columnId: "amount", name: "정밀 금액", typeName: "DECIMAL", nullable: true, editable: true, readonlyReason: null };
const schema: CatalogItem = { name: "main", kind: "schema", object: { catalogId: "schema", schemaVersion: version } };
const relation: CatalogItem = { name: "원장", kind: "table", object: { catalogId: "table", schemaVersion: version } };
const row = { rowHandle: "row-native", cells: [{ kind: "decimal" as const, value: "12345678901234567890.000000001", scale: 9 }] };
function review(): EditReviewData { return { draftId: "draft", reviewId: "review", target: { ...writable, connectionAlias: "파일", actualEndpoint: writable.actualContext.endpoint, databaseName: writable.actualContext.database, databaseRole: "", purpose: "source" }, object: relation.object, statementPreviews: ["UPDATE verified_table SET amount = ? WHERE key = ?"], parameterPreview: [{ kind: "decimal", value: "12345678901234567890.000000002", scale: 9 }], changes: [{ kind: "update", rowHandle: row.rowHandle, original: [{ columnId: column.columnId, value: row.cells[0] }], draft: [{ columnId: column.columnId, value: { kind: "decimal", value: "12345678901234567890.000000002", scale: 9 } }], omittedDefaultColumnIds: [] }], warnings: [], expiresAtMs: nativeU64(String(Date.now() + 30000)) }; }

describe("Workbench pane changes with quarantined editing (mock native transport)", () => {
  it("keeps Unknown and acknowledgement accessible on the SQL pane without reapplying the change", async () => {
    workbenchPages.clear(); const requests: string[] = []; let acknowledged = false;
    const operation = () => ({ operationId, version: acknowledged ? "2" : "1", kind: "verified_apply", state: "outcome_unknown", effectSummary: "unknown", transactionOutcome: "outcome_unknown", resultCompleteness: "none", transactionId: null, statements: [], parentOperationId: null, batchIndex: null, acknowledgedAtMs: acknowledged ? "2" : null, createdAtMs: "1", updatedAtMs: acknowledged ? "2" : "1" });
    const client = new WorkbenchClient({ available: true, invoke: async (serialized) => {
      const request = JSON.parse(serialized); requests.push(request.command); let data: unknown;
      if (request.command === "credential.put") data = { credentialRef: "credential" };
      else if (request.command === "session.open") data = opened;
      else if (request.command === "transaction.get") data = null;
      else if (request.command === "session.mode.set") data = writable;
      else if (request.command === "catalog.page") data = { items: request.payload.kind === "schemas" ? [schema] : [relation], nextCursor: null, completeness: "complete", count: { kind: "unknown" }, asOfMs: nativeU64("1") };
      else if (request.command === "browse.open") data = { stream: null, result: { resultSetId: "result-native", columns: [column], items: [row], nextCursor: null, completeness: "complete", count: { kind: "unknown" }, asOfMs: nativeU64("1"), consistency: { kind: "live_per_page" }, schemaVersion: version } };
      else if (request.command === "edit.prepare") data = review();
      else if (request.command === "operation.prepare") data = { attemptToken: "mock-boot.mac/opaque-edit", payloadDigest: "b".repeat(64), expiresAtMs: nativeU64(String(Date.now() + 30000)) };
      else if (request.command === "edit.apply") return { ok: false, requestId: request.requestId, error: { code: "WB_OUTCOME_UNKNOWN", retryable: false, operationId, details: {} } };
      else if (request.command === "operation.get") data = { unchanged: false, operation: operation() };
      else if (request.command === "operation.acknowledge_unknown") { acknowledged = true; data = operation(); }
      else if (request.command === "session.close") data = { closed: true };
      else throw new Error(`Unexpected native command: ${request.command}`);
      return { ok: true, requestId: request.requestId, data };
    } });
    const loadTargets = vi.fn(async () => ({ items: [target], nextCursor: null, totalCount: 1 }));
    const view = render(<Workbench route={{ page: "workbench", target: target.id }} client={client} loadTargets={loadTargets} />);
    const connect = await screen.findByRole("button", { name: "실제 대상 확인하고 연결" });
    await waitFor(() => expect(connect).toBeEnabled()); fireEvent.click(connect);
    const mode = await screen.findByRole("combobox", { name: "SQL 모드" });
    await waitFor(() => expect(mode).toBeEnabled());
    fireEvent.change(mode, { target: { value: "read_write" } });
    fireEvent.click(screen.getByRole("button", { name: "이 대상의 모드 전환" }));
    await waitFor(() => expect(screen.getByRole("combobox", { name: "SQL 모드" })).toHaveValue("read_write"));
    const explorerSummary = screen.getByText("스키마 탐색기", { selector: "summary" });
    if (!(explorerSummary.parentElement as HTMLDetailsElement).open) fireEvent.click(explorerSummary);
    fireEvent.click(await screen.findByRole("treeitem", { name: /main/ }));
    fireEvent.click(await screen.findByRole("treeitem", { name: /원장/ }));
    const grid = await screen.findByRole("grid");
    fireEvent.click(within(grid).getByRole("gridcell", { name: /정밀 금액:/ }));
    fireEvent.change(screen.getByRole("textbox", { name: "새 값 원문" }), { target: { value: "12345678901234567890.000000002" } });
    fireEvent.click(screen.getByRole("button", { name: "이 값을 변경 초안에 추가" }));
    fireEvent.click(screen.getByRole("button", { name: "생성한 변경 검토" }));
    await screen.findByRole("region", { name: "생성한 변경의 대상과 매개변수 검토" });
    fireEvent.click(screen.getByRole("checkbox", { name: /위 실제 대상의 원본/ }));
    fireEvent.click(screen.getByRole("button", { name: "검토한 변경 적용 요청" }));
    await screen.findByText(/이 변경의 결과를 확인하지 못했습니다/);
    await screen.findByRole("checkbox", { name: "결과가 미확인 상태임을 이해했습니다." });
    fireEvent.click(screen.getByRole("button", { name: "SQL 작성·검토" }));
    expect(screen.getByText(/이 변경의 결과를 확인하지 못했습니다/)).toBeVisible();
    const acknowledge = screen.getByRole("checkbox", { name: "결과가 미확인 상태임을 이해했습니다." }); expect(acknowledge).toBeVisible();
    fireEvent.click(acknowledge); fireEvent.click(screen.getByRole("button", { name: "미확인 상태를 인지했음을 기록" }));
    expect(await screen.findByText("인지 기록이 있습니다. 원래 결과는 계속 미확인입니다.")).toBeVisible();
    expect(screen.getByRole("button", { name: "생성한 변경 검토" })).toBeDisabled();
    expect(requests.filter((command) => command === "edit.apply")).toHaveLength(1);
    expect(requests.filter((command) => command === "operation.acknowledge_unknown")).toHaveLength(1);
    expect(requests.filter((command) => command === "browse.open")).toHaveLength(1);
    expect(requests.filter((command) => command === "query.run")).toEqual([]);
    view.unmount(); workbenchPages.clear();
  });
});
