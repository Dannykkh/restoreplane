import { lazy, Suspense, useRef, useState } from "react";
import type { Mode, QueryPurpose, QueryReview } from "../../lib/workbenchTypes";
import { Dialog } from "../../components/Dialog";
const SqlCodeEditor = lazy(() => import("./SqlCodeEditor"));
import type { SqlCompletionProvider } from "./useSqlCompletions";

export const MAX_SQL_BYTES = 1024 * 1024;
export function sqlBytes(sql: string): number { return new TextEncoder().encode(sql).byteLength; }
export function explainWireSql(sql: string, purpose: QueryPurpose, engine: string): string {
  if (purpose === "run") return sql;
  if (purpose === "explain_analyze") {
    if (engine === "postgresql") return `EXPLAIN ANALYZE ${sql}`;
    if (engine === "mysql") return `EXPLAIN ANALYZE FORMAT=TREE ${sql}`;
    throw new Error("EXPLAIN_ANALYZE_UNSUPPORTED");
  }
  if (engine === "sqlite") return `EXPLAIN QUERY PLAN ${sql}`;
  if (engine === "postgresql" || engine === "mysql" || engine === "mariadb") return `EXPLAIN ${sql}`;
  throw new Error("EXPLAIN_PLAN_UNSUPPORTED");
}
type ExplainDraft = { purpose: Exclude<QueryPurpose, "run">; sourceSql: string; wireSql: string; engine: string };
export interface ReviewedQuery { sourceSql: string; wireSql: string; purpose: QueryPurpose; review: QueryReview; binding: string }
const WARNING_LABEL: Record<string, string> = {
  import_independent_batches: "각 가져오기 배치는 별도의 트랜잭션으로 요청합니다.", import_partial_commits: "뒤 배치가 실패하거나 중지되어도 앞서 확인한 커밋은 유지됩니다.", import_continuation_unsent_only: "새로 확인한 파일에서 아직 한 번도 보내지 않은 다음 구간만 계속합니다.",
  standalone_transaction: "이 변경은 독립된 트랜잭션으로 요청합니다.", optimistic_basis_not_history: "원본 비교는 현재 변경 충돌을 확인하며 과거 이력을 증명하지 않습니다.", transactional_effects_only: "검증한 트랜잭션 효과만 확인하며 외부 효과의 취소를 보장하지 않습니다.",
  statement_scoped_readonly_control: "각 문장을 별도의 읽기 전용 트랜잭션에서 실행합니다.",
  explain_analyze_executes: "실제 SQL을 실행합니다.", plan_is_not_effect_proof: "실행 계획은 효과 없음의 증거가 아닙니다.", function_effects_unproven: "함수의 효과를 확인하지 못했습니다.", locking_read: "잠금을 요청하는 조회입니다.", data_change: "데이터를 변경합니다.", ddl_change: "데이터베이스 구조를 변경합니다.", unknown_effects: "실행 효과가 불명확합니다.", implicit_commit_possible: "묵시적으로 커밋될 수 있습니다.", multiple_statements_partial_effects: "일부 문장만 반영될 수 있습니다.", server_readonly_authority_required: "서버의 읽기 전용 보호가 필요합니다.", unverified_boundary_single_unit: "문장 경계를 확인할 수 없어 전체를 한 단위로 실행합니다.", session_retired_after_execution: "실행 후 연결을 폐기합니다.",
};
export function warningText(code: string): string { return Object.prototype.hasOwnProperty.call(WARNING_LABEL, code) ? WARNING_LABEL[code] : "추가 주의사항을 확인하세요."; }
export interface QueryEditorProps {
  sql: string; engine: string; mode: Mode; reviewed: ReviewedQuery | null; confirmedWarnings: string[];
  busy: boolean; canRun: boolean; staleBinding: boolean;
  onChange: (sql: string) => void; onReview: (purpose: QueryPurpose, wireSql: string) => void;
  onWarnings: (warnings: string[]) => void; onRun: () => void; onSave: () => void;
  suspended?: boolean;
  canChange?: (sql: string) => boolean;
  completionProvider?: SqlCompletionProvider;
}
export function QueryEditor({ sql, engine, mode, reviewed, confirmedWarnings, busy, canRun, staleBinding, onChange, onReview, onWarnings, onRun, onSave, suspended = false, canChange, completionProvider }: QueryEditorProps) {
  const composing = useRef(false);
  const [explainState, setExplain] = useState<ExplainDraft | null>(null), [inputError, setInputError] = useState("");
  const explain = suspended ? null : explainState;
  const planSupported = ["postgresql", "mysql", "mariadb", "sqlite"].includes(engine);
  const analyzeSupported = engine === "postgresql" || engine === "mysql";
  const explainCurrent = !!explain && explain.sourceSql === sql && explain.engine === engine && (explain.purpose !== "explain_analyze" || mode === "full");
  const bytes = sqlBytes(sql), expired = reviewed !== null && BigInt(reviewed.review.expiresAtMs) <= BigInt(Date.now());
  const modeRank: Record<Mode, number> = { read_only: 0, read_write: 1, full: 2 };
  const requiresMode = reviewed && modeRank[reviewed.review.requiredMode] > modeRank[mode];
  const ready = !!reviewed && !expired && !requiresMode && reviewed.sourceSql === sql && reviewed.review.warnings.every((warning) => confirmedWarnings.includes(warning)) && !staleBinding;
  function review(purpose: QueryPurpose): void {
    if (composing.current || busy || suspended || staleBinding || !sql.trim() || bytes > MAX_SQL_BYTES) return;
    if (purpose === "run") { onReview(purpose, sql); return; }
    if (!planSupported || purpose === "explain_analyze" && (!analyzeSupported || mode !== "full")) return;
    try {
      const wireSql = explainWireSql(sql, purpose, engine);
      if (sqlBytes(wireSql) > MAX_SQL_BYTES) { setInputError("생성한 검토 SQL이 UTF-8 1MiB 상한을 넘었습니다. 원문을 줄인 뒤 다시 검토해 주세요."); return; }
      setExplain({ purpose, sourceSql: sql, wireSql, engine }); setInputError("");
    }
    catch { setInputError("이 연결에서는 선택한 실행 계획 도우미를 지원하지 않습니다. 원문은 유지됩니다."); }
  }
  function runReviewed(): void { if (!composing.current && !suspended && !busy && ready && canRun) onRun(); }
  return <section className="wb-query-editor" aria-label="SQL 원문 편집기" onCompositionStartCapture={() => { composing.current = true; }} onCompositionEndCapture={() => { composing.current = false; }}>
    <div className="wb-query-actions"><button className="button button-secondary" type="button" disabled={busy || suspended || !sql.trim() || staleBinding} onClick={() => review("run")}>원문 검토</button><button className="button button-secondary" type="button" disabled={busy || suspended || !planSupported || !sql.trim() || staleBinding} onClick={() => review("explain_plan")}>실행 계획 검토</button><button className="button button-secondary" type="button" disabled={busy || suspended || !analyzeSupported || !sql.trim() || mode !== "full" || staleBinding} onClick={() => review("explain_analyze")}>실행 분석 검토</button><button className="button button-primary" type="button" disabled={busy || suspended || !ready || !canRun} onClick={runReviewed}>검토한 SQL 실행</button><button className="button button-secondary" type="button" disabled={busy || suspended || !sql.trim()} onClick={onSave}>이름을 정해 저장</button></div>
    <Suspense fallback={<div className="wb-sql-loading" role="status">SQL 편집기를 불러오는 중입니다.</div>}><SqlCodeEditor sql={sql} engine={engine} describedBy="wb-sql-help" completionProvider={completionProvider} canChange={canChange}
      onChange={(next) => { if (sqlBytes(next) > MAX_SQL_BYTES) { setInputError("SQL 원문은 UTF-8 1MiB 이하로 입력해 주세요. 기존 초안은 유지됩니다."); return; } setInputError(""); onChange(next); }}
      onExecuteShortcut={() => { if (ready && canRun) runReviewed(); else review("run"); }} /></Suspense>
    <details className="wb-editor-help"><summary><span>편집 도움말 · Ctrl+Space{!analyzeSupported && (engine === "mariadb" || engine === "sqlite") ? ` · ${engine === "mariadb" ? "MariaDB" : "SQLite"} 실행 계획만 지원` : !planSupported ? " · 실행 계획 도우미 미지원" : ""}</span><span className="mono">{bytes} / {MAX_SQL_BYTES} bytes</span></summary><p id="wb-sql-help">원문은 현재 창의 메모리에만 유지되며 자동 실행·자동 저장하지 않습니다. Ctrl+Space는 선택한 스키마의 이름을 제안합니다. 이름 제안은 SQL 검토나 실행 권한 확인을 대신하지 않습니다.</p></details>
    {inputError && <p role="alert" className="wb-error">{inputError}</p>}
    {staleBinding && <p role="status" className="wb-notice">연결 대상 또는 세대가 바뀌었습니다. 이 초안을 현재 연결에 다시 연결한 뒤 검토해 주세요.</p>}
    {!canRun && <p className="wb-muted">현재 연결에서 지금 실행을 요청할 수 없습니다. 작업 상태와 연결 지원을 확인하세요.</p>}
    {reviewed && <section className="wb-sql-review" aria-label="네이티브 SQL 검토 결과"><strong>{expired ? "검토 기한이 지났습니다." : "원문 검토 결과"}</strong><span>{reviewed.review.statementCount}개 문장 · 요구 모드 {({ read_only: "읽기 전용", read_write: "데이터 변경", full: "전체 SQL" })[reviewed.review.requiredMode]}</span>
      <details><summary>검토 식별정보</summary><code>{reviewed.review.digest}</code></details>{requiresMode && <p>요구하는 모드로 명시적으로 전환한 뒤 다시 검토해 주세요.</p>}
      {reviewed.purpose !== "run" && <details open><summary>검토한 실행 SQL · 편집 원문은 유지됨</summary><pre>{reviewed.wireSql}</pre></details>}
      {reviewed.review.warnings.map((warning) => <label key={warning} className="wb-check"><input type="checkbox" checked={confirmedWarnings.includes(warning)} onChange={(event) => onWarnings(event.target.checked ? [...confirmedWarnings, warning] : confirmedWarnings.filter((item) => item !== warning))} />{warningText(warning)}</label>)}
    </section>}
    {explain && <Dialog title={explain.purpose === "explain_analyze" ? "실제로 실행하는 분석 SQL을 검토할까요?" : "실행 계획 SQL을 검토할까요?"} description={explain.purpose === "explain_analyze" ? "실행 분석은 실제 SQL을 실행할 수 있습니다. 아래 전체 SQL을 네이티브 검토에 보냅니다." : "아래 전체 SQL을 네이티브 검토에 보냅니다. 편집 원문에는 접두사를 추가하지 않습니다."} onClose={() => setExplain(null)}><pre className="wb-wire-sql">{explain.wireSql}</pre>{!explainCurrent && <p role="status">초안 또는 연결 조건이 바뀌었습니다. 닫은 뒤 현재 원문을 다시 검토해 주세요.</p>}<div className="dialog-actions"><button className="button button-secondary" type="button" onClick={() => setExplain(null)}>취소</button><button className="button button-primary" type="button" disabled={busy || staleBinding || !explainCurrent} onClick={() => { if (composing.current || busy || suspended || staleBinding || !explainCurrent) return; const captured = explain; setExplain(null); onReview(captured.purpose, captured.wireSql); }}>이 SQL 전체 검토</button></div></Dialog>}
  </section>;
}
