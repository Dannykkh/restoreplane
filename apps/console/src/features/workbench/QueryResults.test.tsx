import { act, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { WorkbenchClient, workbenchPages } from "../../lib/workbench";
import { nativeU64, type Column, type ResultEnvelope, type Session, type StreamIdentity } from "../../lib/workbenchTypes";
import { QueryResults } from "./QueryResults";

const session: Session = { workspaceId: "workspace", connectionProfileId: "profile", databaseTargetId: "target", sessionId: "session", connectionGeneration: nativeU64("1"), descriptorRevision: "a".repeat(64), executionEpoch: nativeU64("1"), state: "ready", mode: "read_only", actualContext: { endpoint: "fixture.invalid", database: "fixture", role: "reader", purpose: "source" }, capabilities: {} };
const column: Column = { columnId: "column", name: "value", typeName: "text", nullable: false, editable: false, readonlyReason: null };
function stream(queryRunId: string, generation: string): StreamIdentity { return { windowId: "window", workspaceId: "workspace", databaseTargetId: "target", sessionId: "session", connectionGeneration: nativeU64(generation), queryRunId, resultSetId: queryRunId, streamGeneration: nativeU64("1") }; }
function envelope(identity: StreamIdentity, sequence: string, event: ResultEnvelope["event"]): ResultEnvelope { return { protocolVersion: 1, identity, sequence: nativeU64(sequence), encodedBytes: 512, event }; }
describe("query result cleanup across stream generations (mock native transport)", () => {
  it("keeps the new buffer and cache when the disposed old pull resolves late", async () => {
    workbenchPages.clear(); const old = stream("old-query", "1"), next = stream("new-query", "2");
    let resolveOld!: (value: unknown) => void; const commands: { command: string; identity?: StreamIdentity }[] = [];
    const client = new WorkbenchClient({ available: true, invoke: async (serialized) => {
      const request = JSON.parse(serialized); commands.push({ command: request.command, identity: request.payload.identity }); let data: unknown;
      if (request.command === "query.results") { const id = request.payload.queryRunId === old.queryRunId ? old : next; data = { items: [{ identity: id, state: "streaming", completeness: null }], nextCursor: null, completeness: "complete", count: { kind: "unknown" }, asOfMs: nativeU64("1") }; }
      else if (request.command === "result.attach") data = { attached: true, terminal: null };
      else if (request.command === "result.frames" && request.payload.identity.queryRunId === old.queryRunId) data = await new Promise((resolve) => { resolveOld = resolve; });
      else if (request.command === "result.frames") data = request.payload.afterSequence === null ? { frames: [envelope(next, "1", { kind: "schema", columns: [column], consistency: { kind: "live_per_page" } })], hasMore: true } : { frames: [envelope(next, "2", { kind: "rows", startRow: nativeU64("0"), rows: [{ rowHandle: "new-row", cells: [{ kind: "text", value: "new-value" }] }] })], hasMore: false };
      else data = { acceptedThrough: request.payload.highestContiguousSequence, releasedBytes: nativeU64("512"), availableBytes: nativeU64("8388608") };
      return { ok: true, requestId: request.requestId, data };
    } });
    const receipt = (queryRunId: string) => ({ operationId: queryRunId, queryRunId, executionEpoch: nativeU64("1"), version: nativeU64("1"), deduplicated: false });
    const view = render(<QueryResults client={client} session={session} receipt={receipt(old.queryRunId)} onContext={vi.fn()} />);
    await waitFor(() => expect(resolveOld).toBeTypeOf("function"));
    view.rerender(<QueryResults client={client} session={{ ...session, connectionGeneration: next.connectionGeneration }} receipt={receipt(next.queryRunId)} onContext={vi.fn()} />);
    await screen.findByText("new-value"); const retained = workbenchPages.retainedBytes; expect(retained).toBeGreaterThan(0);
    await act(async () => resolveOld({ frames: [envelope(old, "1", { kind: "schema", columns: [{ ...column, name: "old-private-value" }], consistency: { kind: "live_per_page" } })], hasMore: false }));
    expect(screen.getByText("new-value")).toBeVisible(); expect(screen.queryByText("old-private-value")).not.toBeInTheDocument(); expect(workbenchPages.retainedBytes).toBe(retained);
    expect(commands.filter((row) => row.command === "result.ack" && row.identity?.queryRunId === old.queryRunId)).toEqual([]);
    view.unmount(); expect(workbenchPages.retainedBytes).toBe(0);
  });
});
