import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";
import { QueryTabs } from "./QueryTabs";
import { TransactionBar } from "./TransactionBar";
import { WorkbenchClient } from "../../lib/workbench";
import { nativeU64, type QueryRunReceipt, type Session, type Transaction } from "../../lib/workbenchTypes";
import { TransactionFence } from "../../lib/transactionFence";

// These tests exercise parent review/save/transaction state. Real CodeMirror editing is covered in browser QA.
vi.mock("./SqlCodeEditor", () => ({ default: (props: import("./SqlCodeEditor").SqlCodeEditorProps) => <textarea aria-label="SQL 원문" value={props.sql} onChange={event => props.onChange(event.currentTarget.value)} onKeyDown={event => { if ((event.ctrlKey || event.metaKey) && event.key === "Enter") { event.preventDefault(); props.onExecuteShortcut(); } }} /> }));

const session: Session = { workspaceId: "workspace-mock", connectionProfileId: "profile-mock", databaseTargetId: "target-mock", sessionId: "session-mock", connectionGeneration: nativeU64("9007199254740993"), descriptorRevision: "a".repeat(64), executionEpoch: nativeU64("1"), state: "ready", mode: "read_only", actualContext: { endpoint: "mock-db.invalid:5432", database: "mock-ledger", role: "mock-reader", purpose: "source" }, capabilities: {} };
const transaction: Transaction = { transactionId: "transaction-mock", session: { sessionId: session.sessionId, connectionGeneration: session.connectionGeneration }, state: "active", isolation: "repeatable_read", readOnly: false, idleExpiresAtMs: nativeU64(String(Date.now() + 30000)) };
function createMockClient(handler: (command: string, payload: Record<string, unknown>) => unknown | Promise<unknown>) {
  const requests: { command: string; payload: Record<string, unknown>; attemptToken?: string }[] = [];
  const client = new WorkbenchClient({ available: true, invoke: async (serialized) => { const request = JSON.parse(serialized); requests.push(request); return { ok: true, requestId: request.requestId, data: await handler(request.command, request.payload) }; } });
  return { client, requests };
}
const reviewReply = (warnings: string[] = []) => ({ reviewId: "review-mock", digest: "b".repeat(64), requiredMode: "read_only", statementCount: 2, warnings, expiresAtMs: nativeU64(String(Date.now() + 30000)) });
async function enter(sql: string): Promise<void> { fireEvent.change(await screen.findByRole("textbox", { name: "SQL 원문" }), { target: { value: sql } }); }
describe("independent SQL UI with explicit mock client; no native dispatch proof", () => {
  it.each(["rolled_back", "outcome_unknown"] as const)("keeps a newer %s response when an old active poll arrives late", async (state) => {
    let resolveOld!: (value: unknown) => void;
    const mocked = createMockClient((command) => command === "transaction.get" ? new Promise((resolve) => { resolveOld = resolve; }) : { ...transaction, state });
    const observed = vi.fn();
    function Harness() { const [value, setValue] = useState<Transaction | null>(transaction); return <TransactionBar client={mocked.client} session={session} engine="postgresql" transaction={value} onTransaction={(next) => { observed(next); setValue(next); }} onSession={vi.fn()} />; }
    render(<Harness />);
    await waitFor(() => expect(resolveOld).toBeTypeOf("function"));
    fireEvent.click(screen.getByRole("button", { name: "롤백 요청" }));
    await screen.findByText(state === "rolled_back" ? "롤백 확인됨" : "결과 미확인", { exact: true });
    await act(async () => resolveOld(transaction));
    expect(observed.mock.calls.map(([value]) => value.state)).toEqual([state]);
    expect(screen.getByText(state === "rolled_back" ? "롤백 확인됨" : "결과 미확인", { exact: true })).toBeVisible();
  });
  it("keeps a new transaction when an older no-transaction response arrives after BEGIN", async () => {
    let resolveOld!: (value: unknown) => void;
    const mocked = createMockClient((command) => command === "transaction.get" ? new Promise((resolve) => { resolveOld = resolve; }) : transaction);
    const observed = vi.fn();
    function Harness() { const [value, setValue] = useState<Transaction | null>(null); return <TransactionBar client={mocked.client} session={session} engine="postgresql" transaction={value} onTransaction={(next) => { observed(next); setValue(next); }} onSession={vi.fn()} />; }
    render(<Harness />);
    await waitFor(() => expect(resolveOld).toBeTypeOf("function"));
    fireEvent.click(screen.getByRole("button", { name: "트랜잭션 시작" }));
    await screen.findByText("트랜잭션 진행 중", { exact: true });
    await act(async () => resolveOld(null));
    expect(observed).toHaveBeenCalledOnce(); expect(observed).toHaveBeenLastCalledWith(transaction);
    expect(screen.getByText("트랜잭션 진행 중", { exact: true })).toBeVisible();
  });
  it("invalidates a TransactionBar poll when a sibling tab rolls back the shared transaction", async () => {
    let resolveOld!: (value: unknown) => void;
    const mocked = createMockClient((command) => command === "transaction.get" ? new Promise((resolve) => { resolveOld = resolve; }) : { ...transaction, state: "rolled_back" });
    const fence = new TransactionFence(), observed = vi.fn();
    function Harness() { const [value, setValue] = useState<Transaction | null>(transaction); const accept = (next: Transaction | null) => { fence.invalidate(); observed(next); setValue(next); }; return <><TransactionBar client={mocked.client} session={session} engine="postgresql" transaction={value} onTransaction={accept} onSession={vi.fn()} transactionFence={fence} /><QueryTabs client={mocked.client} session={session} engine="postgresql" transaction={value} onTransaction={accept} transactionFence={fence} /></>; }
    render(<Harness />); await waitFor(() => expect(resolveOld).toBeTypeOf("function"));
    fireEvent.click(screen.getByRole("button", { name: "쿼리 1 탭 닫기" }));
    fireEvent.click(screen.getByRole("radio", { name: "롤백 완료를 확인한 뒤 탭 닫기" }));
    fireEvent.click(screen.getByRole("button", { name: "선택한 처리로 탭 닫기" }));
    await screen.findByText("롤백 확인됨", { exact: true }); await act(async () => resolveOld(transaction));
    expect(observed).toHaveBeenCalledOnce(); expect(observed.mock.calls[0][0].state).toBe("rolled_back");
  });
  it("reviews the exact original SQL without LIMIT injection and keeps dispatch unavailable", async () => {
    const mocked = createMockClient(() => reviewReply());
    render(<QueryTabs client={mocked.client} session={session} engine="postgresql" transaction={null} onTransaction={vi.fn()} />);
    const sql = "/* 원문 */ SELECT 'a;b', 18446744073709551615;\nSELECT 2;"; await enter(sql);
    fireEvent.click(screen.getByRole("button", { name: "원문 검토" }));
    await screen.findByRole("region", { name: "네이티브 SQL 검토 결과" });
    expect(mocked.requests[0]).toMatchObject({ command: "query.review", payload: { sql, purpose: "run" } });
    expect(screen.getByRole("textbox", { name: "SQL 원문" })).toHaveValue(sql);
    expect(screen.getByRole("button", { name: "검토한 SQL 실행" })).toBeDisabled();
    expect(mocked.requests.some((request) => request.command === "query.run")).toBe(false);
  });
  it("ignores Korean IME Ctrl+Enter and explicitly previews the full Explain wire SQL", async () => {
    const mocked = createMockClient(() => reviewReply());
    render(<QueryTabs client={mocked.client} session={session} engine="sqlite" transaction={null} onTransaction={vi.fn()} />);
    const sql = "SELECT '한글;원문'"; await enter(sql); const editor = screen.getByRole("textbox", { name: "SQL 원문" });
    fireEvent.compositionStart(editor); fireEvent.keyDown(editor, { key: "Enter", ctrlKey: true, isComposing: true }); fireEvent.compositionEnd(editor);
    expect(mocked.requests).toHaveLength(0);
    fireEvent.click(screen.getByRole("button", { name: "실행 계획 검토" }));
    expect(mocked.requests).toHaveLength(0); expect(screen.getByRole("textbox", { name: "SQL 원문" })).toHaveValue(sql);
    fireEvent.click(screen.getByRole("button", { name: "이 SQL 전체 검토" }));
    await waitFor(() => expect(mocked.requests[0]?.payload.sql).toBe(`EXPLAIN QUERY PLAN ${sql}`));
    expect(screen.getByRole("textbox", { name: "SQL 원문" })).toHaveValue(sql);
  });
  it("does not accept a review returned after the source SQL changed", async () => {
    let resolve!: (value: unknown) => void;
    const mocked = createMockClient(() => new Promise((done) => { resolve = done; }));
    render(<QueryTabs client={mocked.client} session={session} engine="postgresql" transaction={null} onTransaction={vi.fn()} />);
    await enter("SELECT 1"); fireEvent.click(screen.getByRole("button", { name: "원문 검토" }));
    await enter("SELECT 2"); await act(async () => resolve(reviewReply()));
    expect(screen.queryByRole("region", { name: "네이티브 SQL 검토 결과" })).not.toBeInTheDocument();
    expect(screen.getByRole("textbox", { name: "SQL 원문" })).toHaveValue("SELECT 2");
  });
  it("invalidates target-generation binding while preserving volatile source text", async () => {
    const mocked = createMockClient(() => reviewReply());
    const view = render(<QueryTabs client={mocked.client} session={session} engine="postgresql" transaction={null} onTransaction={vi.fn()} />);
    await enter("SELECT '보존할 초안'"); fireEvent.click(screen.getByRole("button", { name: "원문 검토" }));
    await screen.findByRole("region", { name: "네이티브 SQL 검토 결과" });
    view.rerender(<QueryTabs client={mocked.client} session={{ ...session, connectionGeneration: nativeU64("9007199254740994") }} engine="postgresql" transaction={null} onTransaction={vi.fn()} />);
    await screen.findByText(/연결 대상 또는 세대가 바뀌었습니다/);
    expect(screen.getByRole("textbox", { name: "SQL 원문" })).toHaveValue("SELECT '보존할 초안'");
    expect(screen.getByRole("button", { name: "원문 검토" })).toBeDisabled();
    expect(screen.queryByRole("region", { name: "네이티브 SQL 검토 결과" })).not.toBeInTheDocument();
  });
  it("requires native warnings confirmation before an explicitly injected mock dispatcher", async () => {
    const mocked = createMockClient(() => reviewReply(["data_change"]));
    const receipt: QueryRunReceipt = { operationId: "operation-mock", queryRunId: "query-run-mock", executionEpoch: nativeU64("9007199254740995"), version: nativeU64("1"), deduplicated: false };
    const mockDispatch = vi.fn(async () => receipt);
    render(<QueryTabs client={mocked.client} session={session} engine="postgresql" transaction={null} onTransaction={vi.fn()} onExecute={mockDispatch} />);
    await enter("SELECT 1"); fireEvent.click(screen.getByRole("button", { name: "원문 검토" }));
    const warning = await screen.findByRole("checkbox", { name: "데이터를 변경합니다." });
    expect(screen.getByRole("button", { name: "검토한 SQL 실행" })).toBeDisabled();
    fireEvent.click(warning); fireEvent.click(screen.getByRole("button", { name: "검토한 SQL 실행" }));
    await screen.findByText(/SQL 실행 요청 접수/); expect(mockDispatch).toHaveBeenCalledOnce();
    expect(screen.getByText(/실행 세대 9007199254740995/)).toBeInTheDocument();
    expect(screen.queryByText("실행 완료", { exact: true })).not.toBeInTheDocument();
  });
  it("requires explicit encrypted-save consent and never stores raw SQL in localStorage", async () => {
    const storage = vi.spyOn(Storage.prototype, "setItem"), mocked = createMockClient(() => ({ queryId: "saved-mock", version: nativeU64("9007199254740993") }));
    render(<QueryTabs client={mocked.client} session={session} engine="postgresql" transaction={null} onTransaction={vi.fn()} />);
    const sql = "SELECT 'query-private-canary'"; await enter(sql);
    fireEvent.click(screen.getByRole("button", { name: "이름을 정해 저장" }));
    fireEvent.change(screen.getByRole("textbox", { name: "저장할 이름" }), { target: { value: "개인 조회" } });
    expect(screen.getByRole("button", { name: "암호화해서 저장" })).toBeDisabled(); expect(mocked.requests).toHaveLength(0);
    fireEvent.click(screen.getByRole("checkbox", { name: /이 이름과 SQL을 현재 사용자용 암호화 저장소에 저장/ }));
    fireEvent.click(screen.getByRole("button", { name: "암호화해서 저장" }));
    await waitFor(() => expect(mocked.requests[0]).toMatchObject({ command: "saved_query.put", payload: { sql, name: "개인 조회", expectedVersion: null } }));
    expect(storage).not.toHaveBeenCalled();
  });
  it("separates discarding a draft from keeping an active transaction open", async () => {
    const mocked = createMockClient(() => ({})), onTransaction = vi.fn();
    render(<QueryTabs client={mocked.client} session={session} engine="postgresql" transaction={transaction} onTransaction={onTransaction} />);
    await enter("UPDATE table_name SET value=1"); fireEvent.click(screen.getByRole("button", { name: "쿼리 1 탭 닫기" }));
    expect(screen.getByRole("radio", { name: "트랜잭션과 연결 유지" })).toBeChecked();
    expect(screen.getByRole("button", { name: "선택한 처리로 탭 닫기" })).toBeDisabled();
    fireEvent.click(screen.getByRole("checkbox", { name: "이 탭의 저장하지 않은 SQL 초안을 버립니다." }));
    fireEvent.click(screen.getByRole("button", { name: "선택한 처리로 탭 닫기" }));
    expect(screen.queryByRole("textbox", { name: "SQL 원문" })).not.toBeInTheDocument();
    expect(onTransaction).not.toHaveBeenCalled(); expect(mocked.requests).toHaveLength(0);
  });
  it("prepares COMMIT with an opaque native token and does not turn admission into commit proof", async () => {
    let committedRequest = false;
    const mocked = createMockClient((command) => {
      if (command === "transaction.get") return { ...transaction, state: committedRequest ? "commit_requested" : "active" };
      if (command === "operation.prepare") return { attemptToken: "boot.mac/opaque-commit", payloadDigest: "c".repeat(64), expiresAtMs: nativeU64(String(Date.now() + 30000)) };
      if (command === "transaction.commit") { committedRequest = true; return { operationId: transaction.transactionId, version: nativeU64("2"), deduplicated: false }; }
      return transaction;
    });
    function TransactionMockHarness() { const [value, setValue] = useState<Transaction | null>(transaction); return <TransactionBar client={mocked.client} session={session} engine="postgresql" transaction={value} onTransaction={setValue} onSession={vi.fn()} />; }
    render(<TransactionMockHarness />); fireEvent.click(screen.getByRole("button", { name: "COMMIT 검토" }));
    expect(screen.getByRole("button", { name: "확인하고 COMMIT 요청" })).toBeDisabled();
    fireEvent.click(screen.getByRole("checkbox", { name: "위 대상의 이 트랜잭션에 COMMIT을 요청합니다." }));
    fireEvent.click(screen.getByRole("button", { name: "확인하고 COMMIT 요청" }));
    await screen.findByText("COMMIT 확인 중", { exact: true });
    expect(mocked.requests.find((request) => request.command === "transaction.commit")?.attemptToken).toBe("boot.mac/opaque-commit");
    expect(screen.queryByText("COMMIT 확인됨", { exact: true })).not.toBeInTheDocument();
  });
});
