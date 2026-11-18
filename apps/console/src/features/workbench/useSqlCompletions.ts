import { useCallback, useEffect, useRef } from "react";
import { workbenchPages, type WorkbenchClient } from "../../lib/workbench";
import type { CatalogDetail, CatalogItem, Session, SessionRef, WorkbenchPage } from "../../lib/workbenchTypes";

export interface SqlCompletionItem {
  label: string;
  insertText: string;
  kind: "schema" | "table" | "column";
  detail?: string;
}
export type SqlCompletionProvider = (request: { prefix: string; signal: AbortSignal }) => Promise<readonly SqlCompletionItem[]>;
export interface SqlCompletionOptions {
  client: WorkbenchClient;
  session: Session | null;
  engine: string;
  schema: CatalogItem | null;
  relation: CatalogItem | null;
  /** Disable during execution, snapshots/manual leases, composition, modal suspension, etc. */
  enabled?: boolean;
}

const PAGE_SIZE = 100;
const MAX_COLUMNS = 512;
const MAX_COLUMN_PAGES = 6;
const MAX_SUGGESTIONS = 100;
const MAX_SUGGESTION_BYTES = 64 * 1024;
const PREFIX_DELAY_MS = 120;
const utf8 = new TextEncoder();
type SupportedEngine = "postgresql" | "mysql" | "mariadb" | "sqlite";
function supportedEngine(engine: string): engine is SupportedEngine {
  return engine === "postgresql" || engine === "mysql" || engine === "mariadb" || engine === "sqlite";
}

/** Mirrors native types.rs::quote_identifier, including its MySQL backslash refusal. */
export function quoteCompletionIdentifier(engine: string, name: string): string | null {
  if (!supportedEngine(engine) || !name || name.length > 1024 || utf8.encode(name).byteLength > 1024 || name.includes("\0")) return null;
  if ((engine === "mysql" || engine === "mariadb") && name.includes("\\")) return null;
  const quote = engine === "mysql" || engine === "mariadb" ? "`" : '"';
  return quote + name.split(quote).join(quote + quote) + quote;
}

function delay(signal: AbortSignal): Promise<boolean> {
  return new Promise((resolve) => {
    if (signal.aborted) { resolve(false); return; }
    const aborted = () => { clearTimeout(timer); resolve(false); };
    const timer = setTimeout(() => { signal.removeEventListener("abort", aborted); resolve(true); }, PREFIX_DELAY_MS);
    signal.addEventListener("abort", aborted, { once: true });
  });
}

/**
 * Call only for an explicit completion request (Ctrl+Space), never for document-change events.
 * Every result is a name suggestion, never permission, SQL validation, or an execution plan.
 */
export function useSqlCompletions(options: SqlCompletionOptions): SqlCompletionProvider {
  const { client, session, engine, schema, relation } = options;
  const enabled = options.enabled !== false && session?.state === "ready";
  const contextKey = JSON.stringify([
    session && [session.workspaceId, session.connectionProfileId, session.databaseTargetId, session.sessionId,
      session.connectionGeneration, session.descriptorRevision, session.mode, session.state],
    engine, schema?.object ?? null, schema?.name ?? null, relation?.object ?? null, relation?.name ?? null, enabled,
  ]);
  const latest = useRef({ client, session, engine, schema, relation, enabled, contextKey });
  latest.current = { client, session, engine, schema, relation, enabled, contextKey };
  const mounted = useRef(false);
  const active = useRef<AbortController | null>(null);
  // WorkbenchClient.request has no abortable transport. Never stack unabortable native requests.
  const nativeInFlight = useRef(false);
  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; active.current?.abort(); };
  }, []);
  useEffect(() => { active.current?.abort(); }, [client, contextKey]);

  return useCallback<SqlCompletionProvider>(async ({ prefix, signal }) => {
    const captured = latest.current;
    if (captured.client !== client || captured.contextKey !== contextKey) return [];
    active.current?.abort();
    if (!mounted.current || signal.aborted || !captured.enabled || !captured.session || !captured.client.available ||
      !supportedEngine(captured.engine) || prefix.length > 256 || prefix.includes("\0") || utf8.encode(prefix).byteLength > 256) return [];
    const controller = new AbortController();
    active.current = controller;
    const abort = () => controller.abort();
    signal.addEventListener("abort", abort, { once: true });
    const current = () => mounted.current && !signal.aborted && !controller.signal.aborted && active.current === controller &&
      latest.current.client === captured.client && latest.current.contextKey === captured.contextKey;
    const reference: SessionRef = { sessionId: captured.session.sessionId, connectionGeneration: captured.session.connectionGeneration };
    const suggestions: SqlCompletionItem[] = [];
    const seen = new Set<string>();
    let suggestionBytes = 0;
    const foldedPrefix = prefix.toLowerCase();
    const add = (kind: SqlCompletionItem["kind"], name: string, detail?: string) => {
      if (suggestions.length >= MAX_SUGGESTIONS) return;
      const insertText = quoteCompletionIdentifier(captured.engine, name);
      if (insertText === null || !name.toLowerCase().startsWith(foldedPrefix)) return;
      const key = `${kind}:${insertText}`;
      if (seen.has(key)) return;
      const item: SqlCompletionItem = { label: name, insertText, kind, ...(detail ? { detail: detail.slice(0, 128) } : {}) };
      const bytes = utf8.encode(item.label).byteLength + utf8.encode(item.insertText).byteLength + utf8.encode(item.detail ?? "").byteLength + 96;
      if (suggestionBytes + bytes > MAX_SUGGESTION_BYTES) return;
      suggestionBytes += bytes; seen.add(key); suggestions.push(item);
    };
    async function metadataPage<T>(suffix: string, limit: number, load: () => Promise<WorkbenchPage<T>>): Promise<WorkbenchPage<T>> {
      if (!current()) throw new Error("completion cancelled");
      // Reuse the existing bounded LRU; namespace its entries by the full current native/session/object basis.
      const cacheKey = `sql-completion:${captured.contextKey}:${suffix}`;
      const cached = workbenchPages.get<WorkbenchPage<T>>(cacheKey);
      if (cached) {
        if (!Array.isArray(cached.items) || cached.items.length > limit) throw new Error("completion page limit");
        return cached;
      }
      if (nativeInFlight.current) throw new Error("completion request already pending");
      nativeInFlight.current = true;
      try {
        const page = await load();
        if (!current() || !Array.isArray(page.items) || page.items.length > limit) throw new Error("completion cancelled or oversized");
        workbenchPages.put(cacheKey, page);
        return page;
      } finally { nativeInFlight.current = false; }
    }
    try {
      if (!await delay(controller.signal) || !current()) return [];
      // Only the selected table is described. At most 512 columns / six <=100-item pages; never a database-wide crawl.
      if (captured.relation) {
        let cursor: string | null = null;
        let inspected = 0;
        let pages = 0;
        const cursors = new Set<string>();
        do {
          if (!current()) return [];
          // Short nonempty pages must not turn the 512-column bound into 512 native calls.
          if (pages >= MAX_COLUMN_PAGES) break;
          pages++;
          const limit = Math.min(PAGE_SIZE, MAX_COLUMNS - inspected);
          const page: WorkbenchPage<CatalogDetail> = await metadataPage<CatalogDetail>(JSON.stringify(["columns", captured.relation.object, cursor, limit]), limit,
            () => captured.client.request("catalog.describe", { session: reference, object: captured.relation!.object, part: "columns", page: { cursor, limit } }));
          for (const item of page.items) {
            inspected++;
            if (item.kind === "column") add("column", item.column.name, item.column.typeName);
          }
          cursor = page.nextCursor;
          if (!page.items.length || cursor === null || cursors.has(cursor)) break;
          cursors.add(cursor);
        } while (pages < MAX_COLUMN_PAGES && inspected < MAX_COLUMNS && suggestions.length < MAX_SUGGESTIONS);
      }
      if (!current()) return [];
      if (suggestions.length < MAX_SUGGESTIONS && captured.relation) add("table", captured.relation.name, captured.schema?.name);
      // One <=100-item namespace page only. No automatic cursor walk or root-wide table/column preloading.
      if (suggestions.length < MAX_SUGGESTIONS) {
        const parent = captured.schema?.object ?? null;
        const kind = parent ? "relations" as const : "schemas" as const;
        const page = await metadataPage<CatalogItem>(JSON.stringify([kind, parent, prefix, PAGE_SIZE]), PAGE_SIZE,
          () => captured.client.request("catalog.page", { session: reference, parent, kind, search: prefix, page: { cursor: null, limit: PAGE_SIZE } }));
        for (const item of page.items) {
          if (kind === "schemas" && item.kind === "schema") add("schema", item.name);
          if (kind === "relations" && (item.kind === "table" || item.kind === "view")) add("table", item.name, captured.schema?.name);
        }
      }
      if (captured.schema) add("schema", captured.schema.name);
      return current() ? suggestions : [];
    } catch {
      // Permission, busy lease, stale object, unavailable native transport, and cache bounds all fail without suggestions.
      // Keep the editor usable; never retry automatically or fall back to a different connection/schema.
      return [];
    } finally {
      signal.removeEventListener("abort", abort);
      if (active.current === controller) active.current = null;
    }
  }, [client, contextKey]);
}
