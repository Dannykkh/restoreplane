import { useEffect, useRef, useState, type FormEvent } from "react";
import {
  getWorkItem,
  listBackupScopeObjects,
  runBackupScopeDiscovery,
  saveBackupScope,
  type BackupScopeObjectListRequest,
} from "../lib/api";
import { asServiceError, type ServiceError } from "../lib/errors";
import { useMutationAttemptRegistry } from "../lib/mutationAttempts";
import type {
  BackupScope,
  BackupScopeDiscoveryRun,
  BackupScopeMode,
  BackupScopeObjectPage,
  BackupScopeSaveRequest,
  BackupScopeSaveResult,
  DatabaseTarget,
  DiscoveredDatabaseObject,
  WorkItemStatus,
} from "../lib/types";
import { ScopeCoverage } from "./ScopeCoverage";
import { StatusBadge, toneForState } from "./StatusBadge";

const DISCOVERY_STALE_AFTER_MS = 24 * 60 * 60 * 1_000;
const OBJECT_FILTER_DEBOUNCE_MS = 250;

type ObjectEligibilityFilter = DiscoveredDatabaseObject["eligibility"] | "";

type RunDiscovery = (
  databaseTargetId: string,
  idempotencyKey?: string,
) => Promise<BackupScopeDiscoveryRun>;
type ReadWorkItem = (workItemId: string) => Promise<Pick<WorkItemStatus, "state">>;
type LoadObjects = (request: BackupScopeObjectListRequest) => Promise<BackupScopeObjectPage>;
type SaveScope = (
  request: BackupScopeSaveRequest,
  idempotencyKey?: string,
) => Promise<BackupScopeSaveResult>;

interface BackupScopeEditorProps {
  target: DatabaseTarget;
  replacingScope?: BackupScope;
  runDiscovery?: RunDiscovery;
  readWorkItem?: ReadWorkItem;
  loadObjects?: LoadObjects;
  saveScope?: SaveScope;
  onSaved: (result: BackupScopeSaveResult) => void;
  onCancel?: () => void;
}

export function BackupScopeEditor({
  target,
  replacingScope,
  runDiscovery = runBackupScopeDiscovery,
  readWorkItem = getWorkItem,
  loadObjects = listBackupScopeObjects,
  saveScope = saveBackupScope,
  onSaved,
  onCancel,
}: BackupScopeEditorProps) {
  const mutationAttempts = useMutationAttemptRegistry();
  const initialMode = replacingScope?.mode ?? "whole_database";
  const [mode, setMode] = useState<BackupScopeMode>(initialMode);
  const [displayName, setDisplayName] = useState(
    replacingScope?.displayName ?? defaultScopeName(target.displayName, initialMode),
  );
  const [objects, setObjects] = useState<DiscoveredDatabaseObject[]>([]);
  const [selectedObjectIds, setSelectedObjectIds] = useState<Set<string>>(new Set());
  const [objectPage, setObjectPage] = useState<BackupScopeObjectPage | null>(null);
  const [discoveryRunId, setDiscoveryRunId] = useState<string | null>(null);
  const [inventoryVersion, setInventoryVersion] = useState(0);
  const [search, setSearch] = useState("");
  const [schemaName, setSchemaName] = useState("");
  const [eligibility, setEligibility] = useState<ObjectEligibilityFilter>("");
  const [busy, setBusy] = useState<"discovering" | "loading" | "saving" | null>(null);
  const [error, setError] = useState<ServiceError | null>(null);
  const objectRequestSequence = useRef(0);

  const staleDiscovery = Boolean(
    objectPage && Date.now() - objectPage.observedAtMs > DISCOVERY_STALE_AFTER_MS,
  );
  const canSave = Boolean(
    displayName.trim() &&
    !busy &&
    (mode === "whole_database" ||
      (objectPage && !staleDiscovery && selectedObjectIds.size > 0)),
  );

  useEffect(() => {
    if (!discoveryRunId) return;
    const sequence = ++objectRequestSequence.current;
    const timeout = window.setTimeout(() => {
      setBusy("loading");
      setError(null);
      void loadObjects({
        databaseTargetId: target.id,
        discoveryRunId,
        cursor: null,
        limit: 100,
        ...objectFilters(search, schemaName, eligibility),
      }).then((page) => {
        if (sequence !== objectRequestSequence.current) return;
        setObjects(page.items);
        setObjectPage(page);
      }).catch((failure) => {
        if (sequence !== objectRequestSequence.current) return;
        setError(asServiceError(failure));
      }).finally(() => {
        if (sequence === objectRequestSequence.current) setBusy(null);
      });
    }, OBJECT_FILTER_DEBOUNCE_MS);
    return () => window.clearTimeout(timeout);
  }, [discoveryRunId, eligibility, inventoryVersion, loadObjects, schemaName, search, target.id]);

  function changeMode(nextMode: BackupScopeMode): void {
    setDisplayName((current) =>
      current === defaultScopeName(target.displayName, mode)
        ? defaultScopeName(target.displayName, nextMode)
        : current,
    );
    setMode(nextMode);
    setError(null);
  }

  function toggleObject(engineObjectId: string): void {
    setSelectedObjectIds((current) => {
      const next = new Set(current);
      if (next.has(engineObjectId)) next.delete(engineObjectId);
      else next.add(engineObjectId);
      return next;
    });
  }

  async function discover(): Promise<void> {
    objectRequestSequence.current += 1;
    setBusy("discovering");
    setError(null);
    try {
      const payload = { databaseTargetId: target.id };
      const run = await mutationAttempts.run(
        "backup-scope-discovery",
        "backup_scope.discovery_run",
        payload,
        (key) => runDiscovery(target.id, key),
      );
      await waitForDiscovery(run, readWorkItem);
      setObjects([]);
      setObjectPage(null);
      setSelectedObjectIds(new Set());
      setDiscoveryRunId(run.operationRunId);
      setInventoryVersion((current) => current + 1);
    } catch (failure) {
      setError(asServiceError(failure));
    } finally {
      setBusy(null);
    }
  }

  async function loadNextPage(): Promise<void> {
    if (!objectPage?.nextCursor) return;
    const sequence = ++objectRequestSequence.current;
    setBusy("loading");
    setError(null);
    try {
      const page = await loadObjects({
        databaseTargetId: target.id,
        discoveryRunId: objectPage.discoveryRunId,
        cursor: objectPage.nextCursor,
        limit: 100,
        ...objectFilters(search, schemaName, eligibility),
      });
      if (sequence !== objectRequestSequence.current) return;
      setObjects((current) => mergeObjects(current, page.items));
      setObjectPage(page);
    } catch (failure) {
      if (sequence !== objectRequestSequence.current) return;
      setError(asServiceError(failure));
    } finally {
      if (sequence === objectRequestSequence.current) setBusy(null);
    }
  }

  function beginFilterChange(): void {
    objectRequestSequence.current += 1;
    setBusy("loading");
  }

  async function submit(event: FormEvent<HTMLFormElement>): Promise<void> {
    event.preventDefault();
    if (!canSave) return;
    setBusy("saving");
    setError(null);
    try {
      const request: BackupScopeSaveRequest = {
        databaseTargetId: target.id,
        displayName: displayName.trim(),
        mode,
        ...(mode === "explicit_tables" && objectPage
          ? { discoveryRunId: objectPage.discoveryRunId }
          : {}),
        engineObjectIds:
          mode === "explicit_tables" ? Array.from(selectedObjectIds) : [],
        ...(replacingScope ? { replacesScopeId: replacingScope.id } : {}),
      };
      const result = await mutationAttempts.run(
        "backup-scope-save",
        "backup_scope.save",
        request,
        (key) => saveScope(request, key),
      );
      onSaved(result);
    } catch (failure) {
      setError(asServiceError(failure));
    } finally {
      setBusy(null);
    }
  }

  return (
    <form className="form-stack" onSubmit={(event) => void submit(event)}>
      <div aria-live="polite" className="live-region">
        {busy === "discovering"
          ? "테이블 검색 작업을 시작하는 중입니다."
          : busy === "loading"
            ? "검색된 테이블을 불러오는 중입니다."
            : busy === "saving"
              ? "Backup Scope를 저장하는 중입니다."
              : `${selectedObjectIds.size}개 테이블 선택됨`}
      </div>
      <label className="field">
        <span>범위 이름</span>
        <input
          name="scopeName"
          value={displayName}
          onChange={(event) => setDisplayName(event.target.value)}
          autoComplete="off"
        />
      </label>
      <fieldset className="choice-fieldset">
        <legend>Backup Scope</legend>
        <label>
          <input
            type="radio"
            name="scopeMode"
            value="whole_database"
            aria-label="전체 데이터베이스"
            checked={mode === "whole_database"}
            onChange={() => changeMode("whole_database")}
          />
          <span><strong>전체 데이터베이스</strong> 현재와 이후 테이블을 함께 보호</span>
        </label>
        <label>
          <input
            type="radio"
            name="scopeMode"
            value="explicit_tables"
            aria-label="선택한 테이블만"
            checked={mode === "explicit_tables"}
            onChange={() => changeMode("explicit_tables")}
          />
          <span><strong>선택한 테이블만</strong> 명시한 테이블 집합만 보호</span>
        </label>
      </fieldset>

      {mode === "whole_database" ? (
        <ScopeCoverage
          mode="whole_database"
          selectedObjectCount={null}
          eligibleObjectCount={null}
        />
      ) : (
        <section className="form-stack" aria-labelledby="scope-object-title" aria-busy={Boolean(busy)}>
          <header className="section-heading">
            <div>
              <span className="eyebrow">Database inventory</span>
              <h3 id="scope-object-title">테이블 선택</h3>
            </div>
            <button
              type="button"
              className="button button-secondary"
              disabled={Boolean(busy)}
              onClick={() => void discover()}
            >
              {busy === "discovering" ? "검색 요청 중" : "테이블 검색"}
            </button>
          </header>
          <ScopeCoverage
            mode="explicit_tables"
            selectedObjectCount={selectedObjectIds.size}
            eligibleObjectCount={objectPage?.eligibleCount ?? null}
            newObjectCount={replacingScope?.newObjectCount ?? 0}
            missingObjectCount={replacingScope?.missingObjectCount ?? 0}
          />
          {objectPage && (
            <div className="form-grid">
              <label className="field">
              <span>테이블 검색어</span>
              <input
                name="objectSearch"
                type="search"
                value={search}
                onChange={(event) => {
                  beginFilterChange();
                  setSearch(event.target.value);
                }}
                placeholder="schema.table"
              />
              </label>
            <label className="field">
              <span>Schema</span>
              <input
                name="objectSchema"
                value={schemaName}
                onChange={(event) => {
                  beginFilterChange();
                  setSchemaName(event.target.value);
                }}
                placeholder="public"
              />
              </label>
            <label className="field">
              <span>Eligibility</span>
              <select
                name="objectEligibility"
                value={eligibility}
                onChange={(event) => {
                  beginFilterChange();
                  setEligibility(event.target.value as ObjectEligibilityFilter);
                }}
              >
                <option value="">All</option>
                <option value="eligible">Eligible</option>
                <option value="excluded">Excluded</option>
                <option value="unsupported">Unsupported</option>
              </select>
              </label>
            </div>
          )}
          {objectPage && (
            <p className="mono muted-copy" data-testid="object-filter-total">
              Filtered total: {objectPage.totalCount}
            </p>
          )}
          {staleDiscovery && (
            <p role="alert" className="muted-copy">
              검색 근거가 24시간보다 오래되었습니다. 테이블 검색을 다시 실행해야 저장할 수 있습니다.
            </p>
          )}
          {objectPage && objectPage.totalCount === 0 && objects.length === 0 && (
            <p className="muted-copy" role="status">
              {hasObjectFilters(search, schemaName, eligibility)
                ? "No objects match the server-side filters."
                : "No objects were discovered."}
            </p>
          )}
          {objects.length > 0 && (
            <fieldset className="target-picker">
              <legend>발견된 테이블</legend>
              <p>엔진이 제공한 식별자를 그대로 저장합니다. 제외 항목은 선택할 수 없습니다.</p>
              {objects.map((object) => {
                const eligible = object.eligibility === "eligible";
                const objectName = qualifiedObjectName(object);
                return (
                  <label key={object.engineObjectId} data-eligibility={object.eligibility}>
                    <input
                      type="checkbox"
                      name="scopeObjects"
                      value={object.engineObjectId}
                      checked={selectedObjectIds.has(object.engineObjectId)}
                      disabled={!eligible}
                      onChange={() => toggleObject(object.engineObjectId)}
                    />
                    <span>
                      <strong>{objectName}</strong>
                      <small className="mono">{object.objectType} · {object.engineObjectId}</small>
                    </span>
                    <StatusBadge
                      label={object.eligibility}
                      tone={toneForState(object.eligibility)}
                      reason={object.exclusionReason ?? undefined}
                    />
                  </label>
                );
              })}
            </fieldset>
          )}
          {objectPage?.nextCursor && (
            <button
              type="button"
              className="button button-secondary"
              disabled={Boolean(busy)}
              onClick={() => void loadNextPage()}
            >
              다음 페이지
            </button>
          )}
        </section>
      )}

      {error && (
        <p role="alert" className="muted-copy">
          {error.message} {error.action} 입력과 선택은 유지됩니다.
          <span className="mono error-code"> {error.code}</span>
        </p>
      )}
      <div className="form-submit role-pair">
        {onCancel && (
          <button type="button" className="button button-secondary" onClick={onCancel}>
            취소
          </button>
        )}
        <button type="submit" className="button button-primary" disabled={!canSave}>
          {busy === "saving" ? "저장 중" : "범위 저장"}
        </button>
      </div>
    </form>
  );
}

function defaultScopeName(databaseName: string, mode: BackupScopeMode): string {
  return mode === "whole_database"
    ? `${databaseName} 전체 백업`
    : `${databaseName} 선택 백업`;
}

function qualifiedObjectName(object: DiscoveredDatabaseObject): string {
  return object.schemaName ? `${object.schemaName}.${object.objectName}` : object.objectName;
}

function objectFilters(
  search: string,
  schemaName: string,
  eligibility: ObjectEligibilityFilter,
): Pick<BackupScopeObjectListRequest, "search" | "schemaName" | "eligibility"> {
  return {
    search: search.trim() || null,
    schemaName: schemaName.trim() || null,
    eligibility: eligibility || null,
  };
}

function hasObjectFilters(
  search: string,
  schemaName: string,
  eligibility: ObjectEligibilityFilter,
): boolean {
  return Boolean(search.trim() || schemaName.trim() || eligibility);
}

function mergeObjects(
  current: readonly DiscoveredDatabaseObject[],
  incoming: readonly DiscoveredDatabaseObject[],
): DiscoveredDatabaseObject[] {
  const merged = new Map(current.map((object) => [object.engineObjectId, object]));
  incoming.forEach((object) => merged.set(object.engineObjectId, object));
  return Array.from(merged.values());
}

async function waitForDiscovery(
  run: BackupScopeDiscoveryRun,
  readWorkItem: ReadWorkItem,
): Promise<void> {
  if (run.state === "succeeded") return;
  if (run.state === "failed" || run.state === "cancelled") {
    throw "BACKUP_SCOPE_DISCOVERY_FAILED";
  }
  for (let attempt = 0; attempt < 120; attempt += 1) {
    const work = await readWorkItem(run.workItemId);
    if (work.state === "succeeded") return;
    if (["failed", "failed_terminal", "cancelled"].includes(work.state)) {
      throw "BACKUP_SCOPE_DISCOVERY_FAILED";
    }
    await delay(1_000);
  }
  throw "BACKUP_SCOPE_DISCOVERY_TIMEOUT";
}

function delay(durationMs: number): Promise<void> {
  return new Promise((resolve) => window.setTimeout(resolve, durationMs));
}
