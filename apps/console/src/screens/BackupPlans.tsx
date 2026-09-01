import { useEffect, useMemo, useState } from "react";
import { BackupPlanWizard } from "../components/BackupPlanWizard";
import { BackupTaskCenter } from "../components/BackupTaskCenter";
import { Dialog } from "../components/Dialog";
import { EvidenceRow, Inspector } from "../components/Inspector";
import { InventoryTable } from "../components/InventoryTable";
import { LoadingState, PageState } from "../components/PageState";
import { FilterField, PageHeader } from "../components/PageHeader";
import { StatusBadge, toneForState } from "../components/StatusBadge";
import {
  cancelWork,
  enqueueBackup,
  getEngineMatrix,
  getServiceHealth,
  getWorkItem,
  listBackupScopes,
  listDatabaseTargets,
  listDestinations,
  listPlans,
  runtimeEvidenceMode,
} from "../lib/api";
import { backupExecutionGate } from "../lib/backupExecution";
import { enrichBackupScopes } from "../lib/backupScopes";
import { cronPreview } from "../lib/cron";
import { asServiceError, type ServiceError } from "../lib/errors";
import { formatDateTime, formatDuration, formatFreshness } from "../lib/format";
import { useMutationAttemptRegistry } from "../lib/mutationAttempts";
import {
  collectCursorPages,
  INVENTORY_PAGE_LIMIT,
  type CollectedCursorPage,
} from "../lib/pagination";
import { useServiceQuery } from "../lib/queries";
import { transitionTo } from "../lib/route";
import type {
  BackupPlan,
  BackupEnqueueResult,
  BackupEnqueueWorkItem,
  BackupScope,
  DatabaseTarget,
  Destination,
  EngineMatrix,
  RouteState,
  ServiceHealth,
  WorkItemStatus,
} from "../lib/types";

interface PlanData {
  plans: CollectedCursorPage<BackupPlan>;
  targets: CollectedCursorPage<DatabaseTarget>;
  destinations: CollectedCursorPage<Destination>;
  engineMatrix: EngineMatrix;
  health: ServiceHealth;
}

type ScopeData = CollectedCursorPage<BackupScope>;

const terminalWorkStates = new Set([
  "succeeded",
  "partially_succeeded",
  "failed",
  "failed_terminal",
  "cancelled",
]);

export function BackupPlans({ route }: { route: RouteState }) {
  const mutationAttempts = useMutationAttemptRegistry();
  const [wizardOpen, setWizardOpen] = useState(route.action === "new");
  const [notice, setNotice] = useState<string | null>(null);
  const [trackedWorks, setTrackedWorks] = useState<WorkItemStatus[]>([]);
  const [runningPlanId, setRunningPlanId] = useState<string | null>(null);
  const [cancellingWorkId, setCancellingWorkId] = useState<string | null>(null);
  const [operationError, setOperationError] = useState<ServiceError | null>(null);
  const query = useServiceQuery<PlanData>("backup-plans", async () => {
    const [plans, targets, destinations, engineMatrix, health] = await Promise.all([
      collectCursorPages((cursor) => listPlans({ cursor, limit: INVENTORY_PAGE_LIMIT })),
      collectCursorPages((cursor) => listDatabaseTargets({ cursor, limit: INVENTORY_PAGE_LIMIT })),
      collectCursorPages((cursor) => listDestinations({ cursor, limit: INVENTORY_PAGE_LIMIT })),
      getEngineMatrix(),
      getServiceHealth(),
    ]);
    return { plans, targets, destinations, engineMatrix, health };
  });
  const scopeQuery = useServiceQuery<ScopeData>("backup-plan-scopes", () =>
    collectCursorPages((cursor) =>
      listBackupScopes({ cursor, limit: INVENTORY_PAGE_LIMIT }),
    ),
  );
  const filter = route.filter ?? "all";
  const rows = useMemo(
    () =>
      (query.data?.plans.items ?? []).filter(
        (plan) => filter === "all" || plan.state === filter,
      ),
    [filter, query.data],
  );
  const selected = rows.find((plan) => plan.id === route.selected) ?? rows[0] ?? null;
  const targets = query.data?.targets.items ?? [];
  const inventoryComplete = query.status === "ready" && scopeQuery.status === "ready" &&
    query.data !== null && query.data.plans.complete === true &&
    query.data.targets.complete === true &&
    query.data.destinations.complete === true &&
    query.data.engineMatrix.complete === true &&
    scopeQuery.data?.complete === true;
  const scopes = useMemo(
    () => enrichBackupScopes(scopeQuery.data?.items ?? [], targets),
    [scopeQuery.data, targets],
  );
  const executionGate = query.status === "ready" && query.data
    ? backupExecutionGate(query.data.health, runtimeEvidenceMode)
    : { allowed: false, reason: "서비스 실행 권한 근거를 불러오는 중입니다." };
  const runReason = selected?.state !== "enabled"
    ? "enabled 상태의 게시된 Plan만 수동 실행할 수 있습니다."
    : executionGate.reason;
  const activeWorkKey = trackedWorks
    .filter((item) => !terminalWorkStates.has(item.state))
    .map((item) => item.workItemId)
    .sort()
    .join("|");

  useEffect(() => {
    if (!activeWorkKey) return undefined;
    let disposed = false;
    const workItemIds = activeWorkKey.split("|");
    const refresh = async () => {
      try {
        const statuses = await Promise.all(workItemIds.map(getWorkItem));
        if (disposed) return;
        setTrackedWorks((current) => mergeTrackedWorks(current, statuses));
      } catch (error: unknown) {
        if (!disposed) setOperationError(asServiceError(error));
      }
    };
    const timer = window.setInterval(() => void refresh(), 2_000);
    return () => {
      disposed = true;
      window.clearInterval(timer);
    };
  }, [activeWorkKey]);

  const handleRunNow = async (plan: BackupPlan) => {
    if (plan.state !== "enabled" || !executionGate.allowed) return;
    setRunningPlanId(plan.id);
    setOperationError(null);
    try {
      const execution = await mutationAttempts.run(
        `backup-enqueue:${plan.id}`,
        "backup.enqueue",
        { planId: plan.id },
        (key) => enqueueBackup(plan.id, key),
      );
      const pendingWorks = execution.workItems.map((work) =>
        pendingBackupWork(work, execution, plan),
      );
      setTrackedWorks((current) => mergeTrackedWorks(current, pendingWorks));
      const statuses = await Promise.all(
        execution.workItems.map(async (work) =>
          withBackupSubject(await getWorkItem(work.workItemId), execution, work, plan),
        ),
      );
      setTrackedWorks((current) => mergeTrackedWorks(current, statuses));
    } catch (error: unknown) {
      setOperationError(asServiceError(error));
    } finally {
      setRunningPlanId(null);
    }
  };

  const handleCancel = async (workItemId: string, fencingToken: number) => {
    setCancellingWorkId(workItemId);
    setOperationError(null);
    try {
      const payload = { workItemId, fencingToken };
      const status = await mutationAttempts.run(
        `work-cancel:${workItemId}`,
        "work.cancel",
        payload,
        (key) => cancelWork(workItemId, fencingToken, key),
      );
      setTrackedWorks((current) => mergeTrackedWorks(current, [status]));
    } catch (error: unknown) {
      setOperationError(asServiceError(error));
    } finally {
      setCancellingWorkId(null);
    }
  };

  if (query.status === "loading" && !query.data) return <LoadingState label="Plan revision 근거를 불러오는 중입니다." />;
  if (query.status === "error" && !query.data) {
    return (
      <PageState
        kind="error"
        title="Backup Plans를 읽지 못했습니다."
        detail="예약 상태와 실행 여부를 추정하지 않습니다."
        error={query.error}
        action={<button type="button" className="button button-secondary" onClick={query.refresh}>다시 연결</button>}
      />
    );
  }

  return (
    <div className="page-stack">
      <PageHeader
        eyebrow="Immutable policy revisions"
        title="Backup Plans"
        description="Target, Destination 역할, schedule/RPO, retention, verification을 immutable revision으로 게시합니다."
        meta={query.fetchedAt && <span>근거 {formatFreshness(query.fetchedAt)}</span>}
        actions={
          <>
            <button
              type="button"
              className="button button-secondary"
              disabled={query.status === "loading"}
              onClick={query.refresh}
            >
              Plan 근거 새로고침
            </button>
            <button
              type="button"
              className="button button-primary"
              disabled={!inventoryComplete}
              aria-describedby="backup-scope-readiness"
              onClick={() => {
                if (!inventoryComplete) return;
                setWizardOpen(true);
                transitionTo({ ...route, action: "new" }, true);
              }}
            >
              새 Backup Plan
            </button>
          </>
        }
      />
      <p id="backup-scope-readiness" className="sr-only">
        {scopeQuery.status === "ready" && scopeQuery.data?.complete === true
          ? "저장된 Backup Scope를 선택할 수 있습니다."
          : scopeQuery.status === "loading"
            ? "Backup Scope 목록을 불러오는 중입니다."
            : "Backup Scope 목록을 읽지 못해 새 Plan을 만들 수 없습니다."}
      </p>
      {scopeQuery.status === "loading" && (
        <LoadingState label="Plan에 연결할 Backup Scope를 불러오는 중입니다." />
      )}
      {scopeQuery.status === "error" && (
        <PageState
          kind="partial"
          title="Backup Scope 목록을 읽지 못했습니다."
          detail="기존 Plan은 조회할 수 있지만 범위를 확인하기 전에는 새 revision을 만들지 않습니다."
          error={scopeQuery.error}
          action={<button type="button" className="button button-secondary" onClick={scopeQuery.refresh}>범위 다시 조회</button>}
        />
      )}
      {(scopeQuery.data?.complete === false || (
        query.data && (
          !query.data.plans.complete ||
          !query.data.targets.complete ||
          !query.data.destinations.complete ||
          !query.data.engineMatrix.complete
        )
      )) && (
        <PageState
          kind="partial"
          title="Backup plan inventory is partial."
          detail="Scope, target, destination, plan, and engine certification inventories must all be exact before plan changes are enabled."
        />
      )}
      {notice && (
        <PageState
          kind="partial"
          title="Plan revision을 게시했습니다."
          detail={`Plan ${notice}을 enabled 상태로 고정했습니다. 실제 실행을 요청한 뒤에는 Activity에서 진행을 추적합니다.`}
          action={<button type="button" className="button button-secondary" onClick={() => transitionTo({ page: "backup-plans", selected: notice })}>Plan 보기</button>}
        />
      )}
      {operationError && (
        <PageState
          kind="error"
          title="백업 작업 요청을 완료하지 못했습니다."
          detail="수락된 작업이 있다면 durable work ID는 아래 작업 센터에 유지됩니다."
          error={operationError}
          action={
            <button
              type="button"
              className="button button-secondary"
              onClick={() => setOperationError(null)}
            >
              닫기
            </button>
          }
        />
      )}
      <BackupTaskCenter
        items={trackedWorks}
        cancellingId={cancellingWorkId}
        onCancel={handleCancel}
      />
      <div className="filter-bar">
        <FilterField label="Plan 상태">
          <select
            name="planState"
            value={filter}
            onChange={(event) =>
              transitionTo(
                {
                  page: "backup-plans",
                  filter: event.target.value === "all" ? undefined : event.target.value,
                },
                true,
              )
            }
          >
            <option value="all">전체</option>
            <option value="enabled">Enabled</option>
            <option value="attention">Attention</option>
            <option value="paused">Paused</option>
            <option value="draft">Draft</option>
          </select>
        </FilterField>
      </div>
      {query.status === "error" && query.data && (
        <PageState
          kind="stale"
          title="마지막 Plan 목록입니다."
          detail="게시 또는 실행 전에 서비스 연결을 복구하세요."
          error={query.error}
          action={<button type="button" className="button button-secondary" onClick={query.refresh}>Plan 다시 조회</button>}
        />
      )}
      <div className="inventory-layout">
        <section className="inventory-main" aria-labelledby="plan-list-title">
          <header className="section-heading">
            <div><span className="eyebrow">Published policies</span><h2 id="plan-list-title">{rows.length} plans</h2></div>
          </header>
          <InventoryTable
            ariaLabel="Backup Plan inventory"
            rows={rows}
            rowKey={(row) => row.id}
            selectedId={selected?.id ?? null}
            onSelect={(row) => transitionTo({ ...route, selected: row.id, action: undefined }, true)}
            emptyMessage="이 필터에 해당하는 Backup Plan이 없습니다."
            columns={[
              {
                key: "plan",
                header: "Backup Plan",
                render: (row) => (
                  <span className="resource-name" title={row.displayName}>
                    <strong>{row.displayName}</strong>
                    <small>revision {row.revisionNo ?? "draft"}</small>
                  </span>
                ),
              },
              { key: "state", header: "State", render: (row) => <StatusBadge label={row.state} tone={toneForState(row.state)} /> },
              {
                key: "schedule",
                header: "Schedule",
                render: (row) => (
                  <span className="schedule-summary">
                    <strong>
                      {row.scheduleExpression
                        ? cronPreview(row.scheduleExpression)
                        : row.scheduleKind ?? "예약 없음"}
                    </strong>
                    <small className="mono">
                      {row.scheduleExpression ?? "manual"} · {row.timezone ?? "timezone 없음"}
                    </small>
                  </span>
                ),
              },
              { key: "rpo", header: "RPO", render: (row) => formatDuration(row.rpoMs) },
              { key: "targets", header: "Targets / Dest.", className: "mono", render: (row) => `${row.targetCount} / ${row.requiredDestinationCount}` },
            ]}
          />
        </section>
        {selected ? (
          <Inspector
            eyebrow="Backup plan"
            title={selected.displayName}
            actions={
              <>
                <button
                  type="button"
                  className="button button-secondary"
                  disabled={!inventoryComplete}
                  onClick={() => {
                    if (!inventoryComplete) return;
                    setWizardOpen(true);
                    transitionTo({ page: "backup-plans", action: "new" }, true);
                  }}
                >
                  새 Revision
                </button>
                <button
                  type="button"
                  className="button button-primary"
                  disabled={
                    selected.state !== "enabled" ||
                    !executionGate.allowed ||
                    runningPlanId === selected.id
                  }
                  aria-describedby="backup-run-authority"
                  title={runReason}
                  onClick={() => void handleRunNow(selected)}
                >
                  {runningPlanId === selected.id ? "요청 중" : "지금 실행"}
                </button>
              </>
            }
          >
            <dl className="evidence-list">
              <EvidenceRow label="State" value={<StatusBadge label={selected.state} tone={toneForState(selected.state)} />} />
              <EvidenceRow label="Revision" value={selected.revisionNo ?? "draft"} mono />
              <EvidenceRow
                label="Schedule"
                value={`${selected.scheduleExpression ? cronPreview(selected.scheduleExpression) : "예약 없음"} · ${selected.scheduleExpression ?? "manual"} · ${selected.timezone ?? "timezone 없음"}`}
                mono
              />
              <EvidenceRow label="RPO" value={formatDuration(selected.rpoMs)} />
              <EvidenceRow label="Targets" value={selected.targetCount} mono />
              <EvidenceRow label="Required replicas" value={selected.requiredDestinationCount} mono />
              <EvidenceRow label="Published" value={formatDateTime(selected.publishedAtMs)} mono />
            </dl>
            <section className="inspector-section">
              <h3>Revision immutability</h3>
              <p className="muted-copy">게시된 revision은 수정하지 않습니다. 정책 변경은 다음 revision을 만들어 전환합니다.</p>
            </section>
            <section className="inspector-section">
              <h3>Run authority</h3>
              <p id="backup-run-authority" className="muted-copy">{runReason}</p>
            </section>
          </Inspector>
        ) : (
          <PageState kind="empty" title="Plan을 선택하세요." detail="현재 immutable revision과 실행 정책을 표시합니다." />
        )}
      </div>
      {wizardOpen && query.data && inventoryComplete && (
        <Dialog
          title="Backup Plan workspace"
          description="Target부터 preflight까지 다섯 단계로 정책을 게시합니다."
          onClose={() => {
            setWizardOpen(false);
            transitionTo({ ...route, action: undefined }, true);
          }}
        >
          <BackupPlanWizard
            targets={targets}
            scopes={scopes}
            destinations={query.data.destinations.items}
            engineMatrix={query.data.engineMatrix}
            preselectedTargetId={route.selected}
            onCancel={() => {
              setWizardOpen(false);
              transitionTo({ ...route, action: undefined }, true);
            }}
            onComplete={(planId) => {
              setNotice(planId);
              setWizardOpen(false);
              transitionTo({ page: "backup-plans" }, true);
              query.refresh();
            }}
          />
        </Dialog>
      )}
    </div>
  );
}

function pendingBackupWork(
  work: BackupEnqueueWorkItem,
  execution: BackupEnqueueResult,
  plan: BackupPlan,
): WorkItemStatus {
  const now = Date.now();
  return {
    workItemId: work.workItemId,
    state: work.state,
    activityAfterSeq: execution.activityAfterSeq,
    kind: "backup",
    currentAttemptNo: 0,
    fencingToken: 0,
    waitReason: "서비스 상태 근거를 조회하는 중입니다.",
    operationRunId: null,
    operationKind: null,
    result: null,
    errorCode: null,
    discoveredCount: 0,
    eligibleDiscoveredCount: 0,
    discovered: [],
    subject: {
      planId: plan.id,
      planName: execution.planName || plan.displayName,
      targetId: work.targetId,
      targetName: work.targetName,
      engine: work.engine,
    },
    progress: {
      phase: "queued",
      completedUnits: null,
      totalUnits: null,
      unit: "bytes",
      currentObject: null,
      updatedAtMs: now,
    },
    createdAtMs: now,
    updatedAtMs: now,
  };
}

function withBackupSubject(
  status: WorkItemStatus,
  execution: BackupEnqueueResult,
  work: BackupEnqueueWorkItem,
  plan: BackupPlan,
): WorkItemStatus {
  if (status.subject) return status;
  return {
    ...status,
    subject: {
      planId: plan.id,
      planName: execution.planName || plan.displayName,
      targetId: work.targetId,
      targetName: work.targetName,
      engine: work.engine,
    },
  };
}

function mergeTrackedWorks(
  current: WorkItemStatus[],
  incoming: readonly WorkItemStatus[],
): WorkItemStatus[] {
  const updates = new Map(incoming.map((item) => [item.workItemId, item]));
  const merged = current.map((item) => {
    const update = updates.get(item.workItemId);
    if (!update) return item;
    updates.delete(item.workItemId);
    if (terminalWorkStates.has(item.state) && !terminalWorkStates.has(update.state)) {
      return item;
    }
    return {
      ...update,
      subject: update.subject ?? item.subject,
      progress: update.progress ?? item.progress,
    };
  });
  return [...updates.values(), ...merged].slice(0, 8);
}
