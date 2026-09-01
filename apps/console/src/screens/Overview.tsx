import { useMemo } from "react";
import {
  activeWorkspaceId,
  getServiceHealth,
  listActivity,
  listBackupScopes,
  listConnections,
  listDatabaseTargets,
  listDestinations,
  listPlans,
  listWorkspaces,
  readSettings,
} from "../lib/api";
import { formatBytes, formatCount, formatDateTime, formatFreshness } from "../lib/format";
import {
  collectCursorPages,
  INVENTORY_PAGE_LIMIT,
  type CollectedCursorPage,
} from "../lib/pagination";
import { useServiceQuery } from "../lib/queries";
import { routeFor } from "../lib/route";
import type {
  ActivityEvent,
  BackupPlan,
  BackupScope,
  ConnectionProfile,
  CursorPage,
  DatabaseTarget,
  Destination,
  ServiceHealth,
  SettingsData,
  Workspace,
} from "../lib/types";
import { InventoryTable } from "../components/InventoryTable";
import { LoadingState, PageState } from "../components/PageState";
import { PageHeader } from "../components/PageHeader";
import { StatusBadge, toneForState } from "../components/StatusBadge";
import { ScopeCoverage } from "../components/ScopeCoverage";

interface OverviewData {
  health: ServiceHealth;
  connections: CollectedCursorPage<ConnectionProfile>;
  targets: CollectedCursorPage<DatabaseTarget>;
  destinations: CollectedCursorPage<Destination>;
  plans: CollectedCursorPage<BackupPlan>;
  failures: CursorPage<ActivityEvent>;
}

interface RuntimeOverviewData {
  scopes: CollectedCursorPage<BackupScope>;
  settings: SettingsData;
  workspaces: Workspace[];
}

export function Overview() {
  const query = useServiceQuery<OverviewData>("overview", async () => {
    const [health, connections, targets, destinations, plans, failures] = await Promise.all([
      getServiceHealth(),
      collectCursorPages((cursor) =>
        listConnections({ cursor, limit: INVENTORY_PAGE_LIMIT })
      ),
      collectCursorPages((cursor) =>
        listDatabaseTargets({ cursor, limit: INVENTORY_PAGE_LIMIT })
      ),
      collectCursorPages((cursor) =>
        listDestinations({ cursor, limit: INVENTORY_PAGE_LIMIT })
      ),
      collectCursorPages((cursor) =>
        listPlans({ cursor, limit: INVENTORY_PAGE_LIMIT })
      ),
      listActivity({ cursor: null, limit: 8, severity: "error" }),
    ]);
    return { health, connections, targets, destinations, plans, failures };
  });
  const runtimeQuery = useServiceQuery<RuntimeOverviewData>("overview-runtime", async () => {
    const [scopes, settings, workspaces] = await Promise.all([
      collectCursorPages((cursor) =>
        listBackupScopes({ cursor, limit: INVENTORY_PAGE_LIMIT })
      ),
      readSettings(),
      listWorkspaces(),
    ]);
    return { scopes, settings, workspaces };
  });

  const targets = useMemo(() => query.data?.targets.items ?? [], [query.data]);

  if (query.status === "loading" && !query.data) return <LoadingState label="보호 상태 근거를 불러오는 중입니다." />;
  if (query.status === "error" && !query.data) {
    return (
      <PageState
        kind="error"
        title="Overview를 불러오지 못했습니다."
        detail="서비스 연결이 복구되기 전에는 보호 상태를 추정하지 않습니다."
        error={query.error}
        action={<button type="button" className="button button-secondary" onClick={query.refresh}>다시 연결</button>}
      />
    );
  }
  if (!query.data) return null;

  const { health, connections, destinations, plans, failures } = query.data;
  const protectionInventoryComplete = query.status === "ready" &&
    connections.complete &&
    query.data.targets.complete &&
    destinations.complete &&
    plans.complete;
  const compliant = targets.filter((item) => item.protectionCompliance === "compliant").length;
  const unprotected = targets.filter((item) => item.protectionScope === "none").length;
  const partial = targets.filter((item) => item.protectionScope === "partial").length;
  const nonCompliant = targets.filter((item) =>
    ["non_compliant", "unknown"].includes(item.protectionCompliance),
  ).length;
  const rpoViolations = targets.filter((item) => item.rpoViolation).length;
  const overdue = targets.filter((item) => item.verificationOverdue).length;
  const unknownVerification = targets.filter((item) => item.verificationState === "unknown").length;
  const destinationFailures = destinations.items.filter((item) => item.lastProbeStatus === "failed").length;
  const attentionCount = unprotected + partial + nonCompliant + rpoViolations + overdue
    + unknownVerification + destinationFailures;
  const queueReady = protectionInventoryComplete &&
    health.phase === "production_ready" && health.claimsEnabled;
  const scopes = runtimeQuery.data?.scopes.items ?? [];
  const runtimeInventoryComplete = runtimeQuery.status === "ready" &&
    runtimeQuery.data?.scopes.complete === true &&
    runtimeQuery.data.settings.complete === true;
  const currentWorkspace = runtimeInventoryComplete ? runtimeQuery.data?.workspaces.find(
    (workspace) => workspace.id === activeWorkspaceId(),
  ) : undefined;
  const runtimeSettings = runtimeQuery.data?.settings;
  const settingsComplete = runtimeInventoryComplete && runtimeSettings?.complete === true;
  const globalPool = settingsComplete
    ? runtimeSettings.resourcePools.find(
        (pool) => pool.resourceType === "global_job" && pool.enabled,
      ) ?? runtimeSettings.resourcePools.find((pool) => pool.enabled)
    : undefined;
  const spoolVolume = settingsComplete
    ? runtimeSettings.spoolVolumes
        .slice()
        .sort((left, right) => spoolStateRank(right.state) - spoolStateRank(left.state))[0]
    : undefined;
  const scheduler = health.scheduler;
  const retention = health.retention;

  return (
    <div className="page-stack">
      <PageHeader
        eyebrow="Protection control"
        title="백업 상태를 근거별로 봅니다"
        description="최근 성공 하나가 아니라 RPO, 검증 신선도, Replica 내구성, 서비스 실행 권한을 분리해 표시합니다."
        meta={
          <span>
            마지막 조회 {query.fetchedAt ? formatFreshness(query.fetchedAt) : "기록 없음"}
          </span>
        }
        actions={
          <button type="button" className="button button-secondary" onClick={query.refresh}>
            근거 새로고침
          </button>
        }
      />

      <section className="protection-strip" aria-label="Protection summary">
        <div className="protection-primary">
          <span className="eyebrow">
            {protectionInventoryComplete
              ? "Protected targets"
              : "Observed protected targets"}
          </span>
          <strong>{compliant}<span> / {targets.length}</span></strong>
          <p>
            {!protectionInventoryComplete
              ? "보호 인벤토리 근거가 완전하지 않아 개입 필요 여부를 알 수 없습니다."
              : attentionCount === 0
              ? "현재 조회 근거에서 개입이 필요한 Target이 없습니다."
              : `${formatCount(attentionCount)}개 근거가 운영자 확인을 요구합니다.`}
          </p>
        </div>
        <dl className="protection-facts">
          <div>
            <dt>RPO 위반</dt>
            <dd>{rpoViolations}</dd>
          </div>
          <div>
            <dt>검증 기한 초과</dt>
            <dd>{overdue}</dd>
          </div>
          <div>
            <dt>미보호 DB</dt>
            <dd>{unprotected}</dd>
          </div>
          <div>
            <dt>Partial coverage</dt>
            <dd>{partial}</dd>
          </div>
          <div>
            <dt>Verification unknown</dt>
            <dd>{unknownVerification}</dd>
          </div>
        </dl>
        <div className="protection-authority">
          <StatusBadge
            label={queueReady ? "Service authority" : "실운영 실행 비활성"}
            tone={queueReady ? "success" : "warning"}
            reason={queueReady ? "service claims enabled" : `${health.phase} · ${health.executionAuthority}`}
          />
          <span className="mono">boot {health.bootId}</span>
        </div>
      </section>

      {query.status === "error" && query.data && (
        <PageState
          kind="stale"
          title="마지막 Overview 근거입니다."
          detail="서비스 연결을 복구하기 전에는 보호 상태나 개입 필요 여부를 확정하지 않습니다."
          error={query.error}
          action={<button type="button" className="button button-secondary" onClick={query.refresh}>Overview 다시 조회</button>}
        />
      )}

      {(
        !protectionInventoryComplete ||
        !query.data.targets.complete ||
        !destinations.complete ||
        !plans.complete ||
        runtimeQuery.data?.scopes.complete === false ||
        runtimeQuery.data?.settings.complete === false ||
        runtimeQuery.status === "error"
      ) && (
        <PageState
          kind="partial"
          title="Protection inventory is partial."
          detail="The bounded cursor scan did not reach a verified end. Counts apply only to the rows shown; refresh after the service cursor stabilizes."
        />
      )}

      {!queueReady && (
        <PageState
          kind="stale"
          title="조회는 가능하지만 서비스가 작업을 claim하지 않습니다."
          detail="전환 게이트를 통과하기 전까지 예약과 수동 실행은 legacy authority에 남습니다. 이 상태를 정상 운영으로 표시하지 않습니다."
          action={<a className="button button-secondary" href={routeFor("settings")}>실행 권한 확인</a>}
        />
      )}

      <section
        className="fleet-register"
        aria-labelledby="program-engine-title"
      >
        <div className="fleet-register-head">
          <div>
            <span className="eyebrow">Runtime ownership</span>
            <h2 id="program-engine-title">프로그램 소유 백업 엔진</h2>
            <p>예약, durable queue, native 작업 permit, spool과 보관 정책을 서로 다른 근거로 표시합니다.</p>
          </div>
        </div>
        {runtimeQuery.status === "loading" && !runtimeQuery.data && (
          <LoadingState label="Scheduler, queue, spool, retention 근거를 불러오는 중입니다." />
        )}
        {runtimeQuery.status === "error" && (
          <PageState
            kind={runtimeQuery.data ? "stale" : "partial"}
            title="엔진 운영 근거를 읽지 못했습니다."
            detail="백업 건강 상태와 실행 용량을 추정하지 않습니다. Settings 연결을 복구한 뒤 다시 조회하세요."
            error={runtimeQuery.error}
            action={<button type="button" className="button button-secondary" onClick={runtimeQuery.refresh}>엔진 근거 다시 조회</button>}
          />
        )}
        {runtimeQuery.data && (
          <div className="fleet-kpis">
            <div data-attention={scheduler.state === "attention" || scheduler.state === "partial" || undefined}>
              <span>Scheduler</span>
              <strong className="mono">
                {scheduler.state === "unknown"
                  ? "근거 없음"
                  : `${scheduler.observedCursorCount} / ${scheduler.configuredPlanCount} cursors`}
              </strong>
              <small>
                {scheduler.lastTickAtMs === null
                  ? scheduler.state
                  : `${scheduler.state} · tick ${formatDateTime(scheduler.lastTickAtMs)}`}
                {scheduler.lastErrorCode ? ` · ${scheduler.lastErrorCode}` : ""}
              </small>
            </div>
            <div data-accent={Boolean(globalPool?.inUse)}>
              <span>Queue / permit</span>
              <strong className="mono">
                {globalPool ? `${globalPool.inUse} / ${globalPool.capacity} permits` : "근거 없음"}
              </strong>
              <small>{currentWorkspace ? `${currentWorkspace.activeWorkCount} workspace work items` : "Workspace queue projection 없음"}</small>
            </div>
            <div data-attention={spoolVolume?.state !== "ready" || undefined}>
              <span>Spool</span>
              <strong className="mono">
                {spoolVolume?.freeBytes == null ? "용량 근거 없음" : formatBytes(spoolVolume.freeBytes)}
              </strong>
              <small>{spoolVolume ? `${spoolVolume.state} · ${spoolVolume.rootPath}` : "등록된 spool volume 없음"}</small>
            </div>
            <div data-attention={retention.failedOperationCount > 0 || retention.state === "awaiting_first_scan" || undefined}>
              <span>Retention</span>
              <strong className="mono">
                {retention.state === "unknown"
                  ? "근거 없음"
                  : `${retention.queuedDeleteCount} queued · ${retention.activeTombstoneCount} tombstones`}
              </strong>
              <small>
                {retention.lastScanAtMs === null
                  ? `${retention.configuredPolicyCount} policies · ${retention.state}`
                  : `${retention.configuredPolicyCount} policies · scan ${formatDateTime(retention.lastScanAtMs)}`}
              </small>
            </div>
          </div>
        )}
      </section>

      <section aria-labelledby="scope-coverage-title">
        <header className="section-heading">
          <div>
            <span className="eyebrow">Database → Backup Scope</span>
            <h2 id="scope-coverage-title">Backup Scope coverage</h2>
          </div>
          <a href={routeFor("databases")}>범위 관리</a>
        </header>
        {runtimeQuery.status === "loading" && !runtimeQuery.data ? (
          <LoadingState label="Backup Scope coverage를 불러오는 중입니다." />
        ) : runtimeQuery.status === "error" && !runtimeQuery.data ? (
          <PageState
            kind="partial"
            title="Backup Scope coverage를 표시할 수 없습니다."
            detail="범위 목록이 복구되기 전에는 전체 DB 보호와 선택 테이블 보호를 합산하지 않습니다."
            error={runtimeQuery.error}
          />
        ) : runtimeQuery.status === "ready" && scopes.length === 0 ? (
          <PageState
            kind="empty"
            title="저장된 Backup Scope가 없습니다."
            detail="Databases에서 전체 DB 또는 선택 테이블 범위를 확인한 뒤 Plan에 연결하세요."
          />
        ) : runtimeQuery.data ? (
          <InventoryTable
            ariaLabel="Backup Scope coverage"
            rows={scopes.slice(0, 8)}
            rowKey={(scope) => scope.id}
            selectedId={null}
            onSelect={(scope) => {
              window.location.hash = routeFor("databases", { selected: scope.databaseTargetId }).slice(1);
            }}
            emptyMessage="Backup Scope 근거를 기다리는 중입니다."
            columns={[
              {
                key: "scope",
                header: "Backup Scope",
                render: (scope) => (
                  <span className="resource-name" title={scope.displayName}>
                    <strong>{scope.displayName}</strong>
                    <small>{scope.databaseName} · {scope.engine || "engine unknown"}</small>
                  </span>
                ),
              },
              {
                key: "coverage",
                header: "Coverage",
                render: (scope) => (
                  <ScopeCoverage
                    mode={scope.mode}
                    selectedObjectCount={scope.selectedObjectCount}
                    eligibleObjectCount={scope.eligibleObjectCount}
                    newObjectCount={scope.newObjectCount}
                    missingObjectCount={scope.missingObjectCount}
                    changedObjectCount={scope.changedObjectCount}
                  />
                ),
              },
              {
                key: "policy",
                header: "Plan / Retention",
                className: "mono",
                render: (scope) => `${scope.planCount ?? "?"} / ${scope.retentionPolicyCount ?? "?"}`,
              },
              {
                key: "state",
                header: "State",
                render: (scope) => (
                  <StatusBadge
                    label={scope.driftState}
                    tone={scope.driftState === "drifted" ? "warning" : toneForState(scope.driftState)}
                    evidenceAtMs={scope.inventoryObservedAtMs}
                  />
                ),
              },
              {
                key: "latest",
                header: "Latest backup",
                className: "mono",
                render: (scope) => formatDateTime(scope.latestSuccessAtMs),
              },
            ]}
          />
        ) : null}
      </section>

      <div className="overview-split">
        <section className="attention-panel" aria-labelledby="attention-title">
          <header className="section-heading">
            <div>
              <span className="eyebrow">Attention</span>
              <h2 id="attention-title">지금 개입할 항목</h2>
            </div>
            <a href={routeFor("activity", { filter: "error" })}>Activity 열기</a>
          </header>
          <ul className="attention-list">
            {rpoViolations > 0 && (
              <li>
                <StatusBadge label="RPO 위반" tone="danger" />
                <div><strong>{rpoViolations}개 Target</strong><span>최근 성공 시각이 계획 RPO를 넘었습니다.</span></div>
              </li>
            )}
            {overdue > 0 && (
              <li>
                <StatusBadge label="검증 기한 초과" tone="warning" />
                <div><strong>{overdue}개 Target</strong><span>L2/L3 근거가 정책 기한보다 오래됐습니다.</span></div>
              </li>
            )}
            {unknownVerification > 0 && (
              <li>
                <StatusBadge label="Verification evidence missing" tone="warning" />
                <div>
                  <strong>{unknownVerification} Target</strong>
                  <span>No passed verification evidence is available for the required policy.</span>
                </div>
              </li>
            )}
            {destinationFailures > 0 && (
              <li>
                <StatusBadge label="Reader probe 실패" tone="danger" />
                <div><strong>{destinationFailures}개 Destination</strong><span>백업 성공과 복구 읽기 가능성은 별도 근거입니다.</span></div>
              </li>
            )}
            {!protectionInventoryComplete && (
              <li>
                <StatusBadge label="근거 불완전" tone="warning" />
                <div>
                  <strong>개입 필요 여부 알 수 없음</strong>
                  <span>정확한 인벤토리와 최신 서비스 응답을 다시 확인하세요.</span>
                </div>
              </li>
            )}
            {protectionInventoryComplete && attentionCount === 0 && (
              <li className="attention-empty">조회 범위에서 열린 Attention이 없습니다.</li>
            )}
          </ul>
        </section>

        <section className="queue-panel" aria-labelledby="queue-title">
          <header className="section-heading">
            <div>
              <span className="eyebrow">Execution</span>
              <h2 id="queue-title">Queue와 permit</h2>
            </div>
          </header>
          <dl className="queue-facts">
            <div><dt>서비스 단계</dt><dd><StatusBadge label={health.phase} tone={toneForState(health.phase)} /></dd></div>
            <div><dt>Work claim</dt><dd>{health.claimsEnabled ? "활성" : "비활성"}</dd></div>
            <div><dt>활성 계획</dt><dd>{plans.items.filter((plan) => plan.state === "enabled").length}</dd></div>
            <div><dt>최근 오류 표본</dt><dd>{failures.items.length}</dd></div>
          </dl>
        </section>
      </div>

      <section aria-labelledby="target-health-title">
        <header className="section-heading">
          <div>
            <span className="eyebrow">Database targets</span>
            <h2 id="target-health-title">DB별 보호 현황</h2>
          </div>
          <a href={routeFor("databases")}>전체 Databases</a>
        </header>
        <InventoryTable
          ariaLabel="DB별 보호 현황"
          rows={targets.slice(0, 8)}
          rowKey={(row) => row.id}
          selectedId={null}
          onSelect={(row) => {
            window.location.hash = routeFor("databases", { selected: row.id }).slice(1);
          }}
          emptyMessage="선택된 Database Target이 없습니다."
          columns={[
            {
              key: "database",
              header: "Database",
              render: (row) => (
                <span className="resource-name" title={row.displayName}>
                  <strong>{row.displayName}</strong>
                  <small>{row.connectionName} · {row.engine}</small>
                </span>
              ),
            },
            {
              key: "protection",
              header: "Protection",
              render: (row) => (
                <StatusBadge
                  label={row.protectionCompliance}
                  tone={toneForState(row.protectionCompliance)}
                  evidenceAtMs={row.evidenceAtMs}
                />
              ),
            },
            {
              key: "latest",
              header: "Latest backup",
              className: "mono",
              render: (row) => formatDateTime(row.latestSuccessAtMs),
            },
            {
              key: "durability",
              header: "Replica",
              render: (row) => <StatusBadge label={row.replicaDurability} tone={toneForState(row.replicaDurability)} />,
            },
          ]}
        />
      </section>
    </div>
  );
}

function spoolStateRank(state: SettingsData["spoolVolumes"][number]["state"]): number {
  switch (state) {
    case "offline": return 4;
    case "full": return 3;
    case "throttled": return 2;
    case "ready": return 1;
  }
}
