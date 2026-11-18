import { fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { WorkbenchClient } from "../../lib/workbench";
import { nativeU64, type Column, type EditReviewData, type Session } from "../../lib/workbenchTypes";
import { ConflictPanel } from "./ConflictPanel";
import { EditDraft } from "./EditDraft";
import { parseEditValue } from "./EditCellInput";

const version = "a".repeat(64), operationId = "00000000-0000-4000-8000-000000000071", conflictId = "00000000-0000-4000-8000-000000000072";
const session: Session = { workspaceId: "workspace", connectionProfileId: "profile", databaseTargetId: "target", sessionId: "session", connectionGeneration: nativeU64("9007199254740993"), descriptorRevision: version, mode: "read_write", state: "ready", executionEpoch: nativeU64("1"), actualContext: { endpoint: "D:\\fixture.sqlite", database: "fixture", role: "", purpose: "source" }, capabilities: { verified_editing: { supported: true, reason: null } } };
const column: Column = { columnId: "amount", name: "정밀 금액", typeName: "DECIMAL", nullable: true, editable: true, readonlyReason: null };
const object = { name: "원장", kind: "table", object: { catalogId: "table", schemaVersion: version } };
const selectedRow = { rowHandle: "row-native", cells: [{ kind: "decimal" as const, value: "12345678901234567890.000000001", scale: 9 }] };
function review(): EditReviewData { return { draftId: "draft", reviewId: "review", target: { ...session, connectionAlias: "파일", actualEndpoint: session.actualContext.endpoint, databaseName: session.actualContext.database, databaseRole: "", purpose: "source" }, object: object.object, statementPreviews: ["UPDATE verified_table SET amount = ? WHERE key = ?"], parameterPreview: [{ kind: "decimal", value: "12345678901234567890.000000002", scale: 9 }], changes: [{ kind: "update", rowHandle: selectedRow.rowHandle, original: [{ columnId: column.columnId, value: selectedRow.cells[0] }], draft: [{ columnId: column.columnId, value: { kind: "decimal", value: "12345678901234567890.000000002", scale: 9 } }], omittedDefaultColumnIds: [] }], warnings: [], expiresAtMs: nativeU64(String(Date.now() + 30000)) }; }
function mockClient(unknown = false) {
  const requests: { command: string; payload: Record<string, unknown>; attemptToken?: string }[] = [];
  const client = new WorkbenchClient({ available: true, invoke: async (serialized) => {
    const request = JSON.parse(serialized); requests.push(request);
    let data: unknown;
    if (request.command === "edit.prepare") data = review();
    else if (request.command === "operation.prepare") data = { attemptToken: "mock-boot.mac/opaque-edit", payloadDigest: "b".repeat(64), expiresAtMs: nativeU64(String(Date.now() + 30000)) };
    else if (request.command === "edit.apply") return { ok: false, requestId: request.requestId, error: { code: unknown ? "WB_OUTCOME_UNKNOWN" : "WB_ROW_CONFLICT", retryable: false, operationId, details: { conflictId } } };
    else if (request.command === "operation.get") data = { unchanged: false, operation: { operationId, version: "1", kind: "verified_apply", state: unknown ? "outcome_unknown" : "rolled_back", effectSummary: "none", transactionOutcome: unknown ? "outcome_unknown" : "rolled_back", resultCompleteness: "none", transactionId: null, statements: [], parentOperationId: null, batchIndex: null, acknowledgedAtMs: null, createdAtMs: "1", updatedAtMs: "1" } };
    else if (request.command === "edit.conflict.get") data = { conflictId, draftId: "draft", reviewId: "review", operationId, basisObject: object.object, currentSchemaVersion: version, observedAtMs: "1794898800000", rows: [{ changeIndex: 0, rowHandle: selectedRow.rowHandle, current: { kind: "missing" } }] };
    else data = { discarded: true };
    return { ok: true, requestId: request.requestId, data };
  } }); return { client, requests };
}
function props(client: WorkbenchClient) { return { client, session, object, resultSetId: "result-native", columns: [column], selectedRow, transaction: null, onDirty: vi.fn(), onRefresh: vi.fn(), suspended: false }; }
describe("verified editing frontend with explicit mock transport", () => {
  it("keeps exact integer/decimal/bytes/temporal values and distinguishes NULL from empty", () => {
    expect(parseEditValue("u64", "18446744073709551615", column)).toEqual({ kind: "u64", value: "18446744073709551615" });
    expect(parseEditValue("i64", "-9223372036854775808", column)).toEqual({ kind: "i64", value: "-9223372036854775808" });
    expect(() => parseEditValue("u64", "18446744073709551616", column)).toThrow();
    expect(parseEditValue("decimal", selectedRow.cells[0].value, column, selectedRow.cells[0])).toEqual(selectedRow.cells[0]);
    expect(parseEditValue("text", "", column)).toEqual({ kind: "text", value: "" }); expect(parseEditValue("null", "", column)).toEqual({ kind: "null" });
    expect(parseEditValue("bytes", "AP8=", column)).toEqual({ kind: "bytes", base64: "AP8=" });
    const temporal = { kind: "temporal" as const, value: "2026-11-17 12:34:56.123456789", typeName: "timestamp", precision: 9, offset: "+09:00" };
    expect(parseEditValue("temporal", temporal.value, column, temporal)).toEqual(temporal);
  });
  it("keeps uncertified/native-readonly capabilities disabled with a specific Korean reason", () => {
    const mock = mockClient(); render(<EditDraft {...props(mock.client)} session={{ ...session, capabilities: { verified_editing: { supported: false, reason: "codec_key_transaction_certification_required" } } }} />);
    expect(screen.getByText("값 형식·고유 키·트랜잭션의 편집 인증이 필요합니다.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "이 값을 변경 초안에 추가" })).toBeDisabled(); expect(mock.requests).toHaveLength(0);
  });
  it("preserves a generation-bound draft after explicit review/apply becomes Unknown and never resends", async () => {
    const mock = mockClient(true), input = props(mock.client); const mounted = render(<EditDraft {...input} />);
    fireEvent.change(screen.getByRole("textbox", { name: "새 값 원문" }), { target: { value: "12345678901234567890.000000002" } }); fireEvent.click(screen.getByRole("button", { name: "이 값을 변경 초안에 추가" }));
    fireEvent.click(screen.getByRole("button", { name: "생성한 변경 검토" }));
    await screen.findByRole("region", { name: "생성한 변경의 대상과 매개변수 검토" });
    expect(mock.requests.some((item) => item.command === "edit.apply")).toBe(false);
    fireEvent.click(screen.getByRole("checkbox", { name: /위 실제 대상의 원본/ })); fireEvent.click(screen.getByRole("button", { name: "검토한 변경 적용 요청" }));
    await screen.findByText(/이 변경의 결과를 확인하지 못했습니다/);
    expect(mock.requests.filter((item) => item.command === "edit.apply")).toHaveLength(1);
    expect(mock.requests.find((item) => item.command === "edit.apply")?.attemptToken).toBe("mock-boot.mac/opaque-edit");
    expect(screen.getByRole("button", { name: "생성한 변경 검토" })).toBeDisabled(); expect(screen.getByText(/등록한 변경 1건/)).toBeInTheDocument();
    mounted.rerender(<EditDraft {...input} resultSetId="result-new" />);
    expect(screen.getByText(/이전 초안을 보존하며 그대로 적용하지 않습니다/)).toBeInTheDocument();
  });
  it("loads only captured conflict observations on an explicit action; missing is distinct from unavailable", async () => {
    const mock = mockClient(), refresh = vi.fn(); render(<ConflictPanel client={mock.client} session={{ sessionId: session.sessionId, connectionGeneration: session.connectionGeneration }} review={review()} conflictId={conflictId} onRefresh={refresh} />);
    expect(mock.requests).toHaveLength(0); expect(screen.getByText(/현재 관측값 미조회/)).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "실패 시점의 관측값 읽기" })); await screen.findByText("관측 시점에 행이 없었습니다.");
    expect(mock.requests.map((item) => item.command)).toEqual(["edit.conflict.get"]); expect(refresh).not.toHaveBeenCalled();
    expect(within(screen.getByRole("region", { name: "충돌 관측과 보존한 초안" })).getByText(/실시간 현재값을 보장하지 않습니다/)).toBeInTheDocument();
  });
  it("pairs multiple INSERT observations by immutable changeIndex even with null row handles", async () => {
    const draft = review(); draft.changes = ["첫째", "둘째"].map((value) => ({ kind: "insert", rowHandle: null, original: [], draft: [{ columnId: column.columnId, value: { kind: "text", value } }], omittedDefaultColumnIds: [] }));
    const client = new WorkbenchClient({ available: true, invoke: async (serialized) => ({ ok: true, requestId: JSON.parse(serialized).requestId, data: { conflictId, draftId: draft.draftId, reviewId: draft.reviewId, operationId, basisObject: object.object, currentSchemaVersion: version, observedAtMs: "1794898800000", rows: [{ changeIndex: 1, rowHandle: null, current: { kind: "unavailable", reason: "comparison_unproven" } }, { changeIndex: 0, rowHandle: null, current: { kind: "missing" } }] } }) });
    render(<ConflictPanel client={client} session={session} review={draft} conflictId={conflictId} onRefresh={vi.fn()} />);
    fireEvent.click(screen.getByRole("button", { name: "실패 시점의 관측값 읽기" }));
    await screen.findByText(/둘째/); expect(screen.queryByText(/첫째/)).not.toBeInTheDocument(); expect(screen.getByText(/관측값을 확보하지 못했습니다/)).toBeInTheDocument();
    fireEvent.change(screen.getByRole("combobox", { name: "관측한 행" }), { target: { value: "1" } });
    expect(screen.getByText(/첫째/)).toBeInTheDocument(); expect(screen.getByText("관측 시점에 행이 없었습니다.")).toBeInTheDocument();
  });
});
