import { act, renderHook } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { WorkbenchClient } from "../../lib/workbench";
import { nativeU64, type Session } from "../../lib/workbenchTypes";
import { explainWireSql } from "./QueryEditor";
import { quoteCompletionIdentifier, useSqlCompletions } from "./useSqlCompletions";

let sequence = 1000;
const id = (n: number) => `00000000-0000-4000-8000-${String(n).padStart(12, "0")}`;
function fixture(handler: (command: string, payload: Record<string, any>) => unknown | Promise<unknown>) {
  const requests: { command: string; payload: Record<string, any> }[] = [];
  const client = new WorkbenchClient({ available: true, invoke: async serialized => {
    const request = JSON.parse(serialized); requests.push(request);
    return { ok: true, requestId: request.requestId, data: await handler(request.command, request.payload) };
  } });
  const session: Session = { workspaceId: id(++sequence), connectionProfileId: id(2), databaseTargetId: id(3), sessionId: id(++sequence), connectionGeneration: nativeU64("1"), descriptorRevision: "a".repeat(64), executionEpoch: nativeU64("0"), mode: "read_only", state: "ready", actualContext: { endpoint: "owned-mock", database: "fixture", role: "reader", purpose: "source" }, capabilities: {} };
  const schema = { object: { catalogId: id(4), schemaVersion: "a".repeat(64) }, name: "main", kind: "schema" };
  const relation = { object: { catalogId: id(5), schemaVersion: "a".repeat(64) }, name: "ledger", kind: "table" };
  return { client, session, schema, relation, requests };
}
const page = (items: unknown[], nextCursor: string | null = null) => ({ items, nextCursor, completeness: "complete", count: { kind: "unknown" }, asOfMs: "1" });
const column = (name: string, ordinal = 0) => ({ kind: "column", ordinal, column: { columnId: id(100 + ordinal), name, typeName: "text", nullable: true, editable: false, readonlyReason: "read_only" } });
const request = () => ({ prefix: "", signal: new AbortController().signal });

describe("SQL edit assistance without execution authority", () => {
  it("generates engine-specific plan SQL while preserving all source characters", () => {
    const sql = "-- 주석\r\nSELECT '한😀', '\\';\nSELECT 2\r";
    expect(explainWireSql(sql, "run", "unknown")).toBe(sql);
    expect(explainWireSql(sql, "explain_plan", "sqlite")).toBe(`EXPLAIN QUERY PLAN ${sql}`);
    expect(explainWireSql(sql, "explain_analyze", "mysql")).toBe(`EXPLAIN ANALYZE FORMAT=TREE ${sql}`);
    expect(explainWireSql(sql, "explain_analyze", "postgresql")).toBe(`EXPLAIN ANALYZE ${sql}`);
    for (const engine of ["mariadb", "sqlite", "unknown"]) expect(() => explainWireSql(sql, "explain_analyze", engine)).toThrow("EXPLAIN_ANALYZE_UNSUPPORTED");
    expect(() => explainWireSql(sql, "explain_plan", "unknown")).toThrow("EXPLAIN_PLAN_UNSUPPORTED");
  });
  it("quotes names as identifiers and refuses unsupported forms", () => {
    expect(quoteCompletionIdentifier("postgresql", '한"글')).toBe('"한""글"');
    expect(quoteCompletionIdentifier("sqlite", '한"글')).toBe('"한""글"');
    for (const engine of ["mysql", "mariadb"]) {
      expect(quoteCompletionIdentifier(engine, "한`글; DROP TABLE ledger")).toBe("`한``글; DROP TABLE ledger`");
      expect(quoteCompletionIdentifier(engine, "path\\name")).toBeNull();
    }
    for (const name of ["", "x\0y", "한".repeat(400)]) expect(quoteCompletionIdentifier("postgresql", name)).toBeNull();
    expect(quoteCompletionIdentifier("unknown", "ledger")).toBeNull();
  });
  it("caps short nonempty column pagination at six calls plus one namespace page", async () => {
    let pageSequence = 0;
    const f = fixture((command, payload) => command === "catalog.describe" ? page([column("match_" + payload.page.cursor)], "next-" + (++pageSequence)) : page([]));
    const hook = renderHook(() => useSqlCompletions({ ...f, engine: "postgresql", enabled: true }));
    const options = await hook.result.current(request());
    expect(f.requests.filter(r => r.command === "catalog.describe")).toHaveLength(6);
    expect(f.requests.filter(r => r.command === "catalog.page")).toHaveLength(1);
    expect(options.some(o => o.label.startsWith("match_"))).toBe(true);
    expect(f.requests.every(r => r.command.startsWith("catalog."))).toBe(true);
  });
  it("inspects at most512 columns and bounds output to100 names/64KiB", async () => {
    let offset = 0;
    const f = fixture((command, payload) => {
      if (command !== "catalog.describe") return page([]);
      const items = Array.from({ length: payload.page.limit }, (_, n) => column((offset + n === 511 ? "needle_" : "other_") + (offset + n), offset + n));
      offset += items.length; return page(items, "cursor-" + offset);
    });
    const hook = renderHook(() => useSqlCompletions({ ...f, engine: "sqlite", enabled: true }));
    const options = await hook.result.current({ ...request(), prefix: "needle" });
    expect(offset).toBe(512); expect(options.map(o => o.label)).toEqual(["needle_511"]);
    expect(f.requests.filter(r => r.command === "catalog.describe").map(r => r.payload.page.limit)).toEqual([100, 100, 100, 100, 100, 12]);
    const names = fixture(command => command === "catalog.describe" ? page(Array.from({ length: 100 }, (_, n) => column("한".repeat(200) + n, n))) : page([]));
    const bounded = renderHook(() => useSqlCompletions({ ...names, engine: "sqlite", enabled: true }));
    const result = await bounded.result.current(request());
    expect(result.length).toBeGreaterThan(0); expect(result.length).toBeLessThan(100);
    expect(new TextEncoder().encode(JSON.stringify(result)).byteLength).toBeLessThanOrEqual(64 * 1024);
  });
  it("does not stack an unabortable request or publish a response after context changes", async () => {
    let resolve!: (value: unknown) => void;
    const f = fixture(() => new Promise(r => { resolve = r; }));
    const hook = renderHook(({ generation }) => useSqlCompletions({ ...f, session: { ...f.session, connectionGeneration: nativeU64(generation) }, engine: "sqlite", enabled: true }), { initialProps: { generation: "1" } });
    const pending = hook.result.current(request());
    await new Promise(r => setTimeout(r, 150));
    expect(f.requests).toHaveLength(1);
    hook.rerender({ generation: "2" });
    expect(await hook.result.current(request())).toEqual([]);
    expect(f.requests).toHaveLength(1);
    await act(async () => resolve(page([column("stale_name")])));
    expect(await pending).toEqual([]);
  });
  it.each([["unknown", true], ["sqlite", false]] as const)("issues no metadata for engine=%s enabled=%s", async (engine, enabled) => {
    const f = fixture(() => page([]));
    const hook = renderHook(() => useSqlCompletions({ ...f, engine, enabled }));
    expect(await hook.result.current(request())).toEqual([]); expect(f.requests).toHaveLength(0);
  });
});
