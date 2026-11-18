import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { WorkbenchClient } from "../../lib/workbench";
import { nativeU64, type Session, type ShownResult, type WorkbenchOperation } from "../../lib/workbenchTypes";
import { ImportReview } from "./ImportReview";
import { ExportDialog } from "./ExportDialog";
import { MovementProgressPanel } from "./MovementProgressPanel";

const id = (value: number) => `00000000-0000-4000-8000-${String(value).padStart(12, "0")}`;
const session: Session = { workspaceId: id(1), connectionProfileId: id(2), databaseTargetId: id(3), sessionId: id(4), connectionGeneration: nativeU64("9007199254740993"), descriptorRevision: "a".repeat(64), executionEpoch: nativeU64("1"), state: "ready", mode: "read_write", actualContext: { endpoint: "D:\\owned-fixture.sqlite", database: "fixture", role: "", purpose: "source" }, capabilities: {} };
const column = { columnId: id(10), name: "금액", typeName: "decimal", nullable: true, editable: false, readonlyReason: "verified_editing_not_certified" };
const object = { object: { catalogId: id(11), schemaVersion: "a".repeat(64) }, kind: "table", name: "원장" };
const page = { nextCursor: null, completeness: "complete", count: { kind: "unknown" }, asOfMs: "1" };
const operation: WorkbenchOperation = { operationId: id(20), version: nativeU64("1"), kind: "import_job", state: "outcome_unknown", effectSummary: "partial", transactionOutcome: "outcome_unknown", resultCompleteness: "partial", transactionId: null, statements: [], parentOperationId: null, batchIndex: null, acknowledgedAtMs: nativeU64("1"), createdAtMs: nativeU64("1"), updatedAtMs: nativeU64("1") };
function client(handler: (command: string, payload: Record<string, unknown>) => unknown) {
  const requests: { command: string; payload: Record<string, unknown>; attemptToken?: string }[] = [];
  const api = new WorkbenchClient({ available: true, invoke: async (serialized) => { const request = JSON.parse(serialized); requests.push(request); const result = handler(request.command, request.payload) as Record<string, unknown>; return result && "nativeFailure" in result ? { ok: false, requestId: request.requestId, error: result.nativeFailure } : { ok: true, requestId: request.requestId, data: result }; } });
  return { api, requests };
}
const source = { sourceHandle: id(12), displayName: "원본.csv", byteLength: "9007199254740993", columns: [{ sourceKey: "source:0", ordinal: 0, name: "중복 제목", typeHint: null }, { sourceKey: "source:1", ordinal: 1, name: "중복 제목", typeHint: null }] };
const preview = { previewId: id(13), columns: [column], rows: { ...page, items: [{ rowHandle: id(14), cells: [{ kind: "decimal", value: "12345678901234567890.000000001", scale: 9 }] }] }, warnings: [] };
function importProps(api: WorkbenchClient) { return { client: api, session, object, columns: [column], transaction: null, suspended: false, onAdmitted: vi.fn(), onClose: vi.fn() }; }
async function prepareImport(): Promise<void> {
  fireEvent.click(screen.getByRole("button", { name: "원본 파일 선택" })); await screen.findByText(/원본.csv.*9007199254740993/);
  fireEvent.click(screen.getByRole("button", { name: "이 열 연결" })); fireEvent.click(screen.getByRole("button", { name: "형식과 매핑 미리보기" })); await screen.findByRole("grid");
}
describe("native-held data movement UI with explicit mock transport", () => {
  it("requires explicit read-only mode and settled transaction state for a fresh export review", () => {
    const mock = client(() => ({}));
    render(<ExportDialog client={mock.api} session={session} engine="sqlite" shown={null} transaction={null} suspended={false} onAdmitted={vi.fn()} onClose={vi.fn()} />);
    fireEvent.change(screen.getByRole("textbox", { name: "새로 읽을 SQL 원문" }), { target: { value: "SELECT 1" } });
    expect(screen.getByRole("button", { name: "새 읽기 원문 검토" })).toBeDisabled();
    expect(screen.getByText(/새 읽기 내보내기는 읽기 전용 모드/)).toBeVisible();
    expect(mock.requests).toHaveLength(0);
  });
  it.each([["WB_CAPABILITY_UNSUPPORTED", true], ["WB_TRANSPORT_UNAVAILABLE", false]] as const)("keeps snapshot ownership honest after %s", async (code, rejectedBeforeAdmission) => {
    const mock = client((command) => command === "query.review" ? { reviewId: id(130), digest: "a".repeat(64), requiredMode: "read_only", statementCount: 1, warnings: [], expiresAtMs: String(Date.now() + 30000) } : command === "snapshot.begin" ? { snapshotId: id(131), expiresAtMs: String(Date.now() + 60000) } : command === "movement.destination.pick" ? { destination: { destinationHandle: id(132), displayName: "snapshot.csv" } } : command === "export.start" ? { nativeFailure: { code, retryable: false } } : command === "snapshot.end" ? { ended: true } : { released: true });
    const closed = vi.fn(); const mounted = render(<ExportDialog client={mock.api} session={{ ...session, mode: "read_only", capabilities: { snapshot: { supported: true, reason: null } } }} engine="sqlite" shown={null} transaction={null} suspended={false} onAdmitted={vi.fn()} onClose={closed} />);
    fireEvent.change(screen.getByRole("textbox", { name: "새로 읽을 SQL 원문" }), { target: { value: "SELECT 1" } });
    fireEvent.click(screen.getByRole("button", { name: "새 읽기 원문 검토" })); await vi.waitFor(() => expect(mock.requests.some((request) => request.command === "query.review")).toBe(true));
    await vi.waitFor(() => expect(screen.getByRole("checkbox", { name: /검증된 읽기 전용 스냅샷 요청/ })).toBeEnabled()); fireEvent.click(screen.getByRole("checkbox", { name: /검증된 읽기 전용 스냅샷 요청/ })); await vi.waitFor(() => expect(screen.getByRole("button", { name: "일관된 읽기 시작" })).toBeEnabled()); fireEvent.click(screen.getByRole("button", { name: "일관된 읽기 시작" })); await screen.findByRole("button", { name: "스냅샷 종료 후 SQL 다시 검토" });
    fireEvent.click(screen.getByRole("button", { name: "저장 위치 선택" })); await screen.findByText(/snapshot.csv/); fireEvent.click(screen.getByRole("checkbox", { name: /실제 대상·범위·일관성/ }));
    fireEvent.click(screen.getByRole("button", { name: "검토한 내보내기 요청" })); await screen.findByText(/요청을 다시 보내지 않습니다/);
    fireEvent.click(screen.getByRole("button", { name: "Close dialog" })); await vi.waitFor(() => expect(closed).toHaveBeenCalledOnce()); mounted.unmount();
    expect(mock.requests.filter((request) => request.command === "snapshot.end")).toHaveLength(rejectedBeforeAdmission ? 1 : 0);
    expect(mock.requests.filter((request) => request.command === "export.start")).toHaveLength(1);
  });
  it.each([["sqlite", "serializable"], ["postgresql", "repeatable_read"]])("reviews before a %s snapshot and requires a new review and snapshot after editing", async (engine, isolation) => {
    let reviews = 0, snapshots = 0;
    const mock = client((command) => command === "query.review" ? { reviewId: id(100 + ++reviews), digest: "a".repeat(64), requiredMode: "read_only", statementCount: 1, warnings: [], expiresAtMs: String(Date.now() + 30000) } : command === "snapshot.begin" ? { snapshotId: id(110 + ++snapshots), expiresAtMs: String(Date.now() + 60000) } : command === "snapshot.end" ? { ended: true } : command === "movement.destination.pick" ? { destination: { destinationHandle: id(120), displayName: "exact.csv" } } : { operationId: id(121), version: "1", deduplicated: false });
    render(<ExportDialog client={mock.api} session={{ ...session, mode: "read_only", capabilities: { snapshot: { supported: true, reason: null } } }} engine={engine} shown={null} transaction={null} suspended={false} onAdmitted={vi.fn()} onClose={vi.fn()} />);
    const editor = screen.getByRole("textbox", { name: "새로 읽을 SQL 원문" });
    fireEvent.change(editor, { target: { value: "SELECT 1" } }); fireEvent.click(screen.getByRole("checkbox", { name: /검증된 읽기 전용 스냅샷 요청/ }));
    expect(screen.getByRole("button", { name: "일관된 읽기 시작" })).toBeDisabled();
    fireEvent.click(screen.getByRole("button", { name: "새 읽기 원문 검토" })); await vi.waitFor(() => expect(screen.getByRole("button", { name: "일관된 읽기 시작" })).toBeEnabled());
    fireEvent.click(screen.getByRole("button", { name: "일관된 읽기 시작" })); await screen.findByRole("button", { name: "스냅샷 종료 후 SQL 다시 검토" });
    expect(mock.requests.find((request) => request.command === "snapshot.begin")?.payload).toMatchObject({ isolation, maxLifetimeMs: 60000 });
    expect(editor).toBeDisabled(); expect(screen.getByRole("button", { name: "새 읽기 원문 검토" })).toBeDisabled();
    fireEvent.change(editor, { target: { value: "SELECT 999" } }); expect(editor).toHaveValue("SELECT 1");
    fireEvent.click(screen.getByRole("button", { name: "스냅샷 종료 후 SQL 다시 검토" })); await vi.waitFor(() => expect(editor).toBeEnabled());
    fireEvent.change(editor, { target: { value: "SELECT 2" } }); expect(screen.getByRole("button", { name: "일관된 읽기 시작" })).toBeDisabled();
    fireEvent.click(screen.getByRole("button", { name: "새 읽기 원문 검토" })); await vi.waitFor(() => expect(screen.getByRole("button", { name: "일관된 읽기 시작" })).toBeEnabled());
    fireEvent.click(screen.getByRole("button", { name: "저장 위치 선택" })); await screen.findByText(/exact.csv/);
    fireEvent.click(screen.getByRole("checkbox", { name: /실제 대상·범위·일관성/ })); expect(screen.getByRole("button", { name: "검토한 내보내기 요청" })).toBeDisabled();
    fireEvent.click(screen.getByRole("button", { name: "일관된 읽기 시작" })); await screen.findByRole("button", { name: "스냅샷 종료 후 SQL 다시 검토" });
    fireEvent.click(screen.getByRole("button", { name: "검토한 내보내기 요청" })); await vi.waitFor(() => expect(mock.requests.some((request) => request.command === "export.start")).toBe(true));
    expect(mock.requests.find((request) => request.command === "export.start")?.payload.source).toEqual({ kind: "fresh", reviewId: id(102), snapshotId: id(112) });
    expect(mock.requests.filter((request) => request.command === "query.review").map((request) => request.payload.sql)).toEqual(["SELECT 1", "SELECT 2"]);
    expect(mock.requests.filter((request) => request.command === "snapshot.end")).toHaveLength(1);
  });
  it("pulls unconfirmed export evidence again after the status panel reopens without rewriting the file", async () => {
    const exported = { ...operation, kind: "export_job", state: "interrupted", effectSummary: "read", transactionOutcome: "none", resultCompleteness: "partial" };
    const mock = client((command) => command === "operation.get" ? { operation: exported, unchanged: false } : { operation: exported, progress: { bytesProcessed: "200", totalBytes: null, rowsProcessed: "4", confirmedCommittedRows: "0", confirmedCommittedBatches: "0", unresolvedBatches: "0", nextBatchIndex: null }, children: { ...page, items: [] }, file: { displayPath: "D:\\selected.csv", partialDisplayPath: null, format: "csv", state: "unconfirmed", bytesWritten: "200", rowsWritten: "4", sourceCompleteness: "complete", finalized: false } });
    const first = render(<MovementProgressPanel client={mock.api} operationId={operation.operationId} kind="export" />);
    await screen.findByText("파일 완료 미확인", { exact: true }); first.unmount();
    render(<MovementProgressPanel client={mock.api} operationId={operation.operationId} kind="export" />);
    await screen.findByText("파일 완료 미확인", { exact: true });
    expect(screen.queryByText("선택한 내보내기 범위의 파일 완료 확인", { exact: true })).not.toBeInTheDocument();
    expect(mock.requests.filter((request) => request.command === "movement.status").length).toBeGreaterThanOrEqual(2);
    expect(mock.requests.some((request) => ["query.run", "browse.open", "export.start", "movement.partial.remove"].includes(request.command))).toBe(false);
  });
  it.each(["partial", "capped"] as const)("keeps finalized shown-range files distinct from source completeness %s", async (sourceCompleteness) => {
    const exported = { ...operation, kind: "export_job", state: "completed", effectSummary: "read", transactionOutcome: "none", resultCompleteness: sourceCompleteness };
    const mock = client((command) => command === "operation.get" ? { operation: exported, unchanged: false } : { operation: exported, progress: { bytesProcessed: "200", totalBytes: null, rowsProcessed: "4", confirmedCommittedRows: "0", confirmedCommittedBatches: "0", unresolvedBatches: "0", nextBatchIndex: null }, children: { ...page, items: [] }, file: { displayPath: "D:\\selected.csv", partialDisplayPath: null, format: "csv", state: "complete", bytesWritten: "200", rowsWritten: "4", sourceCompleteness, finalized: true } });
    render(<MovementProgressPanel client={mock.api} operationId={operation.operationId} kind="export" />);
    await screen.findByText("선택한 내보내기 범위의 파일 완료 확인", { exact: true });
    expect(screen.getByText(new RegExp(sourceCompleteness === "partial" ? "원래 조회 상태 일부 결과" : "원래 조회 상태 조회 상한 도달"))).toBeInTheDocument();
    expect(mock.requests.some((request) => ["query.run", "browse.open", "export.start"].includes(request.command))).toBe(false);
  });
  it("uses sourceKey mapping and bounded typed preview; unknown admission never auto-resends", async () => {
    const mock = client((command) => command === "movement.source.pick" ? { source } : command === "import.preview" ? preview : command === "operation.prepare" ? { attemptToken: "boot.mac/opaque-import", payloadDigest: "b".repeat(64), expiresAtMs: String(Date.now() + 30000) } : command === "import.start" ? { nativeFailure: { code: "WB_OUTCOME_UNKNOWN", retryable: false, operationId: id(20) } } : { released: true });
    const input = importProps(mock.api); render(<ImportReview {...input} />); await prepareImport();
    expect(mock.requests.find((request) => request.command === "import.preview")?.payload.mapping).toEqual({ "source:0": column.columnId });
    expect(screen.getByRole("gridcell")).toHaveTextContent("12345678901234567890.000000001");
    fireEvent.click(screen.getByRole("checkbox", { name: /실제 대상·매핑·값 형식/ })); fireEvent.click(screen.getByRole("button", { name: "검토한 가져오기 요청" }));
    await screen.findByText(/요청을 다시 보내지 않습니다/); expect(input.onAdmitted).toHaveBeenCalledWith(id(20));
    expect(mock.requests.filter((request) => request.command === "import.start")).toHaveLength(1); expect(mock.requests.find((request) => request.command === "import.start")?.attemptToken).toBe("boot.mac/opaque-import"); expect(screen.getByRole("button", { name: "검토한 가져오기 요청" })).toBeDisabled();
  });
  it("requires a fresh continuation review and keeps acknowledged unknown children blocked", async () => {
    const mock = client((command) => command === "movement.source.pick" ? { source } : command === "import.preview" ? preview : command === "import.continue.review" ? { reviewId: id(31), parentOperationId: id(20), sourceDigest: "a".repeat(64), nextUnsent: { batchIndex: 2, byteStart: "100", rowStart: "2" }, confirmedCommittedRows: "2", unresolvedBatches: "1", warnings: [], expiresAtMs: String(Date.now() + 30000) } : { released: true });
    render(<ImportReview {...importProps(mock.api)} parentOperationId={id(20)} />); await prepareImport();
    expect(mock.requests.some((request) => request.command === "import.continue.review")).toBe(false);
    fireEvent.click(screen.getByRole("button", { name: "다음 미전송 구간의 새 검토 받기" })); await screen.findByText(/커밋 확인 2행 · 미확정 1배치/);
    expect(screen.getByRole("button", { name: "검토한 가져오기 요청" })).toBeDisabled(); expect(mock.requests.some((request) => request.command === "import.continue")).toBe(false);
  });
  it("freezes exact shown range and requires raw formula confirmation without rerunning SQL", async () => {
    const shown: ShownResult = { identity: { windowId: id(5), workspaceId: session.workspaceId, databaseTargetId: session.databaseTargetId, sessionId: session.sessionId, connectionGeneration: session.connectionGeneration, queryRunId: id(6), resultSetId: id(7), streamGeneration: nativeU64("1") }, range: { startRow: nativeU64("9007199254740993"), rowCount: 2 } };
    const mock = client((command) => command === "movement.destination.pick" ? { destination: { destinationHandle: id(40), displayName: "결과.csv" } } : { operationId: id(41), version: "1", deduplicated: false });
    const admitted = vi.fn(); render(<ExportDialog client={mock.api} session={session} engine="sqlite" shown={shown} transaction={null} suspended={false} onAdmitted={admitted} onClose={vi.fn()} />);
    fireEvent.click(screen.getByRole("button", { name: "저장 위치 선택" })); await screen.findByText(/결과.csv/);
    fireEvent.change(screen.getByRole("combobox", { name: "스프레드시트 수식 처리" }), { target: { value: "raw_confirmed" } });
    fireEvent.click(screen.getByRole("checkbox", { name: /실제 대상·범위·일관성/ })); expect(screen.getByRole("button", { name: "검토한 내보내기 요청" })).toBeDisabled();
    fireEvent.click(screen.getByRole("checkbox", { name: /스프레드시트에서 수식으로 실행될/ })); fireEvent.click(screen.getByRole("button", { name: "검토한 내보내기 요청" }));
    await vi.waitFor(() => expect(admitted).toHaveBeenCalledWith(id(41)));
    expect(mock.requests.find((request) => request.command === "export.start")?.payload.source).toEqual({ kind: "shown", ...shown }); expect(mock.requests.some((request) => ["query.run", "query.review", "browse.open"].includes(request.command))).toBe(false);
  });
  it("rejects an unproven-effect fresh read and never labels an unfinalized file complete", async () => {
    const mock = client((command) => command === "query.review" ? { reviewId: id(50), digest: "a".repeat(64), requiredMode: "full", statementCount: 1, warnings: ["function_effects_unproven"], expiresAtMs: String(Date.now() + 30000) } : { destination: { destinationHandle: id(51), displayName: "새조회.json" } });
    render(<ExportDialog client={mock.api} session={{ ...session, mode: "read_only" }} engine="sqlite" shown={null} transaction={null} suspended={false} onAdmitted={vi.fn()} onClose={vi.fn()} />);
    fireEvent.change(screen.getByRole("textbox", { name: "새로 읽을 SQL 원문" }), { target: { value: "SELECT volatile_function()" } }); fireEvent.click(screen.getByRole("button", { name: "새 읽기 원문 검토" }));
    await screen.findByText(/효과나 안전한 재조회 범위를 증명하지 못한 SQL/); expect(screen.getByRole("button", { name: "검토한 내보내기 요청" })).toBeDisabled(); expect(mock.requests.some((request) => request.command === "export.start")).toBe(false);
  });
  it("separates positive commits, unknown children and partial files from completion", async () => {
    const mock = client((command) => command === "operation.get" ? { operation, unchanged: false } : { operation, progress: { bytesProcessed: "200", totalBytes: null, rowsProcessed: "4", confirmedCommittedRows: "2", confirmedCommittedBatches: "1", unresolvedBatches: "1", nextBatchIndex: 2 }, children: { ...page, items: [{ operationId: id(21), batchIndex: 1, state: "outcome_unknown", version: "1", sourceByteStart: "100", sourceByteEnd: "200", sourceRowStart: "2", sourceRowEnd: "4", rowsCommitted: null, errorCode: null }] }, file: { displayPath: "D:\\output.csv", partialDisplayPath: "D:\\output.csv.partial", format: "csv", state: "partial", bytesWritten: "200", rowsWritten: "4", sourceCompleteness: "partial", finalized: false } });
    render(<MovementProgressPanel client={mock.api} operationId={operation.operationId} kind="import" onContinue={vi.fn()} />);
    await screen.findByText(/커밋 확인 2행 \/ 1배치 · 미확정 1배치/); expect(screen.getByRole("button", { name: "새 파일 확인 후 미전송 구간 검토" })).toBeDisabled(); expect(screen.getByText("부분 파일", { exact: true })).toBeInTheDocument(); expect(screen.queryByText("파일 완료 확인", { exact: true })).not.toBeInTheDocument();
    expect(mock.requests.some((request) => ["import.start", "import.continue", "movement.partial.remove"].includes(request.command))).toBe(false);
  });
});
