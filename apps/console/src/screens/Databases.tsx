import { useMemo, useState } from "react";
import { BackupScopeEditor } from "../components/BackupScopeEditor";
import { Dialog } from "../components/Dialog";
import { EvidenceRow, Inspector } from "../components/Inspector";
import { InventoryTable } from "../components/InventoryTable";
import { LoadingState, PageState } from "../components/PageState";
import { FilterField, PageHeader } from "../components/PageHeader";
import { StatusBadge, toneForState } from "../components/StatusBadge";
import { ScopeCoverage } from "../components/ScopeCoverage";
import { listBackupScopes, listDatabaseTargets } from "../lib/api";
import { formatDateTime, formatFreshness } from "../lib/format";
import {
  collectCursorPages,
  INVENTORY_PAGE_LIMIT,
  type CollectedCursorPage,
} from "../lib/pagination";
import { useServiceQuery } from "../lib/queries";
import { transitionTo } from "../lib/route";
import type {
  BackupScope,
  BackupScopeSaveResult,
  DatabaseTarget,
  RouteState,
} from "../lib/types";

interface DatabasesProps {
  route: RouteState;
}

export function Databases({ route }: DatabasesProps) {
  const [editorOpen, setEditorOpen] = useState(false);
  const [inspectedScopeId, setInspectedScopeId] = useState<string | null>(null);
  const [savedScope, setSavedScope] = useState<BackupScopeSaveResult | null>(null);
  const query = useServiceQuery<CollectedCursorPage<DatabaseTarget>>("database-targets", () =>
    collectCursorPages((cursor) => listDatabaseTargets({ cursor, limit: INVENTORY_PAGE_LIMIT })),
  );
  const scopeQuery = useServiceQuery<CollectedCursorPage<BackupScope>>(
    "database-backup-scopes",
    () => collectCursorPages((cursor) =>
      listBackupScopes({ cursor, limit: INVENTORY_PAGE_LIMIT })
    ),
  );
  const allTargets = useMemo(() => query.data?.items ?? [], [query.data]);
  const scopes = useMemo(
    () => scopeQuery.data?.items ?? [],
    [scopeQuery.data],
  );
  const filter = route.filter ?? "all";
  const rows = useMemo(
    () =>
      allTargets.filter((target) => {
        if (filter === "all") return true;
        if (filter === "unprotected") return target.protectionCompliance === "not_configured";
        if (filter === "drift") {
          return target.missingSinceMs !== null || scopes.some(
            (scope) => scope.databaseTargetId === target.id && scope.driftState === "drifted",
          );
        }
        if (filter === "rpo") return target.rpoViolation;
        if (filter === "verification") return target.verificationOverdue;
        return target.connectionId === filter || target.engine === filter;
      }),
    [allTargets, filter, scopes],
  );
  const selected =
    rows.find((target) => target.id === route.selected) ?? rows[0] ?? null;
  const selectedScopes = selected
    ? scopes.filter((scope) => scope.databaseTargetId === selected.id && scope.state !== "retired")
    : [];
  const inspectedScope =
    selectedScopes.find((scope) => scope.id === inspectedScopeId) ??
    selectedScopes[0] ??
    null;

  if (query.status === "loading" && !query.data) return <LoadingState label="Database Target 근거를 불러오는 중입니다." />;
  if (query.status === "error" && !query.data) {
    return (
      <PageState
        kind="error"
        title="Database inventory를 읽지 못했습니다."
        detail="보호 여부를 알 수 없으므로 빈 목록을 정상 상태로 표시하지 않습니다."
        error={query.error}
        action={<button type="button" className="button button-secondary" onClick={query.refresh}>다시 연결</button>}
      />
    );
  }

  return (
    <div className="page-stack">
      <PageHeader
        eyebrow="Protected objects"
        title="Databases"
        description="Connection에서 선택한 Database Target의 보호, drift, RPO, 검증 신선도를 한 목록에서 비교합니다."
        meta={query.fetchedAt && <span>근거 {formatFreshness(query.fetchedAt)}</span>}
        actions={
          <button
            type="button"
            className="button button-primary"
            disabled={
              !selected ||
              query.data?.complete !== true ||
              scopeQuery.status !== "ready" ||
              scopeQuery.data?.complete !== true ||
              selectedScopes.length === 0
            }
            onClick={() =>
              selected &&
              transitionTo({
                page: "backup-plans",
                selected: selected.id,
                action: "new",
              })
            }
          >
            선택 DB로 Plan 만들기
          </button>
        }
      />
      <div className="filter-bar">
        <FilterField label="관찰할 상태">
          <select
            name="databaseFilter"
            value={filter}
            onChange={(event) =>
              transitionTo(
                {
                  page: "databases",
                  filter: event.target.value === "all" ? undefined : event.target.value,
                },
                true,
              )
            }
          >
            <option value="all">전체 Target</option>
            <option value="unprotected">미보호</option>
            <option value="rpo">RPO 위반</option>
            <option value="verification">검증 기한 초과</option>
            <option value="drift">Discovery drift</option>
            <option value="postgresql">PostgreSQL</option>
            <option value="mysql">MySQL</option>
            <option value="mariadb">MariaDB</option>
          </select>
        </FilterField>
        <span className="filter-summary">
          {rows.length} / {allTargets.length} targets
          {query.data?.complete === false ? " · partial inventory" : ""}
        </span>
      </div>
      {(query.data?.complete === false || scopeQuery.data?.complete === false) && (
        <PageState
          kind="partial"
          title="Database inventory is partial."
          detail="The bounded cursor scan did not reach a verified end. Filters and scope counts apply only to the rows shown."
        />
      )}
      {query.status === "error" && query.data && (
        <PageState
          kind="stale"
          title="마지막 Database 근거를 표시합니다."
          detail="서비스가 새 projection을 제공할 때까지 작업 버튼을 신중하게 사용하세요."
          error={query.error}
        />
      )}
      {scopeQuery.status === "loading" && (
        <LoadingState label="Database별 Backup Scope를 불러오는 중입니다." />
      )}
      {scopeQuery.status === "error" && (
        <PageState
          kind="partial"
          title="Backup Scope 근거를 읽지 못했습니다."
          detail="Database 상태는 표시하지만 범위가 확인되기 전에는 새 범위나 Plan을 만들지 않습니다."
          error={scopeQuery.error}
          action={<button type="button" className="button button-secondary" onClick={scopeQuery.refresh}>범위 다시 조회</button>}
        />
      )}
      {savedScope && (
        <p className="muted-copy" role="status">
          Backup Scope {savedScope.scopeId}를 저장했습니다. 새 Plan revision에서 선택할 수 있습니다.
        </p>
      )}
      <div className="inventory-layout">
        <section className="inventory-main" aria-labelledby="database-list-title">
          <header className="section-heading">
            <div>
              <span className="eyebrow">Protection inventory</span>
              <h2 id="database-list-title">Database Targets</h2>
            </div>
          </header>
          <InventoryTable<DatabaseTarget>
            ariaLabel="Database Target inventory"
            rows={rows}
            rowKey={(row) => row.id}
            selectedId={selected?.id ?? null}
            onSelect={(row) =>
              transitionTo({ ...route, selected: row.id, action: undefined }, true)
            }
            emptyMessage="이 필터에 해당하는 Database Target이 없습니다."
            columns={[
              {
                key: "database",
                header: "Database",
                render: (row) => (
                  <span className="resource-name" title={row.displayName}>
                    <strong>{row.displayName}</strong>
                    <small>{row.connectionName}</small>
                  </span>
                ),
              },
              {
                key: "engine",
                header: "Engine",
                className: "mono",
                render: (row) => row.engine,
              },
              {
                key: "protection",
                header: "Protection",
                render: (row) => (
                  <div className="status-stack">
                    <StatusBadge
                      label={row.protectionCompliance}
                      tone={toneForState(row.protectionCompliance)}
                      evidenceAtMs={row.evidenceAtMs}
                    />
                    <span className="mono">{row.protectionScope}</span>
                  </div>
                ),
              },
              {
                key: "rpo",
                header: "RPO",
                render: (row) => (
                  <StatusBadge
                    label={row.rpoViolation ? "violation" : row.latestSuccessAtMs ? "within RPO" : "unknown"}
                    tone={row.rpoViolation ? "danger" : row.latestSuccessAtMs ? "success" : "neutral"}
                    evidenceAtMs={row.latestSuccessAtMs}
                  />
                ),
              },
              {
                key: "verification",
                header: "Verification",
                render: (row) => (
                  <StatusBadge
                    label={row.verificationOverdue ? "overdue" : row.verificationState}
                    tone={
                      row.verificationOverdue || row.verificationState === "pending"
                        ? "warning"
                        : row.verificationState === "current"
                          ? "success"
                          : "neutral"
                    }
                  />
                ),
              },
              {
                key: "scope",
                header: "Scopes",
                className: "mono",
                render: (row) => scopes.filter(
                  (scope) => scope.databaseTargetId === row.id && scope.state !== "retired",
                ).length,
              },
            ]}
          />
        </section>
        {selected ? (
          <Inspector
            eyebrow="Database target"
            title={selected.displayName}
            actions={
              <>
                <button
                  type="button"
                  className="button button-secondary"
                  disabled={scopeQuery.status !== "ready" || scopeQuery.data?.complete !== true}
                  onClick={() => setEditorOpen(true)}
                >
                  새 선택 범위
                </button>
                <button
                  type="button"
                  className="button button-primary"
                  disabled={
                    scopeQuery.status !== "ready" ||
                    scopeQuery.data?.complete !== true ||
                    selectedScopes.length === 0
                  }
                  onClick={() =>
                    transitionTo({
                      page: "backup-plans",
                      selected: selected.id,
                      action: "new",
                    })
                  }
                >
                  Backup Plan 시작
                </button>
              </>
            }
          >
            <dl className="evidence-list">
              <EvidenceRow label="Connection" value={selected.connectionName} />
              <EvidenceRow label="Engine" value={selected.engine} mono />
              <EvidenceRow label="Target state" value={<StatusBadge label={selected.state} tone={toneForState(selected.state)} />} />
              <EvidenceRow
                label="Protection"
                value={<StatusBadge label={selected.protectionCompliance} tone={toneForState(selected.protectionCompliance)} />}
              />
              <EvidenceRow label="Coverage" value={selected.protectionScope} mono />
              <EvidenceRow
                label="Replica durability"
                value={<StatusBadge label={selected.replicaDurability} tone={toneForState(selected.replicaDurability)} />}
              />
              <EvidenceRow label="최근 성공" value={formatDateTime(selected.latestSuccessAtMs)} mono />
              <EvidenceRow label="최근 실패" value={formatDateTime(selected.latestFailureAtMs)} mono />
              <EvidenceRow label="Recovery Point" value={selected.latestRecoveryPointId ?? "없음"} mono />
            </dl>
            <section className="inspector-section" aria-labelledby="database-scope-title">
              <div className="section-heading">
                <div>
                  <span className="eyebrow">Persisted selectors</span>
                  <h3 id="database-scope-title">Backup Scopes</h3>
                </div>
                <span className="mono">{selectedScopes.length}</span>
              </div>
              {scopeQuery.status === "loading" ? (
                <LoadingState label="선택한 Database의 Backup Scope를 불러오는 중입니다." />
              ) : scopeQuery.status === "error" ? (
                <p className="muted-copy" role="alert">
                  범위 근거를 표시할 수 없습니다. 위의 범위 다시 조회를 사용하세요.
                </p>
              ) : scopeQuery.data?.complete === false ? (
                <PageState
                  kind="partial"
                  title="Scope inventory is partial."
                  detail="Refresh before creating or publishing a scope-dependent plan."
                />
              ) : selectedScopes.length === 0 ? (
                <PageState
                  kind="empty"
                  title="저장된 Backup Scope가 없습니다."
                  detail="범위를 만든 뒤 Plan revision에 연결하세요."
                  action={<button type="button" className="button button-secondary" onClick={() => setEditorOpen(true)}>범위 만들기</button>}
                />
              ) : (
                <fieldset className="target-picker" aria-labelledby="database-scope-title">
                  <legend className="sr-only">확인할 Backup Scope</legend>
                  {selectedScopes.map((scope) => (
                    <label key={scope.id}>
                      <input
                        type="radio"
                        name="inspectedBackupScope"
                        value={scope.id}
                        checked={inspectedScope?.id === scope.id}
                        onChange={() => setInspectedScopeId(scope.id)}
                      />
                      <span>
                        <strong>{scope.displayName}</strong>
                        <small>
                          {scope.mode === "whole_database" ? "전체 DB" : "명시 테이블"}
                          {` · Plan ${scope.planCount ?? "?"} · Retention ${scope.retentionPolicyCount ?? "?"}`}
                          {` · Drift ${scope.driftState}`}
                        </small>
                      </span>
                      <ScopeCoverage
                        mode={scope.mode}
                        selectedObjectCount={scope.selectedObjectCount}
                        eligibleObjectCount={scope.eligibleObjectCount}
                        newObjectCount={scope.newObjectCount}
                        missingObjectCount={scope.missingObjectCount}
                        changedObjectCount={scope.changedObjectCount}
                      />
                    </label>
                  ))}
                </fieldset>
              )}
            </section>
            {inspectedScope && (
              <section className="inspector-section" aria-labelledby="scope-recovery-title">
                <h3 id="scope-recovery-title">Scope recovery evidence</h3>
                <dl className="evidence-list">
                  <EvidenceRow label="Drift" value={<StatusBadge label={inspectedScope.driftState} tone={inspectedScope.driftState === "drifted" ? "warning" : toneForState(inspectedScope.driftState)} evidenceAtMs={inspectedScope.inventoryObservedAtMs} />} />
                  <EvidenceRow label="최근 Backup Run" value={inspectedScope.latestBackupState ?? "unknown"} mono />
                  <EvidenceRow label="Recovery Point" value={inspectedScope.latestRecoveryPointId ?? "unknown"} mono />
                  <EvidenceRow label="복원 적격성" value={<StatusBadge label={inspectedScope.restoreEligibility} tone={toneForState(inspectedScope.restoreEligibility)} evidenceAtMs={inspectedScope.recoveryEvidenceAtMs} />} />
                  <EvidenceRow label="최근 성공" value={formatDateTime(inspectedScope.latestSuccessAtMs)} mono />
                  <EvidenceRow label="검증 레벨" value={inspectedScope.latestVerifiedLevel === null ? "unknown" : `L${inspectedScope.latestVerifiedLevel}`} mono />
                </dl>
              </section>
            )}
            {(selected.rpoViolation || selected.verificationOverdue) && (
              <section className="inspector-section">
                <h3>Attention reason</h3>
                <ul className="reason-list">
                  {selected.rpoViolation && <li>계획 RPO보다 최근 성공 근거가 오래됐습니다.</li>}
                  {selected.verificationOverdue && <li>요구 검증 레벨의 최신 근거가 기한을 넘었습니다.</li>}
                </ul>
              </section>
            )}
          </Inspector>
        ) : (
          <PageState kind="empty" title="Target을 선택하세요." detail="보호 축별 근거와 Plan 진입점을 표시합니다." />
        )}
      </div>
      {editorOpen && selected && (
        <Dialog
          title={`${selected.displayName} Backup Scope`}
          description="전체 데이터베이스 또는 명시 테이블 집합을 저장합니다. 게시된 Plan은 바꾸지 않습니다."
          onClose={() => setEditorOpen(false)}
        >
          <BackupScopeEditor
            target={selected}
            onCancel={() => setEditorOpen(false)}
            onSaved={(result) => {
              setSavedScope(result);
              setEditorOpen(false);
              scopeQuery.refresh();
            }}
          />
        </Dialog>
      )}
    </div>
  );
}
