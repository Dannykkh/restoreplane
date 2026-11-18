import { Channel, invoke, isTauri } from "@tauri-apps/api/core";
import { nativeU64, type NativeCounterU64, type OperationSnapshot, type ResultEnvelope, type ResultNotice, type ResultTerminalStatus, type StreamIdentity, type TypedCell, type WorkbenchCommands, type WorkbenchOperation } from "./workbenchTypes";

export interface WorkbenchErrorDetails { reason?: string; version?: NativeCounterU64; expectedVersion?: NativeCounterU64; actualVersion?: NativeCounterU64; conflictingOperationIds?: string[] }
export class WorkbenchError extends Error {
  constructor(public readonly code: string, public readonly retryable = false, public readonly operationId: string | null = null, public readonly conflictId: string | null = null, public readonly details: WorkbenchErrorDetails = {}, public readonly field: string | null = null) { super(code); }
}
type Reply<T> = { ok: true; requestId: string; data: T } | { ok: false; requestId: string; error: { code: string; retryable: boolean; operationId?: unknown; field?: unknown; details?: Record<string, unknown> } };
const ERROR_FIELDS = new Set(["scope", "session", "object", "rowHandle", "columnId", "changes", "value", "sql", "reviewId", "draftId", "limits", "transactionId", "sourceHandle", "destinationHandle", "page", "mapping", "queryId", "expectedVersion"]);
function errorId(value: unknown): string | null { return typeof value === "string" && /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(value) ? value : null; }
function errorDetails(raw: Record<string, unknown> | undefined): WorkbenchErrorDetails {
  const details: WorkbenchErrorDetails = {};
  if (!raw) return details;
  if (typeof raw.reason === "string" && Object.prototype.hasOwnProperty.call(READONLY_REASON, raw.reason)) details.reason = raw.reason;
  for (const name of ["version", "expectedVersion", "actualVersion"] as const) { try { if (raw[name] !== undefined) details[name] = nativeU64(raw[name]); } catch { /* Invalid diagnostics never replace the primary typed error. */ } }
  if (Array.isArray(raw.conflictingOperationIds) && raw.conflictingOperationIds.length <= 8) details.conflictingOperationIds = raw.conflictingOperationIds.map(errorId).filter((id): id is string => id !== null);
  return details;
}
export interface WorkbenchTransport {
  readonly available: boolean;
  invoke(request: string, onResult?: (notice: ResultNotice) => void): Promise<unknown>;
}
const nativeTransport: WorkbenchTransport = {
  get available() { return isTauri(); },
  invoke(request, receive) {
    const onResult = receive ? new Channel<ResultNotice>() : undefined;
    if (onResult && receive) onResult.onmessage = receive;
    return invoke("workbench_request", { request, ...(onResult ? { onResult } : {}) });
  },
};
export class WorkbenchClient {
  constructor(private readonly transport: WorkbenchTransport = nativeTransport) {}
  get available() { return this.transport.available; }
  async request<K extends keyof WorkbenchCommands>(command: K, payload: WorkbenchCommands[K][0], onResult?: (notice: ResultNotice) => void, attemptToken?: string): Promise<WorkbenchCommands[K][1]> {
    if (!this.available) throw new WorkbenchError("WB_NATIVE_UNAVAILABLE");
    const requestId = crypto.randomUUID();
    let reply: Reply<WorkbenchCommands[K][1]>;
    try {
      reply = await this.transport.invoke(JSON.stringify({ protocolVersion: 1, requestId, command, payload, ...(attemptToken ? { attemptToken } : {}) }), onResult) as typeof reply;
    } catch { throw new WorkbenchError("WB_TRANSPORT_UNAVAILABLE", true); }
    if (!reply || reply.requestId !== requestId || typeof reply.ok !== "boolean") throw new WorkbenchError("WB_PROTOCOL_UNSUPPORTED");
    if (!reply.ok) {
      const code = /^WB_[A-Z_]+$/.test(reply.error?.code) ? reply.error.code : "WB_IO";
      throw new WorkbenchError(code, code !== "WB_OUTCOME_UNKNOWN" && reply.error?.retryable === true, errorId(reply.error?.operationId), errorId(reply.error?.details?.conflictId), errorDetails(reply.error?.details), typeof reply.error?.field === "string" && ERROR_FIELDS.has(reply.error.field) ? reply.error.field : null);
    }
    validateNativeCounters(reply.data);
    return reply.data;
  }
}
export const workbenchClient = new WorkbenchClient();
const counterFields = new Set(["connectionGeneration", "executionEpoch", "streamGeneration", "sequence", "availableThroughSequence", "highestContiguousSequence", "acceptedThrough", "releasedBytes", "availableBytes", "asOfMs", "observedAtMs", "expiresAtMs", "idleExpiresAtMs", "startRow", "rowsSeen", "operationVersion", "byteLength", "nextOffset", "version", "acknowledgedAtMs", "createdAtMs", "updatedAtMs", "rowsMatched", "rowsChanged", "sourceByteStart", "sourceByteEnd", "sourceRowStart", "sourceRowEnd", "rowsCommitted", "bytesProcessed", "totalBytes", "rowsProcessed", "confirmedCommittedRows", "confirmedCommittedBatches", "unresolvedBatches", "bytesWritten", "rowsWritten", "byteStart", "rowStart"]);
function validateNativeCounters(value: unknown, depth = 0, budget = { nodes: 0 }): void {
  if (++budget.nodes > 100_000 || depth > 24) throw new WorkbenchError("WB_LIMIT");
  if (!value || typeof value !== "object") return;
  for (const [key, item] of Object.entries(value)) {
    if ((counterFields.has(key) || (key === "value" && "kind" in value && ["exact", "estimate", "u64"].includes(String(value.kind)))) && item !== null) nativeU64(item);
    validateNativeCounters(item, depth + 1, budget);
  }
}
export function workbenchErrorText(error: unknown): string {
  const code = error instanceof WorkbenchError ? error.code : "WB_IO";
  const messages: Record<string, string> = {
    WB_NATIVE_UNAVAILABLE: "데스크톱 앱에서 사용할 수 있습니다. 브라우저에서는 실제 데이터베이스에 연결하지 않습니다.",
    WB_TRANSPORT_UNAVAILABLE: "데스크톱 연결 응답을 확인하지 못했습니다. 실행을 자동으로 다시 보내지 않습니다.",
    WB_CAPABILITY_UNSUPPORTED: "이 연결에서 지원하지 않는 기능입니다.", WB_ELEVATED_UNSUPPORTED: "관리자 권한으로 실행한 앱에서는 데이터베이스 작업을 지원하지 않습니다.",
    WB_CURSOR_INVALID: "조회 조건 또는 메타데이터가 바뀌었습니다. 명시적으로 다시 조회해 주세요.",
    WB_RESULT_EXPIRED: "보유 중인 결과가 만료됐습니다. 다시 조회하면 새로운 결과가 됩니다.",
    WB_BINDING_STALE: "연결 대상 설정이 바뀌었습니다. 실제 대상을 확인한 뒤 다시 연결해 주세요.",
    WB_BINDING_UNPROVEN: "실제 연결 대상의 정체성을 확인하지 못했습니다.",
    WB_TARGET_BUSY: "이전 실행이나 연결 정리가 진행 중입니다. 현재 작업 상태를 확인한 뒤 다시 검토해 주세요.",
    WB_LIMIT: "안전한 조회 상한에 도달했습니다. 범위를 줄여 주세요.",
    WB_SESSION_STALE: "연결 세대가 만료됐습니다. 이전 결과를 새 연결에 적용하지 않습니다.",
    WB_OUTCOME_UNKNOWN: "결과를 확인할 수 없습니다. 성공이나 롤백으로 간주하지 않습니다.",
    WB_ACK_INVALID: "결과 전달 순서가 유효하지 않습니다. 결과를 다시 실행하지 않고 전달을 중단했습니다.",
    WB_ROW_CONFLICT: "검토 기준과 대상의 행이 일치하지 않습니다. 초안을 보존했습니다. 롤백 결과는 작업 상태에서 확인하세요.",
    WB_STATEMENT_REJECTED: "데이터베이스가 문장을 거부했습니다. 작업과 트랜잭션 상태를 확인하세요.",
  };
  const text = messages[code] ?? `데이터베이스 작업을 완료하지 못했습니다. 진단 코드: ${code}`;
  const reason = error instanceof WorkbenchError ? error.details.reason : undefined;
  return reason && Object.prototype.hasOwnProperty.call(READONLY_REASON, reason) ? `${text} ${READONLY_REASON[reason]}` : text;
}
export function cellText(cell: TypedCell): string {
  switch (cell.kind) {
    case "null": return "NULL";
    case "bool": return cell.value ? "true" : "false";
    case "text": return cell.value === "" ? "빈 문자열" : cell.value;
    case "bytes": return `base64:${cell.base64}`;
    case "unsupported": return `지원하지 않는 값 (${cell.typeName})`;
    case "large": return cell.preview;
    default: return cell.value;
  }
}
const READONLY_REASON: Record<string, string> = {
  invalid_utf8: "값을 UTF-8 원문으로 해석할 수 없습니다.", invalid_integer: "정수의 원문 형식을 확인하지 못했습니다.", invalid_temporal_fields: "날짜·시간 필드의 정확한 형식을 확인하지 못했습니다.", inline_cell_limit: "값이 인라인 셀의 바이트 상한을 넘었습니다.", inline_cell_limit_or_codec: "값이 인라인 상한을 넘거나 정확한 변환을 지원하지 않습니다.", codec_not_certified: "이 값 형식의 정확한 변환이 검증되지 않았습니다.", codec_or_inline_limit: "값의 정확한 변환 또는 인라인 상한을 보장하지 못했습니다.",
  default_effects_unproven: "서버 기본값의 추가 효과를 인증하지 못했습니다.", transactional_effects_unproven: "이 변경의 효과가 트랜잭션 경계 안에 머무는지 인증하지 못했습니다.", not_observed: "이 변경의 대상 행을 관측하지 못했습니다.",
  verified_editing_not_certified: "이 연결에서는 편집의 값 형식·키·트랜잭션이 검증되지 않았습니다.", codec_key_transaction_certification_required: "값 형식·고유 키·트랜잭션의 편집 인증이 필요합니다.", origin_and_key_not_certified: "결과의 원본 테이블과 고유 키가 검증되지 않았습니다.", live_certification_required: "실제 연결의 기능 검증이 필요합니다.", isolation_certification_required: "격리 수준 검증이 필요합니다.", keyless: "안정된 고유 키가 없어 편집할 수 없습니다.", unsupported_type: "원문을 손실 없이 보존할 수 없는 형식입니다.", generated_column: "서버에서 생성하는 열은 편집할 수 없습니다.", manual_transaction_active: "수동 트랜잭션을 먼저 명시적으로 정리해야 합니다.", read_only_mode: "읽기 전용 모드에서는 변경할 수 없습니다.", nullable_key: "NULL을 허용하는 키의 행 정체성을 보장할 수 없습니다.", partial_key: "조건부 고유 키로 전체 행을 식별할 수 없습니다.", expression_key: "표현식 키의 원본 비교를 인증하지 않았습니다.", view: "뷰는 검증한 기본 테이블 편집 대상이 아닙니다.", join_origin_unproven: "조인 결과의 단일 원본을 증명하지 못했습니다.", multiple_matches: "하나보다 많은 행이 일치해 변경을 거부했습니다.", schema_changed: "검토 이후 구조가 바뀌었습니다.", source_changed: "원본 대상의 정체성이 바뀌었습니다.", non_transactional_storage: "이 저장 엔진의 트랜잭션 효과를 보장할 수 없습니다.", row_missing: "관측 시점에 원본 행이 없었습니다.", row_changed: "원본 행의 값이 바뀌었습니다.", trigger_effects_unproven: "트리거의 추가 효과를 인증하지 못했습니다.", comparison_unproven: "원본 값의 정확한 비교를 보장할 수 없습니다.",
};
export function readonlyReasonText(reason: string | null | undefined): string {
  return reason && Object.prototype.hasOwnProperty.call(READONLY_REASON, reason) ? READONLY_REASON[reason] : "이 값의 편집 근거가 확인되지 않아 읽기 전용으로 표시합니다.";
}
export function nativeFreshness(asOfMs: NativeCounterU64): string {
  const elapsed = BigInt(Date.now()) - BigInt(nativeU64(asOfMs));
  if (elapsed < 0n) return "조회 시각 확인 필요";
  if (elapsed < 60_000n) return `${elapsed / 1000n}초 전 관찰`;
  if (elapsed < 3_600_000n) return `${elapsed / 60_000n}분 전 관찰`;
  if (elapsed < 86_400_000n) return `${elapsed / 3_600_000n}시간 전 관찰`;
  return `${elapsed / 86_400_000n}일 전 관찰`;
}
export const UI_CACHE_MAX_BYTES = 32 * 1024 * 1024;
export const UI_CACHE_TARGET_BYTES = 8 * 1024 * 1024;
const sizingEncoder = new TextEncoder();
let sizingScratch: Uint8Array | undefined;
/** Count the same JSON UTF-8 bytes without allocating an output array per value. */
function jsonByteLength(value: unknown): number {
  const json = JSON.stringify(value) ?? "";
  if (!json.length) return 0;
  const scratch = sizingScratch ??= new Uint8Array(64 * 1024);
  let offset = 0, bytes = 0;
  while (offset < json.length) {
    const encoded = sizingEncoder.encodeInto(json.slice(offset), scratch);
    offset += encoded.read; bytes += encoded.written;
  }
  return bytes;
}
/** Shared across metadata and result panes. Eviction never initiates SQL. */
export class WorkbenchPageCache {
  private entries = new Map<string, { value: unknown; bytes: number }>();
  retainedBytes = 0;
  constructor(readonly budget = UI_CACHE_TARGET_BYTES) {
    if (budget < 1 || budget > UI_CACHE_MAX_BYTES) throw new WorkbenchError("WB_LIMIT");
  }
  put(key: string, value: unknown): void {
    const bytes = jsonByteLength(value) * 2 + 256;
    if (bytes > this.budget) throw new WorkbenchError("WB_LIMIT");
    this.delete(key);
    while (this.retainedBytes + bytes > this.budget || this.entries.size >= 128) this.delete(this.entries.keys().next().value!);
    this.entries.set(key, { value, bytes }); this.retainedBytes += bytes;
  }
  get<T>(key: string): T | undefined {
    const entry = this.entries.get(key);
    if (!entry) return undefined;
    this.entries.delete(key); this.entries.set(key, entry); return entry.value as T;
  }
  delete(key: string): void { const entry = this.entries.get(key); if (entry) this.retainedBytes -= entry.bytes; this.entries.delete(key); }
  clear(): void { this.entries.clear(); this.retainedBytes = 0; }
  get size() { return this.entries.size; }
}
export const workbenchPages = new WorkbenchPageCache();
/** A visible/tail window has a byte bound as well as a row bound. */
export function boundedRowTail<T>(rows: T[], maximumRows = 200, maximumBytes = 2 * 1024 * 1024): T[] {
  let bytes = 0, start = rows.length;
  while (start > 0 && rows.length - start < maximumRows) {
    const size = jsonByteLength(rows[start - 1]) * 2 + 128;
    if (bytes + size > maximumBytes) break;
    bytes += size; start--;
  }
  if (rows.length > 0 && start === rows.length) throw new WorkbenchError("WB_LIMIT");
  return rows.slice(start);
}
let departureGuard: ((proceed: () => void) => boolean) | null = null;
export function setWorkbenchDepartureGuard(guard: typeof departureGuard): void { departureGuard = guard; }
export function requestWorkbenchDeparture(proceed: () => void): boolean { return departureGuard?.(proceed) ?? true; }
/** Full authoritative snapshots can reveal Unknown without a durable version increment. */
export function applyOperationSnapshot(current: WorkbenchOperation | null, snapshot: OperationSnapshot): WorkbenchOperation | null {
  if (snapshot.unchanged) return current;
  const next = snapshot.operation;
  if (current && next.operationId !== current.operationId) throw new WorkbenchError("WB_PAYLOAD_INVALID");
  return current && BigInt(nativeU64(next.version)) < BigInt(nativeU64(current.version)) ? current : next;
}
export function operationHintNeedsPull(current: NativeCounterU64, hint: NativeCounterU64): boolean { return BigInt(nativeU64(hint)) > BigInt(nativeU64(current)); }
export function streamKey(identity: StreamIdentity): string {
  return JSON.stringify([identity.windowId, identity.workspaceId, identity.databaseTargetId, identity.sessionId, identity.connectionGeneration, identity.queryRunId, identity.resultSetId, identity.streamGeneration]);
}
/** Bounded dedup/replay receiver. ACK means retained/applied, not painted or committed. */
export class WorkbenchStreamReceiver {
  private pending = new Map<bigint, ResultEnvelope>();
  private pendingBytes = 0;
  private applied = 0n;
  private acked = 0n;
  private stopped = false;
  private replaying = false;
  private ackScheduled = false;
  private terminalSequence: bigint | null = null;
  private gapTimer: ReturnType<typeof setTimeout> | null = null;
  private replayAfter: bigint | null = null;
  private pulling = false;
  private pullRequested = false;
  private availableThrough = 0n;
  private apply: ((frame: ResultEnvelope) => void) | null;
  private failed: ((error: unknown) => void) | null;
  private statusOnly: ((status: ResultTerminalStatus) => void) | null;
  constructor(private readonly client: WorkbenchClient, readonly identity: StreamIdentity, apply: (frame: ResultEnvelope) => void, failed: (error: unknown) => void, statusOnly?: (status: ResultTerminalStatus) => void) { this.apply = apply; this.failed = failed; this.statusOnly = statusOnly ?? null; }
  get afterSequence(): NativeCounterU64 { return nativeU64(this.applied.toString()); }
  async attach(): Promise<void> {
    if (this.stopped || this.replaying) return;
    this.replaying = true;
    try {
      const reply = await this.client.request("result.attach", { identity: this.identity, afterSequence: this.applied ? this.afterSequence : null }, (notice) => this.notice(notice));
      if (this.stopped) return;
      if (!reply.attached) {
        if (reply.terminal && reply.terminal.protocolVersion === 1 && streamKey(reply.terminal.identity) === streamKey(this.identity) && reply.terminal.event.kind === "terminal" && this.statusOnly) {
          const statusOnly = this.statusOnly; this.dispose(); statusOnly(reply.terminal.event); return;
        }
        throw new WorkbenchError("WB_RESULT_EXPIRED");
      }
      if (reply.terminal) this.availableThrough = BigInt(nativeU64(reply.terminal.sequence));
      this.requestPull();
    } catch (error) { this.stop(error); }
    finally { this.replaying = false; }
  }
  notice(notice: ResultNotice): void {
    if (this.stopped) return;
    try {
      if (notice.protocolVersion !== 1 || notice.kind !== "frames_available" || streamKey(notice.identity) !== streamKey(this.identity)) throw new WorkbenchError("WB_SESSION_STALE");
      if (new TextEncoder().encode(JSON.stringify(notice)).byteLength >= 1024) throw new WorkbenchError("WB_LIMIT");
      const through = BigInt(nativeU64(notice.availableThroughSequence));
      if (through <= this.applied) return;
      if (through > this.availableThrough) this.availableThrough = through;
      this.requestPull();
    } catch (error) { this.stop(error); }
  }
  private requestPull(): void { this.pullRequested = true; if (!this.pulling) void this.pull(); }
  private async pull(): Promise<void> {
    if (this.stopped || this.pulling) return;
    this.pulling = true;
    try {
      let turns = 0;
      while (this.pullRequested && !this.stopped) {
        this.pullRequested = false;
        const before = this.applied;
        const reply = await this.client.request("result.frames", { identity: this.identity, afterSequence: this.applied ? this.afterSequence : null });
        if (this.stopped) return;
        if (!Array.isArray(reply.frames) || reply.frames.length > 1 || typeof reply.hasMore !== "boolean" || jsonByteLength(reply) > 256 * 1024) throw new WorkbenchError("WB_LIMIT");
        for (const frame of reply.frames) this.receive(frame);
        if ((reply.hasMore || this.availableThrough > this.applied) && this.applied === before) throw new WorkbenchError("WB_RESULT_EXPIRED");
        this.pullRequested ||= reply.hasMore || this.availableThrough > this.applied;
        // Yield between bounded pulls so paint and reserved control actions remain responsive.
        if (++turns % 4 === 0 && this.pullRequested) await new Promise<void>((resolve) => setTimeout(resolve, 0));
      }
    } catch (error) { this.stop(error); }
    finally { this.pulling = false; }
  }
  receive(frame: ResultEnvelope): void {
    if (this.stopped) return;
    try {
      if (frame.protocolVersion !== 1 || streamKey(frame.identity) !== streamKey(this.identity)) throw new WorkbenchError("WB_SESSION_STALE");
      validateNativeCounters(frame);
      if (!Number.isSafeInteger(frame.encodedBytes) || frame.encodedBytes < 1 || frame.encodedBytes > 256 * 1024 || jsonByteLength(frame) > 256 * 1024) throw new WorkbenchError("WB_LIMIT");
      const sequence = BigInt(nativeU64(frame.sequence));
      if (sequence <= this.applied || this.pending.has(sequence)) return;
      if (this.terminalSequence !== null && sequence > this.terminalSequence) throw new WorkbenchError("WB_ACK_INVALID");
      if (frame.event.kind === "terminal") this.terminalSequence = sequence;
      if (this.pending.size >= 8 || this.pendingBytes + frame.encodedBytes > 2 * 1024 * 1024) throw new WorkbenchError("WB_RESULT_EXPIRED");
      this.pending.set(sequence, frame); this.pendingBytes += frame.encodedBytes;
      while (this.pending.has(this.applied + 1n)) {
        const next = this.pending.get(this.applied + 1n)!;
        const apply = this.apply;
        if (!apply) throw new WorkbenchError("WB_RESULT_EXPIRED");
        apply(next); if (this.stopped) return;
        this.pending.delete(this.applied + 1n); this.pendingBytes -= next.encodedBytes; this.applied += 1n;
      }
      this.scheduleAck();
      if (!this.pending.size && this.availableThrough <= this.applied) this.pullRequested = false;
      if (!this.pending.size && this.gapTimer) { clearTimeout(this.gapTimer); this.gapTimer = null; }
      if (this.pending.size && !this.gapTimer) this.gapTimer = setTimeout(() => this.stop(new WorkbenchError("WB_RESULT_EXPIRED")), 5000);
      if (this.pending.size && this.replayAfter !== this.applied) { this.replayAfter = this.applied; this.requestPull(); }
    } catch (error) { this.stop(error); }
  }
  private scheduleAck(): void {
    if (this.ackScheduled || this.applied <= this.acked) return;
    this.ackScheduled = true;
    queueMicrotask(() => {
      this.ackScheduled = false;
      if (this.stopped || this.applied <= this.acked) return;
      const through = this.applied; this.acked = through;
      void this.client.request("result.ack", { identity: this.identity, highestContiguousSequence: nativeU64(through.toString()) }).catch((error) => this.stop(error));
    });
  }
  dispose(): void { this.stopped = true; this.pending.clear(); this.pendingBytes = 0; this.apply = null; this.failed = null; this.statusOnly = null; if (this.gapTimer) clearTimeout(this.gapTimer); this.gapTimer = null; }
  private stop(error: unknown): void { if (this.stopped) return; const failed = this.failed; this.dispose(); failed?.(error); }
}
