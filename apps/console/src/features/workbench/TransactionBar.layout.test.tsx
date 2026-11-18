// Staged candidate only. Final path: src/features/workbench/TransactionBar.layout.test.tsx.
// NOT TYPECHECKED / NOT RUN. Reuses QueryWorkspace.test.tsx's mock-native pattern.
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";
import { WorkbenchClient } from "../../lib/workbench";
import { nativeU64, type Session, type Transaction } from "../../lib/workbenchTypes";
import { TransactionBar } from "./TransactionBar";

const session: Session = { workspaceId: "workspace-mock", connectionProfileId: "profile-mock", databaseTargetId: "target-mock", sessionId: "session-mock", connectionGeneration: nativeU64("9007199254740993"), descriptorRevision: "a".repeat(64), executionEpoch: nativeU64("1"), state: "ready", mode: "read_only", actualContext: { endpoint: "mock-db.invalid:5432", database: "mock-ledger", role: "mock-reader", purpose: "source" }, capabilities: {} };
const transaction: Transaction = { transactionId: "transaction-mock", session: { sessionId: session.sessionId, connectionGeneration: session.connectionGeneration }, state: "active", isolation: "repeatable_read", readOnly: true, idleExpiresAtMs: nativeU64(String(Date.now() + 30000)) };

function transport(initial: Transaction | null) {
  const requests: string[] = []; let value = initial;
  const client = new WorkbenchClient({ available: true, invoke: async (serialized) => {
    const request = JSON.parse(serialized); requests.push(request.command);
    if (request.command === "transaction.rollback" && value) value = { ...value, state: "rolled_back" };
    if (!["transaction.get", "transaction.rollback"].includes(request.command)) throw new Error(`Unexpected native command: ${request.command}`);
    return { ok: true, requestId: request.requestId, data: value };
  } });
  return { client, requests };
}

describe("layout execution fence (mock native transport; not database proof)", () => {
  it("blocks new BEGIN/mode selection but leaves explicit status reads usable", async () => {
    const mock = transport(null);
    render(<TransactionBar client={mock.client} session={session} engine="postgresql" transaction={null} onSession={vi.fn()} onTransaction={vi.fn()} executionBlocked />);
    await waitFor(() => expect(mock.requests).toContain("transaction.get"));
    const before = mock.requests.length;
    const mode = screen.getByRole("combobox", { name: "SQL 모드" });
    const begin = screen.getByRole("button", { name: "트랜잭션 시작" });
    expect(mode).toBeDisabled(); expect(begin).toBeDisabled();
    fireEvent.change(mode, { target: { value: "read_write" } }); fireEvent.click(begin);
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(mock.requests.slice(before)).toEqual([]);
    fireEvent.click(screen.getByRole("button", { name: "상태 확인" }));
    await waitFor(() => expect(mock.requests.length).toBe(before + 1));
    expect(mock.requests.every((command) => command === "transaction.get")).toBe(true);
  });

  it("blocks an already-open mode confirmation when the execution fence becomes active", async () => {
    const mock = transport(null), props = { client: mock.client, session, engine: "postgresql", transaction: null, onSession: vi.fn(), onTransaction: vi.fn() };
    const view = render(<TransactionBar {...props} executionBlocked={false} />);
    await waitFor(() => expect(mock.requests).toContain("transaction.get"));
    fireEvent.change(screen.getByRole("combobox", { name: "SQL 모드" }), { target: { value: "read_write" } });
    expect(screen.getByRole("dialog")).toBeVisible();
    view.rerender(<TransactionBar {...props} executionBlocked />);
    const confirm = screen.getByRole("button", { name: "이 대상의 모드 전환" });
    expect(confirm).toBeDisabled(); fireEvent.click(confirm);
    expect(mock.requests.filter((command) => command === "session.mode.set")).toEqual([]);
    fireEvent.click(screen.getByRole("button", { name: "취소" }));
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

  it("keeps rollback and its observed status available while new execution is blocked", async () => {
    const mock = transport(transaction);
    function Harness() { const [value, setValue] = useState<Transaction | null>(transaction); return <TransactionBar client={mock.client} session={session} engine="postgresql" transaction={value} onSession={vi.fn()} onTransaction={setValue} executionBlocked />; }
    render(<Harness />); await screen.findByText("트랜잭션 진행 중");
    const rollback = screen.getByRole("button", { name: "롤백 요청" }); expect(rollback).toBeEnabled(); fireEvent.click(rollback);
    await screen.findByText("롤백 확인됨");
    expect(mock.requests.filter((command) => command === "transaction.rollback")).toHaveLength(1);
    expect(mock.requests.filter((command) => ["transaction.begin", "session.mode.set", "transaction.commit"].includes(command))).toEqual([]);
    expect(screen.getByRole("button", { name: "트랜잭션 시작" })).toBeDisabled();
  });
});
