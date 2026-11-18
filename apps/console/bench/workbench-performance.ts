import React, { useLayoutEffect, useState } from "react";
import { createRoot } from "react-dom/client";

/** Isolated native/WebView benchmark; never imported by the shipped Console. */
export const limits = Object.freeze({
  rows: 100_000, columns: 100, seed: 71237, frame: 256 * 1024,
  credit: 8 * 1024 * 1024, frontend: 32 * 1024 * 1024,
  pageCacheTarget: 8 * 1024 * 1024,
  native: 64 * 1024 * 1024, inlineCell: 64 * 1024,
  visibleRows: 20, visibleColumns: 8, rowOverscan: 4, columnOverscan: 2,
  noAckDeadlineMs: 5000, samples: 200, measuredRuns: 5,
});
export interface BenchScenario {
  name: string; seed: number; rows: number; columns: number;
  valueDistribution: string[]; viewport: { width: number; height: number };
  overscan: { rows: number; columns: number }; ackPolicy: string;
  limits: typeof limits;
}
type Cell = { kind: string; value?: string; base64?: string; precision?: number };
interface NativeStatus {
  generation: number; sequence: number; acknowledged: number; outstandingBytes: number;
  peakOutstandingBytes: number; nativeRetainedBytes: number; reservedBytes: number;
  terminal: string; generatedRows: number; rejectedAcks: number; monotonicMicros: number;
}
interface NativeFrame {
  status: NativeStatus; sequence: number; encodedBytes: number;
  payload: string | null; digest: number | null;
}
interface Counter {
  atMs: number; phase: string; status: NativeStatus; retainedBytes: number;
  decodedEstimateBytes: number; mountedCells: number; cachedPages: number;
}
interface InputSample {
  run: number; index: number; action: string; startMs: number; eventMs: number;
  nativeMicros: number; responseMs: number; row: number; column: number;
  trustedEvent: boolean; digestVerified: boolean;
  nativeReadMs: number; decodeMs: number; acknowledgeMs: number; paintMs: number;
  inputResponseMs: number; e2eAutomationLatencyMs: number;
}
interface PendingInput {
  run: number; index: number; action: string; startMs: number;
  resolve: (value: InputSample) => void; reject: (reason: unknown) => void;
}
interface Page { start: number; cells: Cell[][]; bytes: number; decodedEstimate: number }
interface BenchmarkApi {
  manifest: BenchScenario; ready: boolean;
  arm(run: number, index: number, action: string): void;
  settled(): Promise<InputSample>;
  beginRun(): Promise<void>;
  finish(): Promise<Record<string, unknown>>;
  faults(): Promise<Record<string, unknown>[]>;
  snapshot(): { inputs: InputSample[]; frames: number[]; counters: Counter[]; errors: string[] };
}
declare global {
  interface Window {
    __TAURI__: { core: { invoke<T>(command: string, args?: Record<string, unknown>): Promise<T> } };
    workbenchBenchmark: BenchmarkApi;
  }
}
const invoke = <T,>(command: string, args: Record<string, unknown> = {}) =>
  window.__TAURI__.core.invoke<T>(command, args);
const encoder = new TextEncoder();
export function digestBytes(text: string): number {
  let hash = 0x811c9dc5;
  for (const byte of encoder.encode(text)) hash = Math.imul(hash ^ byte, 0x01000193) >>> 0;
  return hash;
}
export function p95(values: number[]): number {
  if (!values.length || values.some((value) => !Number.isFinite(value))) throw new Error("DATA_NONE");
  return [...values].sort((a, b) => a - b)[Math.ceil(values.length * 0.95) - 1];
}
export function validateMetrics(inputs: InputSample[], counters: Counter[]): void {
  for (let run = 1; run <= limits.measuredRuns; run++) {
    if (inputs.filter((sample) => sample.run === run).length !== limits.samples) throw new Error("DATA_NONE");
  }
  if (!counters.length) throw new Error("DATA_NONE");
  for (const item of counters) {
    if (item.status.outstandingBytes > limits.credit || item.status.reservedBytes > limits.credit ||
        item.status.nativeRetainedBytes > limits.native || item.retainedBytes > limits.frontend ||
        item.mountedCells > (limits.visibleRows + 2 * limits.rowOverscan) *
          (limits.visibleColumns + 2 * limits.columnOverscan)) throw new Error("BOUND_VIOLATION");
  }
}
const inputs: InputSample[] = [], frames: number[] = [], counters: Counter[] = [], errors: string[] = [];
const cache = new Map<number, Page>();
let retainedBytes = 0, decodedEstimateBytes = 0, generation = 0;
let currentRow = 0, currentColumn = 0, active = false;
let pending: PendingInput | undefined, settlement: Promise<InputSample> = Promise.reject(new Error("NOT_ARMED"));
void settlement.catch(() => undefined);
let setView: (value: { row: number; column: number; page: Page; ticket: number }) => void;
let nextTicket = 0;
const callbacks = new Map<number, () => void>();
const afterPaint = () => new Promise<void>((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve())));
const pause = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));

function cellText(cell: Cell): string {
  if (cell.kind === "null") return "NULL";
  if (cell.kind === "text" && cell.value === "") return "빈 문자열";
  return cell.value ?? cell.base64 ?? cell.kind;
}
function Surface() {
  const [view, update] = useState<{ row: number; column: number; page: Page; ticket: number }>({
    row: 0, column: 0, page: { start: 0, cells: [], bytes: 0, decodedEstimate: 0 }, ticket: 0,
  });
  setView = update;
  useLayoutEffect(() => {
    const callback = callbacks.get(view.ticket);
    if (callback) { callbacks.delete(view.ticket); void afterPaint().then(callback); }
  }, [view]);
  const rowCount = Math.min(limits.visibleRows + 2 * limits.rowOverscan, view.page.cells.length);
  const columnCount = limits.visibleColumns + 2 * limits.columnOverscan;
  const cells: React.ReactNode[] = [];
  for (let row = 0; row < rowCount; row++) for (let column = 0; column < columnCount; column++) {
    const logicalColumn = Math.min(limits.columns - 1, view.column + column);
    const cell = view.page.cells[row]?.[logicalColumn];
    if (!cell) continue;
    cells.push(React.createElement("div", {
      key: `${view.page.start + row}:${logicalColumn}`, role: "gridcell",
      "aria-rowindex": view.page.start + row + 1, "aria-colindex": logicalColumn + 1,
      "data-bench-cell": "", "data-kind": cell.kind,
      style: { minWidth: 110, maxWidth: 110, overflow: "hidden", whiteSpace: "nowrap", borderBottom: "1px solid #2e3846", padding: "4px 6px" },
    }, cellText(cell)));
  }
  return React.createElement("main", { style: { padding: 16 } },
    React.createElement("h1", { style: { fontSize: 18, color: "#3fbfb0" } }, "데이터 관리 성능 검증"),
    React.createElement("p", {}, "합성 데이터 전용 · 실제 데이터베이스 성능 인증 아님"),
    React.createElement("p", { id: "target-context" }, `논리 행 ${view.row + 1} / ${limits.rows} · 열 ${view.column + 1} / ${limits.columns}`),
    React.createElement("button", { id: "stop", onClick: () => void invoke("bench_cancel", { generation }) }, "읽기 중지"),
    React.createElement("p", { id: "terminal-state", role: "status" }, "합성 결과 준비됨"),
    React.createElement("div", { id: "grid", tabIndex: 0, role: "grid", "aria-rowcount": limits.rows, "aria-colcount": limits.columns,
      style: { height: 640, width: 1080, overflow: "hidden", display: "grid", gridTemplateColumns: `repeat(${columnCount}, 122px)`, marginTop: 12 } }, cells),
  );
}

async function open(mode = "normal"): Promise<NativeStatus> {
  const state = await invoke<NativeStatus>("bench_open", { seed: limits.seed, mode });
  generation = state.generation; return state;
}
async function readPage(startRow: number): Promise<{ page: Page; status: NativeStatus; digestVerified: boolean; nativeReadMs: number; decodeMs: number; acknowledgeMs: number }> {
  const hit = cache.get(startRow);
  if (hit) {
    cache.delete(startRow); cache.set(startRow, hit);
    return { page: hit, status: await invoke<NativeStatus>("bench_status", { generation }), digestVerified: true, nativeReadMs: 0, decodeMs: 0, acknowledgeMs: 0 };
  }
  const readStart = performance.now();
  const frame = await invoke<NativeFrame>("bench_next", { generation, startRow, rowCount: 28, oversize: "none" });
  const receivedAt = performance.now();
  if (frame.payload === null || frame.digest === null) throw new Error(`NO_PAYLOAD:${frame.status.terminal}`);
  if (frame.encodedBytes > limits.frame || digestBytes(frame.payload) !== frame.digest) throw new Error("DIGEST_OR_FRAME");
  const cells = JSON.parse(frame.payload) as Cell[][];
  const page = { start: startRow, cells, bytes: encoder.encode(frame.payload).byteLength,
    decodedEstimate: frame.payload.length * 2 + cells.length * limits.columns * 96 };
  while (retainedBytes + page.bytes + decodedEstimateBytes + page.decodedEstimate > limits.pageCacheTarget && cache.size) {
    const oldest = cache.keys().next().value as number;
    const discarded = cache.get(oldest)!; cache.delete(oldest);
    retainedBytes -= discarded.bytes; decodedEstimateBytes -= discarded.decodedEstimate;
  }
  cache.set(startRow, page); retainedBytes += page.bytes; decodedEstimateBytes += page.decodedEstimate;
  const decodedAt = performance.now();
  const status = await invoke<NativeStatus>("bench_ack", { generation, sequence: frame.sequence });
  return { page, status, digestVerified: true, nativeReadMs: receivedAt-readStart, decodeMs: decodedAt-receivedAt, acknowledgeMs: performance.now()-decodedAt };
}
function recordCounter(status: NativeStatus, phase: string): void {
  counters.push({ atMs: Date.now(), phase, status, retainedBytes: retainedBytes + decodedEstimateBytes,
    decodedEstimateBytes, mountedCells: document.querySelectorAll("[data-bench-cell]").length, cachedPages: cache.size });
}
async function respond(event: KeyboardEvent | WheelEvent): Promise<void> {
  if (!pending || active) return;
  event.preventDefault(); active = true;
  const sample = pending; pending = undefined;
  try {
    currentRow = (sample.index * 499 + sample.run * 997) % (limits.rows - 28);
    currentColumn = (sample.index * 13) % (limits.columns - 12);
    const eventMs = performance.now();
    if (sample.action === "cancel") { await invoke("bench_cancel", { generation }); await open(); }
    const result = await readPage(currentRow);
    const paintStart = performance.now();
    const ticket = ++nextTicket;
    await new Promise<void>((resolve) => {
      callbacks.set(ticket, resolve); setView({ row: currentRow, column: currentColumn, page: result.page, ticket });
    });
    const resultSample = { run: sample.run, index: sample.index, action: sample.action, startMs: sample.startMs,
      eventMs, nativeMicros: result.status.monotonicMicros, responseMs: performance.now() - sample.startMs,
      row: currentRow, column: currentColumn, trustedEvent: event.isTrusted, digestVerified: result.digestVerified,
      nativeReadMs: result.nativeReadMs, decodeMs: result.decodeMs, acknowledgeMs: result.acknowledgeMs, paintMs: performance.now()-paintStart,
      inputResponseMs: performance.now()-eventMs, e2eAutomationLatencyMs: performance.now()-sample.startMs };
    recordCounter(result.status, `input:${sample.run}`); inputs.push(resultSample); sample.resolve(resultSample);
  } catch (error) { errors.push(String(error)); sample.reject(error); }
  finally { active = false; }
}

async function faultScenarios(): Promise<Record<string, unknown>[]> {
  const result: Record<string, unknown>[] = [];
  await open("slow");
  let frame = await invoke<NativeFrame>("bench_next", { generation, startRow: 0, rowCount: 28, oversize: "none" });
  await pause(300);
  let state = await invoke<NativeStatus>("bench_ack", { generation, sequence: frame.sequence });
  recordCounter(state, "slow_ack"); result.push({ name: "slow_ack", passed: state.outstandingBytes === 0, state });
  const oldSequence = frame.sequence;
  frame = await invoke<NativeFrame>("bench_next", { generation, startRow: 28, rowCount: 28, oversize: "none" });
  const beforeDuplicate = frame.status.outstandingBytes;
  state = await invoke<NativeStatus>("bench_ack", { generation, sequence: oldSequence });
  result.push({ name: "duplicate_ack", passed: state.outstandingBytes === beforeDuplicate, state });
  let futureRejected = false;
  try { await invoke("bench_ack", { generation, sequence: frame.sequence + 100 }); }
  catch { futureRejected = true; }
  result.push({ name: "future_ack", passed: futureRejected });
  await open("no_ack");
  const noAckStartedAt = performance.now();
  let generated = 0;
  for (let count = 0; count < 256; count++) {
    frame = await invoke<NativeFrame>("bench_next", { generation, startRow: count * 28, rowCount: 28, oversize: "none" });
    generated += frame.payload === null ? 0 : 28;
    recordCounter(frame.status, "no_ack_fill");
    if (frame.payload === null) break;
  }
  const paused = await invoke<NativeStatus>("bench_status", { generation });
  const expectedDeadline = noAckStartedAt + limits.noAckDeadlineMs;
  while (performance.now() < expectedDeadline && (await invoke<NativeStatus>("bench_status", { generation })).terminal === "streaming") await pause(50);
  state = await invoke<NativeStatus>("bench_status", { generation });
  document.getElementById("terminal-state")!.textContent = `소비자 대기 종료: ${state.terminal}`;
  await afterPaint();
  result.push({ name: "no_ack", passed: paused.outstandingBytes > 0 && generated < limits.rows && state.terminal === "interrupted", paused, state, deadlineObservationMs: performance.now() - expectedDeadline });
  await open("cancel");
  frame = await invoke<NativeFrame>("bench_next", { generation, startRow: 0, rowCount: 28, oversize: "none" });
  const cancelStart = performance.now();
  state = await invoke<NativeStatus>("bench_cancel", { generation });
  document.getElementById("terminal-state")!.textContent = `읽기 중지: ${state.terminal}`;
  await afterPaint();
  const cancelMs = performance.now() - cancelStart;
  const retiredGeneration = generation;
  await open();
  let staleRejected = false;
  try { await invoke("bench_ack", { generation: retiredGeneration, sequence: frame.sequence }); } catch { staleRejected = true; }
  result.push({ name: "cancel_and_stale_ack", passed: state.terminal === "cancelled" && state.outstandingBytes === 0 && staleRejected, cancelMs, state });
  for (const oversize of ["cell", "frame"]) {
    await open();
    frame = await invoke<NativeFrame>("bench_next", { generation, startRow: 0, rowCount: 28, oversize });
    result.push({ name: `oversize_${oversize}`, passed: frame.payload === null && frame.status.terminal === "oversize" && frame.status.outstandingBytes === 0, state: frame.status });
  }
  return result;
}

const manifest: BenchScenario = {
  name: "workbench.baseline", seed: limits.seed, rows: limits.rows, columns: limits.columns,
  valueDistribution: ["null", "empty", "u64_max", "decimal_exact", "korean_text", "bytes_base64", "temporal", "i64_exact"],
  viewport: { width: 1240, height: 840 }, overscan: { rows: limits.rowOverscan, columns: limits.columnOverscan },
  ackPolicy: "contiguous/native-reserve-before-produce", limits,
};
async function initialize(): Promise<void> {
  createRoot(document.getElementById("root")!).render(React.createElement(Surface));
  await afterPaint(); await open();
  const initial = await readPage(0); setView({ row: 0, column: 0, page: initial.page, ticket: 0 });
  await afterPaint(); recordCounter(initial.status, "idle");
  document.addEventListener("keydown", (event) => { if (!event.isComposing) void respond(event); });
  document.addEventListener("wheel", (event) => void respond(event), { passive: false });
  let previousFrame = performance.now();
  const observeFrame = (now: number) => { frames.push(now - previousFrame); previousFrame = now; requestAnimationFrame(observeFrame); };
  requestAnimationFrame(observeFrame);
  window.workbenchBenchmark = {
    manifest, ready: true,
    arm(run, index, action) {
      if (pending || active) throw new Error("INPUT_STILL_PENDING");
      settlement = new Promise<InputSample>((resolve, reject) => { pending = { run, index, action, startMs: performance.now(), resolve, reject }; });
    },
    settled: () => settlement,
    async beginRun() { cache.clear(); retainedBytes = 0; decodedEstimateBytes = 0; await open(); },
    faults: faultScenarios,
    snapshot: () => ({ inputs, frames, counters, errors }),
    async finish() {
      validateMetrics(inputs, counters);
      let missingRejected = false, overflowRejected = false;
      try { validateMetrics([], counters); } catch { missingRejected = true; }
      try { validateMetrics(inputs, [{ ...counters[0], retainedBytes: limits.frontend + 1 }]); } catch { overflowRejected = true; }
      return { inputP95Ms: p95(inputs.filter((sample) => sample.run > 0).map((sample) => sample.inputResponseMs)),
        automationP95Ms: p95(inputs.filter((sample) => sample.run > 0).map((sample) => sample.e2eAutomationLatencyMs)),
        frameP95Ms: p95(frames), negativeChecks: { missingRejected, overflowRejected },
        native: await invoke("bench_info"), manifest };
    },
  };
}
void initialize().catch((error) => { errors.push(String(error)); document.body.textContent = `측정 시작 실패: ${String(error)}`; });
