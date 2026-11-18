import { useId, useLayoutEffect, useRef, useState } from "react";
import { Annotation, Compartment, EditorSelection, EditorState, Prec, Text, Transaction } from "@codemirror/state";
import { drawSelection, EditorView, highlightActiveLineGutter, keymap, lineNumbers } from "@codemirror/view";
import { defaultKeymap, history, historyKeymap, insertNewline } from "@codemirror/commands";
import { bracketMatching, HighlightStyle, syntaxHighlighting, syntaxTree } from "@codemirror/language";
import { acceptCompletion, autocompletion, closeCompletion, completionKeymap, completionStatus, startCompletion, type CompletionSource } from "@codemirror/autocomplete";
import { MariaSQL, MySQL, PostgreSQL, SQLite, type SQLDialect } from "@codemirror/lang-sql";
import { tags } from "@lezer/highlight";
import type { SqlCompletionItem, SqlCompletionProvider } from "./useSqlCompletions";

export interface SqlCodeEditorProps {
  sql: string;
  engine: string;
  onChange(next: string): void;
  onExecuteShortcut(): void;
  describedBy?: string;
  completionProvider?: SqlCompletionProvider;
  /** Pure synchronous aggregate-tab admission. Missing admission makes editing read-only. */
  canChange?(next: string): boolean;
}

const MAX_SQL_BYTES = 1024 * 1024;
const MAX_COMPLETION_BYTES = 64 * 1024;
const COMPLETION_LOOKBEHIND = 256;
const encoder = new TextEncoder();
const externalDocument = Annotation.define<boolean>();
/** Split only LF: CR, CRLF, mixed separators, and Unicode code points are not normalized. */
export function exactSqlText(sql: string): Text { return Text.of(sql.split("\n")); }
function dialect(engine: string): SQLDialect | null {
  switch (engine) { case "postgresql": return PostgreSQL; case "mysql": return MySQL; case "mariadb": return MariaSQL; case "sqlite": return SQLite; default: return null; }
}

const appearance = EditorView.theme({
  "&": { color: "var(--text)", backgroundColor: "var(--bg)", border: "1px solid var(--border-strong)", borderRadius: "var(--r-sm)", fontSize: "12px" },
  "&.cm-focused": { outline: "2px solid var(--primary)", outlineOffset: "2px" },
  ".cm-scroller": { fontFamily: "var(--font-mono)", lineHeight: "1.7", overflow: "auto", minHeight: "120px" },
  ".cm-content": { padding: "12px 0", caretColor: "var(--primary)" },
  ".cm-line": { padding: "0 12px" },
  ".cm-gutters": { backgroundColor: "var(--bg)", color: "var(--text-muted)", borderRight: "1px solid var(--border)", fontFamily: "var(--font-mono)" },
  ".cm-activeLineGutter": { backgroundColor: "var(--surface-2)", color: "var(--text)" },
  ".cm-cursor": { borderLeftColor: "var(--primary)" },
  ".cm-selectionBackground, &.cm-focused .cm-selectionBackground": { backgroundColor: "color-mix(in srgb, var(--primary) 22%, var(--bg))" },
  ".cm-tooltip": { backgroundColor: "var(--surface-raised)", color: "var(--text)", border: "1px solid var(--border-strong)", borderRadius: "var(--r-sm)", boxShadow: "none" },
  ".cm-tooltip-autocomplete > ul": { fontFamily: "var(--font-mono)", fontSize: "12px", maxHeight: "240px" },
  ".cm-tooltip-autocomplete > ul > li[aria-selected]": { backgroundColor: "var(--surface-2)", color: "var(--text)" },
  ".cm-completionDetail": { color: "var(--text-muted)", fontStyle: "normal" },
});
const highlighting = syntaxHighlighting(HighlightStyle.define([
  { tag: tags.keyword, color: "var(--primary)", fontWeight: "500" },
  { tag: tags.string, color: "var(--warning)" },
  { tag: tags.number, color: "var(--info)" },
  { tag: tags.comment, color: "var(--text-muted)" },
  { tag: [tags.typeName, tags.operator], color: "var(--text)" },
]));

function synchronize(view: EditorView, sql: string): void {
  if (view.state.doc.toString() === sql || encoder.encode(sql).length > MAX_SQL_BYTES) return;
  const selection = view.state.selection.main;
  view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: exactSqlText(sql) },
    selection: EditorSelection.single(Math.min(selection.anchor, sql.length), Math.min(selection.head, sql.length)),
    annotations: [externalDocument.of(true), Transaction.addToHistory.of(false)] });
}

export function SqlCodeEditor(props: SqlCodeEditorProps) {
  const host = useRef<HTMLDivElement>(null), viewRef = useRef<EditorView | null>(null), findInput = useRef<HTMLInputElement>(null);
  const latest = useRef(props); latest.current = props;
  const language = useRef(new Compartment()), access = useRef(new Compartment()), accessibility = useRef(new Compartment());
  const composing = useRef(false), request = useRef<AbortController | null>(null), alive = useRef(false);
  const completionPermit = useRef<{ document: Text; pos: number; provider: SqlCompletionProvider; engine: string } | null>(null);
  const labelId = useId(), [notice, setNotice] = useState(""), [findOpen, setFindOpen] = useState(false), [findQuery, setFindQuery] = useState("");
  function abortCompletion(): void { completionPermit.current = null; request.current?.abort(); request.current = null; }
  function ime(view: EditorView, event?: KeyboardEvent): boolean { return composing.current || view.composing || event?.isComposing === true || event?.keyCode === 229; }

  useLayoutEffect(() => {
    if (!host.current) return;
    alive.current = true;
    function notify(message: string): void { queueMicrotask(() => { if (alive.current) setNotice(message); }); }
    const source: CompletionSource = async (context) => {
      const view = viewRef.current, provider = latest.current.completionProvider;
      // CM can preserve explicit=true when it re-queries after typing. Only the key handler grants a one-use permit.
      const permit = completionPermit.current; completionPermit.current = null;
      if (!context.explicit || !view || !view.hasFocus || ime(view) || !provider || !dialect(latest.current.engine) || !permit ||
        permit.document !== context.state.doc || permit.pos !== context.pos || permit.provider !== provider || permit.engine !== latest.current.engine ||
        view.state.doc !== context.state.doc || !view.state.selection.main.empty || view.state.selection.main.head !== context.pos) return null;
      const node = syntaxTree(context.state).resolveInner(context.pos, -1);
      if (/String|Comment|QuotedIdentifier/.test(node.name)) return null;
      const line = context.state.doc.lineAt(context.pos), before = line.text.slice(0, context.pos - line.from);
      const match = /[\p{L}_$][\p{L}\p{N}_$]*$/u.exec(before), prefix = match?.[0] ?? "", from = context.pos - prefix.length;
      if (prefix.length > 128 || /['"`]/.test(before.charAt(before.length - prefix.length - 1))) return null;
      // minimal: only unqualified atoms, <=256 UTF-16 units of context — add qualifier resolution only with a proven SQL-scope model.
      const lookbehindStart = Math.max(0, from - COMPLETION_LOOKBEHIND);
      const lookbehind = context.state.doc.sliceString(lookbehindStart, from), boundary = lookbehind.trimEnd();
      if ((!boundary && lookbehindStart > 0) || /[.'"`/\\]$/.test(boundary)) return null;
      const boundaryEnd = lookbehindStart + boundary.length;
      if (boundaryEnd && /String|Comment|QuotedIdentifier/.test(syntaxTree(context.state).resolveInner(boundaryEnd, -1).name)) return null;
      abortCompletion(); const controller = new AbortController(); request.current = controller;
      const original = context.state.doc;
      context.addEventListener("abort", () => controller.abort(), { onDocChange: true });
      try {
        const items = await provider({ prefix, signal: controller.signal });
        if (!alive.current || controller.signal.aborted || context.aborted || request.current !== controller || latest.current.completionProvider !== provider || ime(view) || !view.state.doc.eq(original)) return null;
        let bytes = 0;
        const valid: SqlCompletionItem[] = [];
        for (const item of items.slice(0, 100)) {
          if (!["schema", "table", "column"].includes(item.kind) || !item.label || !item.insertText || item.label.length > 256 || item.insertText.length > 2048 || (item.detail?.length ?? 0) > 256 || /[\u0000-\u001f\u007f]/.test(item.label + item.insertText + (item.detail ?? ""))) continue;
          bytes += encoder.encode(item.label + item.insertText + (item.detail ?? "")).length;
          if (bytes > MAX_COMPLETION_BYTES) break;
          valid.push(item);
        }
        return { from, options: valid.map((item) => ({ label: item.label, detail: item.detail,
          type: item.kind === "column" ? "property" : item.kind === "table" ? "class" : "namespace",
          apply(target: EditorView, _completion: unknown, start: number, end: number) {
            if (ime(target) || latest.current.completionProvider !== provider || target.state.doc !== original ||
              !target.state.selection.main.empty || target.state.selection.main.head !== context.pos) { closeCompletion(target); return; }
            target.dispatch({ changes: { from: start, to: end, insert: exactSqlText(item.insertText) },
              selection: { anchor: start + item.insertText.length }, userEvent: "input.complete" });
          } })) };
      } catch { if (!controller.signal.aborted) notify("자동 완성 목록을 확인하지 못했습니다. 원문은 유지됩니다."); return null; }
      finally { if (request.current === controller) request.current = null; }
    };
    const view = new EditorView({ parent: host.current, state: EditorState.create({
      doc: exactSqlText(latest.current.sql), extensions: [
        // No default CR/CRLF splitter, formatter, casing rewrite, or automatic indentation.
        EditorState.lineSeparator.of("\n"), EditorState.allowMultipleSelections.of(false),
        language.current.of(dialect(latest.current.engine)?.extension ?? []),
        access.current.of(EditorView.editable.of(!!latest.current.canChange)),
        accessibility.current.of(EditorView.contentAttributes.of({ role: "textbox", "aria-label": "SQL 원문", "aria-multiline": "true", "aria-labelledby": labelId, "aria-readonly": String(!latest.current.canChange), ...(latest.current.describedBy ? { "aria-describedby": latest.current.describedBy } : {}), spellcheck: "false", autocapitalize: "off", autocomplete: "off" })),
        lineNumbers(), highlightActiveLineGutter(), drawSelection(), bracketMatching(), appearance, highlighting,
        // minDepth is an undo-event retention preference, not a heap-byte limit.
        history({ minDepth: 20, newGroupDelay: 500 }),
        EditorState.transactionFilter.of((transaction) => {
          if (!transaction.docChanged) return transaction;
          const next = transaction.newDoc.toString();
          if (encoder.encode(next).length > MAX_SQL_BYTES) { notify("SQL 원문은 UTF-8 1MiB 이하로 입력해 주세요. 이전 원문을 유지합니다."); return []; }
          if (!transaction.annotation(externalDocument)) {
            let accepted = false; try { accepted = latest.current.canChange?.(next) === true; } catch { /* Admission failure preserves the original document. */ }
            if (!accepted) { notify("열린 SQL 초안의 허용 범위를 넘었습니다. 이전 원문을 유지합니다."); return []; }
          }
          return transaction;
        }),
        EditorView.updateListener.of((update) => {
          if (update.docChanged || update.selectionSet) abortCompletion();
          if (!update.docChanged || update.transactions.some((transaction) => transaction.annotation(externalDocument))) return;
          try { latest.current.onChange(update.state.doc.toString()); setNotice(""); }
          catch { queueMicrotask(() => { if (alive.current) { synchronize(view, latest.current.sql); setNotice("원문 변경을 반영하지 못했습니다. 이전 원문을 유지합니다."); } }); }
        }),
        autocompletion({ activateOnTyping: false, defaultKeymap: false, override: [source], closeOnBlur: true, maxRenderedOptions: 100, icons: false }),
        Prec.highest(EditorView.domEventHandlers({
          compositionstart(_event, target) { composing.current = true; abortCompletion(); closeCompletion(target); return false; },
          compositionend() { composing.current = false; return false; },
          keydown(event, target) {
            if (ime(target, event)) return false;
            const modifier = (event.ctrlKey || event.metaKey) && !event.altKey;
            if (modifier && event.key === "Enter") { event.preventDefault(); event.stopPropagation(); abortCompletion(); closeCompletion(target); latest.current.onExecuteShortcut(); return true; }
            if (event.ctrlKey && !event.altKey && !event.metaKey && event.code === "Space") {
              event.preventDefault(); abortCompletion(); closeCompletion(target);
              const provider = latest.current.completionProvider;
              if (!provider || !dialect(latest.current.engine) || !target.state.selection.main.empty) return true;
              completionPermit.current = { document: target.state.doc, pos: target.state.selection.main.head, provider, engine: latest.current.engine };
              if (!startCompletion(target)) completionPermit.current = null;
              return true;
            }
            if (modifier && event.key.toLowerCase() === "f") { event.preventDefault(); setFindOpen(true); return true; }
            if (event.key === "Escape" && completionStatus(target.state) !== null) { abortCompletion(); closeCompletion(target); return true; }
            if (event.key === "Enter") {
              if (completionStatus(target.state) !== null) { acceptCompletion(target); return true; }
              return insertNewline(target);
            }
            return false;
          },
          paste(event, target) {
            // Insert the exact JavaScript text/plain string, bypassing linewise-paste heuristics.
            if (!event.clipboardData || !Array.from(event.clipboardData.types).includes("text/plain")) return true;
            if (!latest.current.canChange) return true;
            target.dispatch({ ...target.state.replaceSelection(exactSqlText(event.clipboardData.getData("text/plain"))), userEvent: "input.paste", scrollIntoView: true });
            return true;
          },
        })),
        keymap.of([
          ...completionKeymap.filter((binding) => ["ArrowDown", "ArrowUp", "PageDown", "PageUp"].includes(binding.key ?? "")),
          ...historyKeymap,
          ...defaultKeymap.filter((binding) => !/(^|-)Enter$/.test(binding.key ?? "")),
        ]),
      ],
    }) });
    viewRef.current = view;
    return () => { alive.current = false; abortCompletion(); viewRef.current = null; view.destroy(); };
  }, []);

  useLayoutEffect(() => { const view = viewRef.current; if (view) synchronize(view, props.sql); }, [props.sql]);
  useLayoutEffect(() => {
    const view = viewRef.current; if (!view) return;
    abortCompletion(); closeCompletion(view);
    view.dispatch({ effects: [language.current.reconfigure(dialect(props.engine)?.extension ?? []), access.current.reconfigure(EditorView.editable.of(!!props.canChange))] });
  }, [props.engine, !!props.canChange, props.completionProvider]);
  useLayoutEffect(() => { viewRef.current?.dispatch({ effects: accessibility.current.reconfigure(EditorView.contentAttributes.of({ role: "textbox", "aria-label": "SQL 원문", "aria-multiline": "true", "aria-labelledby": labelId, "aria-readonly": String(!props.canChange), ...(props.describedBy ? { "aria-describedby": props.describedBy } : {}), spellcheck: "false", autocapitalize: "off", autocomplete: "off" })) }); }, [props.describedBy, !!props.canChange]);
  useLayoutEffect(() => { if (findOpen) findInput.current?.focus(); }, [findOpen]);
  function findNext(): void {
    const view = viewRef.current; if (!view || !findQuery || composing.current) return;
    const sql = view.state.doc.toString(), start = view.state.selection.main.to;
    let index = sql.indexOf(findQuery, start); if (index < 0) index = sql.indexOf(findQuery);
    if (index < 0) { setNotice("일치하는 원문을 찾지 못했습니다."); return; }
    view.dispatch({ selection: { anchor: index, head: index + findQuery.length }, scrollIntoView: true }); view.focus(); setNotice("");
  }
  return <div className="wb-query-label wb-sql-code-editor">
    <div style={{ display: "flex", justifyContent: "space-between", gap: "var(--sp-3)" }}><span id={labelId}>SQL 원문</span><button type="button" className="wb-text-button" onClick={() => setFindOpen((open) => !open)}>찾기</button></div>
    {findOpen && <div style={{ display: "flex", gap: "var(--sp-3)", flexWrap: "wrap" }}>
      <input ref={findInput} aria-label="SQL에서 찾기" value={findQuery} maxLength={256} onChange={(event) => setFindQuery(event.currentTarget.value)}
        onCompositionStart={() => { composing.current = true; }} onCompositionEnd={() => { composing.current = false; }}
        onKeyDown={(event) => { if (event.nativeEvent.isComposing || event.nativeEvent.keyCode === 229 || composing.current) return; if (event.key === "Enter") { event.preventDefault(); findNext(); } else if (event.key === "Escape") { event.preventDefault(); setFindOpen(false); viewRef.current?.focus(); } }} />
      <button type="button" className="button button-secondary" disabled={!findQuery} onClick={findNext}>다음 찾기</button>
      <button type="button" className="wb-text-button" onClick={() => { setFindOpen(false); viewRef.current?.focus(); }}>찾기 닫기</button>
    </div>}
    <div ref={host} className="wb-sql-code-host" style={{ minWidth: 0 }} />
    {!props.canChange && <span role="status">입력 범위 검증이 준비되지 않아 원문을 읽기 전용으로 표시합니다.</span>}
    {notice && <span className="wb-muted" role="status">{notice}</span>}
  </div>;
}

export default SqlCodeEditor;
