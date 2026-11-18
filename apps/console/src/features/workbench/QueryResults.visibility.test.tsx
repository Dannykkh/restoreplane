// Staged candidate only. Final path: src/features/workbench/QueryResults.visibility.test.tsx.
// NOT TYPECHECKED / NOT RUN. Uses the real receiver/cache with a mock native transport.
import { act, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { WorkbenchClient, workbenchPages } from "../../lib/workbench";
import { nativeU64, type Column, type ResultEnvelope, type Session, type StreamIdentity } from "../../lib/workbenchTypes";
import { QueryResults } from "./QueryResults";

const session: Session = { workspaceId: "workspace", connectionProfileId: "profile", databaseTargetId: "target", sessionId: "session", connectionGeneration: nativeU64("1"), descriptorRevision: "a".repeat(64), executionEpoch: nativeU64("1"), state: "ready", mode: "read_only", actualContext: { endpoint: "fixture.invalid", database: "fixture", role: "reader", purpose: "source" }, capabilities: {} };
const column: Column = { columnId: "column", name: "value", typeName: "text", nullable: false, editable: false, readonlyReason: null };
const identity: StreamIdentity = { windowId: "window", workspaceId: "workspace", databaseTargetId: "target", sessionId: "session", connectionGeneration: nativeU64("1"), queryRunId: "query", resultSetId: "result", streamGeneration: nativeU64("1") };
function frame(sequence: string, event: ResultEnvelope["event"]): ResultEnvelope { return { protocolVersion: 1, identity, sequence: nativeU64(sequence), encodedBytes: 512, event }; }

describe("hidden query result lifecycle (mock native transport)", () => {
  it("continues the same receiver and ACKs hidden rows, then shows them without reattaching or rerunning SQL", async () => {
    workbenchPages.clear(); const requests: { command: string; payload: Record<string, unknown> }[] = [];
    let deliverHidden!: (value: unknown) => void;
    const client = new WorkbenchClient({ available: true, invoke: async (serialized) => {
      const request = JSON.parse(serialized); requests.push(request); let data: unknown;
      if (request.command === "query.results") data = { items: [{ identity, state: "streaming", completeness: null }], nextCursor: null, completeness: "complete", count: { kind: "unknown" }, asOfMs: nativeU64("1") };
      else if (request.command === "result.attach") data = { attached: true, terminal: null };
      else if (request.command === "result.frames") {
        if (request.payload.afterSequence === null) data = { frames: [frame("1", { kind: "schema", columns: [column], consistency: { kind: "live_per_page" } })], hasMore: true };
        else if (request.payload.afterSequence === "1") data = { frames: [frame("2", { kind: "rows", startRow: nativeU64("0"), rows: [{ rowHandle: "first", cells: [{ kind: "text", value: "보이던 행" }] }] })], hasMore: true };
        else if (request.payload.afterSequence === "2") data = await new Promise((resolve) => { deliverHidden = resolve; });
        else throw new Error("Unexpected extra pull");
      } else if (request.command === "result.ack") data = { acceptedThrough: request.payload.highestContiguousSequence, releasedBytes: nativeU64("512"), availableBytes: nativeU64("8388608") };
      else throw new Error(`Unexpected native command: ${request.command}`);
      return { ok: true, requestId: request.requestId, data };
    } });
    const props = { client, session, receipt: { operationId: "operation", queryRunId: identity.queryRunId, executionEpoch: nativeU64("1"), version: nativeU64("1"), deduplicated: false }, onContext: vi.fn() };
    const view = render(<QueryResults {...props} visible />);
    await screen.findByText("보이던 행"); await waitFor(() => expect(deliverHidden).toBeTypeOf("function"));
    view.rerender(<QueryResults {...props} visible={false} />);
    expect(screen.queryByRole("grid", { hidden: true })).not.toBeInTheDocument();
    await act(async () => deliverHidden({ frames: [frame("3", { kind: "rows", startRow: nativeU64("1"), rows: [{ rowHandle: "hidden", cells: [{ kind: "text", value: "숨김 중 받은 행" }] }] })], hasMore: false }));
    await waitFor(() => expect(requests.some((request) => request.command === "result.ack" && request.payload.highestContiguousSequence === "3")).toBe(true));
    expect(screen.queryByRole("grid", { hidden: true })).not.toBeInTheDocument(); expect(workbenchPages.retainedBytes).toBeGreaterThan(0);
    const pulls = requests.filter((request) => request.command === "result.frames").length;
    view.rerender(<QueryResults {...props} visible />);
    expect(await screen.findByText("숨김 중 받은 행")).toBeVisible(); expect(screen.getByText("보이던 행")).toBeVisible();
    expect(requests.filter((request) => request.command === "result.attach")).toHaveLength(1);
    expect(requests.filter((request) => request.command === "result.frames")).toHaveLength(pulls);
    expect(requests.filter((request) => ["query.run", "browse.open", "query.review"].includes(request.command))).toEqual([]);
    view.unmount(); expect(workbenchPages.retainedBytes).toBe(0);
  });
});
