import { describe, expect, it, vi } from "vitest";
import { WorkbenchClient, WorkbenchError, WorkbenchPageCache, WorkbenchStreamReceiver, applyOperationSnapshot, boundedRowTail, cellText, operationHintNeedsPull, readonlyReasonText, workbenchErrorText, type WorkbenchTransport } from "./workbench";
import { nativeU64, type ResultEnvelope, type StreamIdentity, type TypedCell, type WorkbenchOperation } from "./workbenchTypes";

const identity: StreamIdentity = { windowId: "window-mock", workspaceId: "workspace-mock", databaseTargetId: "target-mock", sessionId: "session-mock", connectionGeneration: nativeU64("9007199254740993"), queryRunId: "query-mock", resultSetId: "result-mock", streamGeneration: nativeU64("1") };
function frame(sequence: string, event: ResultEnvelope["event"] = { kind: "rows", startRow: nativeU64("0"), rows: [] }): ResultEnvelope { return { protocolVersion: 1, identity, sequence: nativeU64(sequence), encodedBytes: 256, event }; }
function mockClient(respond: (command: string, payload: unknown) => unknown = () => ({})) {
  const requests: { command: string; payload: Record<string, unknown>; attemptToken?: string }[] = [];
  const transport: WorkbenchTransport = { available: true, invoke: vi.fn(async (request) => {
    const parsed = JSON.parse(request); requests.push(parsed);
    return { ok: true, requestId: parsed.requestId, data: respond(parsed.command, parsed.payload) };
  }) };
  return { client: new WorkbenchClient(transport), transport, requests };
}
describe("Workbench typed native boundary (explicit mock transport)", () => {
  it("translates only known static refusal reasons without exposing arbitrary localized diagnostics", () => {
    expect(readonlyReasonText("invalid_utf8")).toBe("값을 UTF-8 원문으로 해석할 수 없습니다.");
    expect(readonlyReasonText("비밀 데이터 드라이버 진단")).not.toContain("비밀");
    expect(readonlyReasonText("transactional_effects_unproven")).toContain("트랜잭션 경계");
  });
  it("preserves full u64 and SQL numeric text and rejects rounded/corrupt native counters", () => {
    expect(nativeU64("18446744073709551615")).toBe("18446744073709551615");
    for (const invalid of [9007199254740992, "01", "-1", "18446744073709551616", "1.0"]) expect(() => nativeU64(invalid)).toThrow();
    const cells: TypedCell[] = [{ kind: "null" }, { kind: "text", value: "" }, { kind: "i64", value: "-9223372036854775808" }, { kind: "u64", value: "18446744073709551615" }, { kind: "decimal", value: "12345678901234567890.000000001", scale: 9 }, { kind: "bytes", base64: "AP8=" }, { kind: "float", value: "NaN" }, { kind: "temporal", value: "2026-11-17 12:34:56.123456789", typeName: "timestamp", offset: "+09:00", precision: 9 }];
    expect(cells.map(cellText)).toEqual(["NULL", "빈 문자열", "-9223372036854775808", "18446744073709551615", "12345678901234567890.000000001", "base64:AP8=", "NaN", "2026-11-17 12:34:56.123456789"]);
    expect(JSON.parse(JSON.stringify(cells))).toEqual(cells);
  });
  it("reports native unavailable without invoking or pretending success", async () => {
    const transport = { available: false, invoke: vi.fn() };
    await expect(new WorkbenchClient(transport).request("operation.get", { operationId: "mock", knownVersion: null })).rejects.toMatchObject({ code: "WB_NATIVE_UNAVAILABLE" });
    expect(transport.invoke).not.toHaveBeenCalled();
  });
  it("does not reveal raw native transport errors or parse opaque MAC attempt tokens", async () => {
    const failed = new WorkbenchClient({ available: true, invoke: async () => { throw new Error("secret=do-not-display SQL SELECT sensitive"); } });
    const error = await failed.request("operation.get", { operationId: "mock", knownVersion: null }).catch((failure: unknown) => failure);
    expect(workbenchErrorText(error)).not.toMatch(/do-not-display|SELECT|sensitive/);
    const mocked = mockClient(() => ({ unchanged: true, operation: null }));
    await mocked.client.request("operation.get", { operationId: "mock", knownVersion: null }, undefined, "opaque.boot.mac/token");
    expect(mocked.requests[0].attemptToken).toBe("opaque.boot.mac/token");
  });
  it("bounds shared metadata/result LRU and never executes a query on eviction", () => {
    const cache = new WorkbenchPageCache(1024);
    for (let page = 0; page < 10_000; page++) cache.put(`metadata:${page}`, { items: ["row".repeat(20)] });
    expect(cache.retainedBytes).toBeLessThanOrEqual(1024); expect(cache.size).toBeLessThan(4); expect(cache.get("metadata:0")).toBeUndefined();
    cache.put("result", { rows: ["exact"] }); expect(cache.get("result")).toEqual({ rows: ["exact"] });
    expect(() => cache.put("giant", "x".repeat(1024))).toThrow(WorkbenchError);
    expect(() => new WorkbenchPageCache(32 * 1024 * 1024 + 1)).toThrow();
  });
  it("projects only bounded static error evidence without raw driver messages or rows", async () => {
    const operationId = "00000000-0000-4000-8000-000000000001", conflictId = "00000000-0000-4000-8000-000000000002";
    const client = new WorkbenchClient({ available: true, invoke: async (serialized) => ({ ok: false, requestId: JSON.parse(serialized).requestId, error: { code: "WB_OUTCOME_UNKNOWN", retryable: true, operationId, field: "changes", message: "private-sql-canary", details: { conflictId, reason: "nullable_key", actualVersion: "9007199254740993", expectedVersion: "01", rows: ["private-row-canary"], driver: "private-driver-canary" } } }) });
    const failure = await client.request("operation.get", { operationId, knownVersion: null }).catch((value: unknown) => value) as WorkbenchError;
    expect(failure).toMatchObject({ code: "WB_OUTCOME_UNKNOWN", retryable: false, operationId, conflictId, field: "changes", details: { reason: "nullable_key", actualVersion: "9007199254740993" } });
    expect(failure.details.expectedVersion).toBeUndefined();
    expect(JSON.stringify(failure)).not.toMatch(/private-|rows|driver/);
    expect(workbenchErrorText(failure)).toContain("NULL을 허용하는 키");
  });
});
describe("bounded native stream receiver with mock transport", () => {
  function sizedFrame(container: "frame" | "reply", bytes: number) {
    const cell = { kind: "text" as const, value: '한글\r\n"\\😀' + "x".repeat(65527) + "😀\ud800" };
    const value = frame("1", { kind: "rows", startRow: nativeU64("0"), rows: [{ rowHandle: "row-mock", cells: [cell] }] });
    const reply = { frames: [value], hasMore: false };
    const sized = container === "frame" ? value : reply;
    cell.value += "x".repeat(bytes - new TextEncoder().encode(JSON.stringify(sized)).byteLength);
    expect(new TextEncoder().encode(JSON.stringify(sized)).byteLength).toBe(bytes);
    return { value, reply };
  }
  it.each([0, 1])("keeps exact Unicode frame256KiB admission and one-byte oversize refusal (overflow=%s)", async (overflow) => {
    const { value } = sizedFrame("frame", 256 * 1024 + overflow);
    const mocked = mockClient(), applied = vi.fn(), failed = vi.fn();
    const receiver = new WorkbenchStreamReceiver(mocked.client, identity, applied, failed);
    const allocate = vi.spyOn(TextEncoder.prototype, "encode");
    receiver.receive(value); await Promise.resolve();
    expect(allocate).not.toHaveBeenCalled();
    expect(applied).toHaveBeenCalledTimes(overflow ? 0 : 1);
    expect(mocked.requests.filter((request) => request.command === "result.ack")).toHaveLength(overflow ? 0 : 1);
    if (overflow) expect(failed).toHaveBeenCalledWith(expect.objectContaining({ code: "WB_LIMIT" }));
    else expect(failed).not.toHaveBeenCalled();
    receiver.dispose();
  });
  it.each([0, 1])("checks the entire Unicode reply at256KiB before apply/ACK (overflow=%s)", async (overflow) => {
    const { reply } = sizedFrame("reply", 256 * 1024 + overflow);
    const mocked = mockClient((command) => command === "result.attach" ? { attached: true, terminal: null } : command === "result.frames" ? reply : {});
    const applied = vi.fn(), failed = vi.fn(), receiver = new WorkbenchStreamReceiver(mocked.client, identity, applied, failed);
    const allocate = vi.spyOn(TextEncoder.prototype, "encode");
    await receiver.attach(); await vi.waitFor(() => expect(overflow ? failed : applied).toHaveBeenCalledOnce());
    await Promise.resolve();
    expect(allocate).not.toHaveBeenCalled();
    expect(applied).toHaveBeenCalledTimes(overflow ? 0 : 1);
    expect(mocked.requests.filter((request) => request.command === "result.ack")).toHaveLength(overflow ? 0 : 1);
    if (overflow) expect(failed).toHaveBeenCalledWith(expect.objectContaining({ code: "WB_LIMIT" }));
    else expect(failed).not.toHaveBeenCalled();
    receiver.dispose();
  });
  it.each([false, true])("ignores late attach metadata after disposal (attached=%s)", async (attached) => {
    let resolve!: (value: unknown) => void;
    const requests: string[] = [];
    const client = new WorkbenchClient({ available: true, invoke: async (serialized) => { const request = JSON.parse(serialized); requests.push(request.command); return { ok: true, requestId: request.requestId, data: await new Promise((done) => { resolve = done; }) }; } });
    const applied = vi.fn(), failed = vi.fn(), status = vi.fn();
    const receiver = new WorkbenchStreamReceiver(client, identity, applied, failed, status);
    const attaching = receiver.attach(); receiver.dispose(); receiver.dispose();
    resolve({ attached, terminal: frame("1", { kind: "terminal", completeness: "interrupted", rowsSeen: nativeU64("0"), operationVersion: nativeU64("1"), errorCode: "WB_RESULT_EXPIRED" }) });
    await attaching; await Promise.resolve();
    expect(requests).toEqual(["result.attach"]); expect(applied).not.toHaveBeenCalled(); expect(failed).not.toHaveBeenCalled(); expect(status).not.toHaveBeenCalled();
  });
  it("ignores a late pending pull and notices without new apply or ACK after disposal", async () => {
    let resolve!: (value: unknown) => void;
    const requests: string[] = [];
    const client = new WorkbenchClient({ available: true, invoke: async (serialized) => { const request = JSON.parse(serialized); requests.push(request.command); return { ok: true, requestId: request.requestId, data: await new Promise((done) => { resolve = done; }) }; } });
    const applied = vi.fn(), failed = vi.fn(); const receiver = new WorkbenchStreamReceiver(client, identity, applied, failed);
    const notice = { protocolVersion: 1 as const, kind: "frames_available" as const, identity, availableThroughSequence: nativeU64("1") };
    receiver.notice(notice); receiver.dispose(); resolve({ frames: [frame("1")], hasMore: false });
    await Promise.resolve(); await Promise.resolve(); receiver.notice(notice); receiver.receive(frame("1"));
    expect(requests).toEqual(["result.frames"]); expect(applied).not.toHaveBeenCalled(); expect(failed).not.toHaveBeenCalled();
  });
  it("stops frame handling when apply synchronously disposes its receiver", async () => {
    const mocked = mockClient(), failed = vi.fn(); let receiver!: WorkbenchStreamReceiver;
    const applied = vi.fn(() => receiver.dispose()); receiver = new WorkbenchStreamReceiver(mocked.client, identity, applied, failed);
    receiver.receive(frame("1")); receiver.receive(frame("2")); await Promise.resolve();
    expect(applied).toHaveBeenCalledOnce(); expect(mocked.requests).toEqual([]); expect(failed).not.toHaveBeenCalled(); expect(receiver.afterSequence).toBe("0");
  });
  it("does not send an already queued ACK after disposal", async () => {
    const mocked = mockClient(), applied = vi.fn(), failed = vi.fn();
    const receiver = new WorkbenchStreamReceiver(mocked.client, identity, applied, failed);
    receiver.receive(frame("1")); expect(applied).toHaveBeenCalledOnce();
    receiver.dispose(); await Promise.resolve();
    expect(mocked.requests.some((request) => request.command === "result.ack")).toBe(false); expect(failed).not.toHaveBeenCalled();
  });
  it("deduplicates/gap-replays and ACKs only the highest contiguous sequence once", async () => {
    const mocked = mockClient((command) => command === "result.frames" ? { frames: [], hasMore: false } : { acceptedThrough: nativeU64("3"), releasedBytes: nativeU64("768"), availableBytes: nativeU64("8388608") });
    const applied = vi.fn(), failed = vi.fn();
    const receiver = new WorkbenchStreamReceiver(mocked.client, identity, applied, failed);
    receiver.receive(frame("3")); receiver.receive(frame("1")); receiver.receive(frame("2")); receiver.receive(frame("2"));
    await vi.waitFor(() => expect(mocked.requests.filter((request) => request.command === "result.ack")).toHaveLength(1));
    expect(applied.mock.calls.map(([event]) => (event as ResultEnvelope).sequence)).toEqual(["1", "2", "3"]);
    expect(mocked.requests.filter((request) => request.command === "result.frames")).toHaveLength(1);
    expect(mocked.requests.find((request) => request.command === "result.ack")?.payload.highestContiguousSequence).toBe("3");
    expect(mocked.requests.some((request) => request.command.startsWith("browse.") || request.command === "query.run")).toBe(false);
    expect(failed).not.toHaveBeenCalled(); receiver.dispose();
  });
  it("rejects foreign window/generation and oversized frames before applying", () => {
    for (const bad of [{ ...frame("1"), identity: { ...identity, windowId: "foreign" } }, { ...frame("1"), identity: { ...identity, connectionGeneration: nativeU64("9007199254740994") } }, { ...frame("1"), encodedBytes: 262145 }]) {
      const mocked = mockClient(), applied = vi.fn(), failed = vi.fn();
      const receiver = new WorkbenchStreamReceiver(mocked.client, identity, applied, failed); receiver.receive(bad);
      expect(applied).not.toHaveBeenCalled(); expect(failed).toHaveBeenCalledOnce(); receiver.dispose();
    }
  });
  it("bounds unresolved gaps and refuses post-terminal data without acknowledging it", async () => {
    const mocked = mockClient(() => ({ frames: [], hasMore: false })), applied = vi.fn(), failed = vi.fn();
    const receiver = new WorkbenchStreamReceiver(mocked.client, identity, applied, failed);
    for (let sequence = 2; sequence <= 10; sequence++) receiver.receive(frame(String(sequence)));
    expect(failed).toHaveBeenCalledOnce(); expect(applied).not.toHaveBeenCalled();
    const terminalFailed = vi.fn();
    const terminal = new WorkbenchStreamReceiver(mocked.client, identity, applied, terminalFailed);
    terminal.receive(frame("1", { kind: "terminal", completeness: "cancelled", rowsSeen: nativeU64("0"), operationVersion: nativeU64("1"), errorCode: null }));
    await Promise.resolve(); terminal.receive(frame("2"));
    expect(terminalFailed).toHaveBeenCalledOnce(); expect(applied).toHaveBeenCalledOnce(); terminal.dispose();
  });
  it("expires a missing retained gap instead of rerunning SQL", async () => {
    vi.useFakeTimers();
    try {
      const mocked = mockClient(() => ({ frames: [], hasMore: false })), failed = vi.fn();
      const receiver = new WorkbenchStreamReceiver(mocked.client, identity, vi.fn(), failed);
      receiver.receive(frame("2")); await vi.advanceTimersByTimeAsync(5001);
      expect(failed).toHaveBeenCalledWith(expect.objectContaining({ code: "WB_RESULT_EXPIRED" }));
      expect(mocked.requests.every((request) => request.command === "result.frames")).toBe(true); receiver.dispose();
    } finally { vi.useRealTimers(); }
  });
  it("coalesces small notices, pulls one RPC at a time and only ACKs fetched frames", async () => {
    let activePulls = 0, peakPulls = 0;
    const fetched: string[] = [], acked: string[] = [];
    const client = new WorkbenchClient({ available: true, invoke: async (serialized) => {
      const request = JSON.parse(serialized); let data: unknown;
      if (request.command === "result.frames") {
        activePulls++; peakPulls = Math.max(peakPulls, activePulls);
        await new Promise((resolve) => setTimeout(resolve, 0));
        const sequence = BigInt(request.payload.afterSequence ?? "0") + 1n;
        fetched.push(sequence.toString());
        data = { frames: [frame(sequence.toString())], hasMore: sequence < 3n }; activePulls--;
      } else {
        const through = request.payload.highestContiguousSequence;
        expect(fetched).toContain(through); acked.push(through);
        data = { acceptedThrough: nativeU64(through), releasedBytes: nativeU64("256"), availableBytes: nativeU64("8388608") };
      }
      return { ok: true, requestId: request.requestId, data };
    } });
    const applied = vi.fn(), failed = vi.fn(); const receiver = new WorkbenchStreamReceiver(client, identity, applied, failed);
    for (let index = 0; index < 20; index++) receiver.notice({ protocolVersion: 1, kind: "frames_available", identity, availableThroughSequence: nativeU64("3") });
    await vi.waitFor(() => expect(applied).toHaveBeenCalledTimes(3));
    expect(peakPulls).toBe(1); expect(fetched).toEqual(["1", "2", "3"]); expect(new Set(acked).size).toBe(acked.length);
    expect(failed).not.toHaveBeenCalled(); receiver.dispose();
  });
  it("rejects a multi-frame reply before applying data or releasing credit", async () => {
    const mocked = mockClient(() => ({ frames: [frame("1"), frame("2")], hasMore: false }));
    const applied = vi.fn(), failed = vi.fn();
    const receiver = new WorkbenchStreamReceiver(mocked.client, identity, applied, failed);
    receiver.notice({ protocolVersion: 1, kind: "frames_available", identity, availableThroughSequence: nativeU64("2") });
    await vi.waitFor(() => expect(failed).toHaveBeenCalledWith(expect.objectContaining({ code: "WB_LIMIT" })));
    expect(applied).not.toHaveBeenCalled(); expect(mocked.requests.some((request) => request.command === "result.ack")).toBe(false); receiver.dispose();
  });
  it("consumes expired attach terminal as status-only without rows, pulls, ACK or sequence advance", async () => {
    const terminal = frame("42", { kind: "terminal", completeness: "interrupted", rowsSeen: nativeU64("9007199254740993"), operationVersion: nativeU64("1"), errorCode: "WB_RESULT_EXPIRED" });
    const mocked = mockClient(() => ({ attached: false, terminal })), applied = vi.fn(), failed = vi.fn(), status = vi.fn();
    const receiver = new WorkbenchStreamReceiver(mocked.client, identity, applied, failed, status);
    await receiver.attach();
    expect(status).toHaveBeenCalledWith(terminal.event); expect(applied).not.toHaveBeenCalled(); expect(failed).not.toHaveBeenCalled();
    expect(receiver.afterSequence).toBe("0"); expect(mocked.requests.map((request) => request.command)).toEqual(["result.attach"]);
  });
});
describe("exact JSON UTF-8 cache accounting with bounded scratch storage", () => {
  const oldBytes = (value: unknown) => new TextEncoder().encode(JSON.stringify(value)).byteLength;
  it("preserves exact weights across scratch boundaries, surrogate pairs and JSON escapes", () => {
    const values: unknown[] = [undefined, () => undefined, Symbol("fixture"), null, false, "", "한글\r\n줄\n끝", "\ud800x\udfff", "\"\\\b\f\t\n\r"];
    for (const count of [65529, 65530, 65531, 65532, 65533, 65534, 65535, 65536, 131070]) {
      values.push("x".repeat(count) + "😀한\ud800\r\n", { value: "a".repeat(count) + "😀", escaped: "\"\\\t" });
    }
    for (const value of values) {
      const bytes = oldBytes(value) * 2 + 256, cache = new WorkbenchPageCache(bytes);
      cache.put("exact", value); expect(cache.retainedBytes).toBe(bytes);
      expect(cache.get("exact")).toBe(value);
      expect(() => new WorkbenchPageCache(bytes - 1).put("too-large", value)).toThrow(WorkbenchError);
    }
    const cycle: { self?: unknown } = {}; cycle.self = cycle;
    for (const invalid of [1n, cycle]) expect(() => new WorkbenchPageCache().put("invalid", invalid)).toThrow(TypeError);
  });
  it("reuses at most64KiB output storage without per-value encode allocations", () => {
    const into = vi.spyOn(TextEncoder.prototype, "encodeInto"), allocated = vi.spyOn(TextEncoder.prototype, "encode");
    const cache = new WorkbenchPageCache(); cache.put("large", { text: "😀한".repeat(40000) }); cache.put("small", { text: "next" });
    expect(allocated).not.toHaveBeenCalled(); expect(into.mock.calls.length).toBeGreaterThan(2);
    const destinations = into.mock.calls.map(([, destination]) => destination);
    expect(destinations.every((destination) => destination === destinations[0] && destination.byteLength === 64 * 1024)).toBe(true);
  });
  it("keeps identical row-tail boundaries and exact LRU eviction weights", () => {
    const rows = ["한글", "😀".repeat(100), "\ud800\r\n", "last"];
    const cost = (value: unknown) => oldBytes(value) * 2 + 128;
    const budget = cost(rows[2]) + cost(rows[3]);
    expect(boundedRowTail(rows, 200, budget)).toEqual(rows.slice(-2));
    expect(boundedRowTail(rows, 200, budget - 1)).toEqual(rows.slice(-1));
    expect(() => boundedRowTail(["too-big"], 200, 1)).toThrow(WorkbenchError);
    const many = Array.from({ length: 220 }, (_, index) => String(index)); expect(boundedRowTail(many)).toEqual(many.slice(-200));
    const wide = Array.from({ length: 30 }, () => "한".repeat(22000));
    const retained = boundedRowTail(wide); expect(retained.reduce((sum, value) => sum + cost(value), 0)).toBeLessThanOrEqual(2 * 1024 * 1024); expect(cost(wide[0]) * (retained.length + 1)).toBeGreaterThan(2 * 1024 * 1024);
    const weight = oldBytes(rows[1]) * 2 + 256, cache = new WorkbenchPageCache(weight * 2);
    cache.put("a", rows[1]); cache.put("b", rows[1]); cache.get("a"); cache.put("c", rows[1]);
    expect(cache.retainedBytes).toBe(weight * 2); expect(cache.get("b")).toBeUndefined(); expect(cache.get("a")).toBe(rows[1]);
    expect(() => cache.put("a", "x".repeat(weight * 2))).toThrow(WorkbenchError); expect(cache.get("a")).toBe(rows[1]);
  });
});
describe("operation authority snapshot is separate from event hints", () => {
  const base: WorkbenchOperation = { operationId: "operation-mock", version: nativeU64("9007199254740993"), kind: "query", state: "running", effectSummary: "none", transactionOutcome: "none", resultCompleteness: "none", transactionId: null, statements: [], parentOperationId: null, batchIndex: null, acknowledgedAtMs: null, createdAtMs: nativeU64("1"), updatedAtMs: nativeU64("1") };
  it("applies equal-version unchanged:false Unknown overlay after journal disk failure", () => {
    const unknown: WorkbenchOperation = { ...base, state: "outcome_unknown", effectSummary: "unknown", transactionOutcome: "outcome_unknown" };
    expect(operationHintNeedsPull(base.version, base.version)).toBe(false);
    expect(applyOperationSnapshot(base, { unchanged: false, operation: unknown })).toEqual(unknown);
    expect(applyOperationSnapshot(unknown, { unchanged: true, operation: null })).toEqual(unknown);
    expect(applyOperationSnapshot(unknown, { unchanged: false, operation: { ...base, version: nativeU64("1") } })).toEqual(unknown);
  });
});
