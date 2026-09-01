import { useMemo, useState, type FormEvent } from "react";
import { Dialog } from "../components/Dialog";
import { PageHeader, FilterField } from "../components/PageHeader";
import { LoadingState, PageState } from "../components/PageState";
import { StatusBadge, type StatusTone } from "../components/StatusBadge";
import { createWorkspace } from "../lib/api";
import { asServiceError, type ServiceError } from "../lib/errors";
import { formatCount, formatDateTime, formatFreshness } from "../lib/format";
import { useMutationAttemptRegistry } from "../lib/mutationAttempts";
import type { Workspace } from "../lib/types";

type WorkspaceSort = "attention" | "name" | "activity";
type WorkspaceStateKey = "running" | "needs_setup" | "configured" | "archived";

interface WorkspaceOperationalState {
  key: WorkspaceStateKey;
  label: string;
  detail: string;
  tone: StatusTone;
}

interface WorkspacesProps {
  workspaces: Workspace[];
  status: "loading" | "ready" | "error";
  error: ServiceError | null;
  fetchedAt: number | null;
  onRefresh: () => void;
  onOpen: (workspaceId: string) => void;
  onCreated: (workspace: Workspace) => void;
}

export function Workspaces({
  workspaces,
  status,
  error,
  fetchedAt,
  onRefresh,
  onOpen,
  onCreated,
}: WorkspacesProps) {
  const [query, setQuery] = useState("");
  const [sort, setSort] = useState<WorkspaceSort>("attention");
  const [creating, setCreating] = useState(false);

  const activeWorkspaces = workspaces.filter((workspace) => workspace.state === "active");
  const targetCount = activeWorkspaces.reduce(
    (total, workspace) => total + workspace.databaseTargetCount,
    0,
  );
  const activeWorkCount = activeWorkspaces.reduce(
    (total, workspace) => total + workspace.activeWorkCount,
    0,
  );
  const attentionCount = activeWorkspaces.filter(
    (workspace) => workspaceOperationalState(workspace).key === "needs_setup",
  ).length;
  const visibleWorkspaces = useMemo(() => {
    const normalized = query.trim().toLocaleLowerCase("ko-KR");
    return workspaces
      .filter(
        (workspace) =>
          !normalized ||
          workspace.displayName.toLocaleLowerCase("ko-KR").includes(normalized),
      )
      .sort((left, right) => compareWorkspaces(left, right, sort));
  }, [query, sort, workspaces]);

  if (status === "loading" && workspaces.length === 0) {
    return <LoadingState label="Workspace 운영 현황을 불러오는 중입니다." />;
  }
  if (status === "error" && workspaces.length === 0) {
    return (
      <PageState
        kind="error"
        title="Workspace 현황을 불러오지 못했습니다."
        detail="서비스 연결을 복구한 뒤 목록을 다시 조회하세요."
        error={error}
        action={
          <button type="button" className="button button-secondary" onClick={onRefresh}>
            다시 조회
          </button>
        }
      />
    );
  }

  return (
    <div className="page-stack workspace-fleet">
      <PageHeader
        eyebrow="Workspace fleet"
        title="모든 백업 경계를 한눈에 봅니다"
        description="설정이 덜 된 곳과 실행 중 작업을 먼저 확인하고, 관리할 Workspace로 들어갑니다."
        meta={
          <span className="mono">
            {fetchedAt ? `조회 ${formatFreshness(fetchedAt)}` : "조회 시각 없음"}
          </span>
        }
        actions={
          <>
            <button type="button" className="button button-secondary" onClick={onRefresh}>
              현황 새로고침
            </button>
            <button type="button" className="button button-primary" onClick={() => setCreating(true)}>
              새 Workspace
            </button>
          </>
        }
      />

      <section className="fleet-kpis" aria-label="Workspace summary">
        <FleetKpi label="활성 Workspace" value={activeWorkspaces.length} note={`전체 ${workspaces.length}`} />
        <FleetKpi label="관리 대상 DB" value={targetCount} note="Workspace 합계" />
        <FleetKpi label="처리 중 작업" value={activeWorkCount} note={activeWorkCount ? "실행 상태 확인 필요" : "대기열 안정"} accent={activeWorkCount > 0} />
        <FleetKpi label="설정 필요" value={attentionCount} note={attentionCount ? "보호 경계 미완성" : "구성 누락 없음"} attention={attentionCount > 0} />
      </section>

      {status === "error" && (
        <PageState
          kind="stale"
          title="마지막 조회값을 표시하고 있습니다."
          detail="새 작업을 판단하기 전에 서비스 연결을 복구하고 현황을 갱신하세요."
          error={error}
          action={
            <button type="button" className="button button-secondary" onClick={onRefresh}>
              다시 조회
            </button>
          }
        />
      )}

      <section className="fleet-register" aria-labelledby="workspace-register-title">
        <div className="fleet-register-head">
          <div>
            <span className="eyebrow">Operational boundaries</span>
            <h2 id="workspace-register-title">Workspace 현황</h2>
            <p>{formatCount(visibleWorkspaces.length)}개 경계를 비교합니다.</p>
          </div>
          <div className="fleet-controls" role="search">
            <FilterField label="Workspace 검색">
              <input
                type="search"
                value={query}
                placeholder="이름으로 찾기"
                onChange={(event) => setQuery(event.target.value)}
              />
            </FilterField>
            <FilterField label="정렬">
              <select value={sort} onChange={(event) => setSort(event.target.value as WorkspaceSort)}>
                <option value="attention">확인 필요 우선</option>
                <option value="activity">최근 활동순</option>
                <option value="name">이름순</option>
              </select>
            </FilterField>
          </div>
        </div>

        <div className="table-scroll fleet-table-wrap" data-empty={visibleWorkspaces.length === 0}>
          <table className="fleet-table" aria-label="Workspace protection overview">
            <caption className="sr-only">Workspace별 구성 및 실행 현황</caption>
            <thead>
              <tr>
                <th scope="col">Workspace</th>
                <th scope="col">현재 상태</th>
                <th scope="col" className="numeric">DB</th>
                <th scope="col" className="numeric">Plan</th>
                <th scope="col" className="numeric">목적지</th>
                <th scope="col" className="numeric">처리 중</th>
                <th scope="col">최근 활동</th>
                <th scope="col" aria-label="열기" />
              </tr>
            </thead>
            <tbody>
              {visibleWorkspaces.map((workspace) => {
                const operationalState = workspaceOperationalState(workspace);
                return (
                  <tr key={workspace.id} data-state={operationalState.key}>
                    <td>
                      <strong>{workspace.displayName}</strong>
                      <span className="workspace-row-meta mono">
                        {workspace.isDefault ? "DEFAULT" : workspace.id.slice(0, 8)}
                        {` · ${workspace.connectionCount} connections`}
                      </span>
                    </td>
                    <td>
                      <StatusBadge
                        label={operationalState.label}
                        tone={operationalState.tone}
                        reason={operationalState.detail}
                      />
                    </td>
                    <td className="numeric mono">{formatCount(workspace.databaseTargetCount)}</td>
                    <td className="numeric mono">{formatCount(workspace.planCount)}</td>
                    <td className="numeric mono">{formatCount(workspace.destinationCount)}</td>
                    <td className="numeric mono">{formatCount(workspace.activeWorkCount)}</td>
                    <td>
                      <span className="fleet-activity">
                        {workspace.latestActivityAtMs
                          ? formatFreshness(workspace.latestActivityAtMs)
                          : "활동 없음"}
                      </span>
                      <span className="workspace-row-meta mono">
                        {formatDateTime(workspace.latestActivityAtMs)}
                      </span>
                    </td>
                    <td className="fleet-open-cell">
                      <button
                        type="button"
                        className="workspace-open"
                        aria-label={`${workspace.displayName} 열기`}
                        onClick={() => onOpen(workspace.id)}
                      >
                        Open
                      </button>
                    </td>
                  </tr>
                );
              })}
              {visibleWorkspaces.length === 0 && (
                <tr>
                  <td colSpan={8} className="table-empty">검색 조건에 맞는 Workspace가 없습니다.</td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
        <p className="fleet-evidence-note">
          상태는 구성 완성도와 실행 중 작업만 나타냅니다. 백업 성공·RPO·검증 상태는 Workspace에 들어가 근거별로 확인합니다.
        </p>
      </section>

      {creating && (
        <WorkspaceCreateDialog
          onClose={() => setCreating(false)}
          onCreated={onCreated}
        />
      )}
    </div>
  );
}

function FleetKpi({
  label,
  value,
  note,
  accent = false,
  attention = false,
}: {
  label: string;
  value: number;
  note: string;
  accent?: boolean;
  attention?: boolean;
}) {
  return (
    <div data-accent={accent || undefined} data-attention={attention || undefined}>
      <span>{label}</span>
      <strong className="mono">{formatCount(value)}</strong>
      <small>{note}</small>
    </div>
  );
}

function WorkspaceCreateDialog({
  onClose,
  onCreated,
}: {
  onClose: () => void;
  onCreated: (workspace: Workspace) => void;
}) {
  const mutationAttempts = useMutationAttemptRegistry();
  const [displayName, setDisplayName] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<ServiceError | null>(null);

  async function submit(event: FormEvent<HTMLFormElement>): Promise<void> {
    event.preventDefault();
    const name = displayName.trim();
    if (!name || submitting) return;
    setSubmitting(true);
    setError(null);
    try {
      const workspace = await mutationAttempts.run(
        "workspace-create",
        "workspace.create",
        { displayName: name },
        (key) => createWorkspace(name, key),
      );
      onCreated(workspace);
      onClose();
    } catch (failure) {
      setError(asServiceError(failure));
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <Dialog
      title="새 Workspace"
      description="연결, 목적지, Plan과 실행 기록을 분리하는 운영 경계를 만듭니다."
      onClose={onClose}
    >
      <form className="workspace-dialog-form" onSubmit={submit}>
        <label htmlFor="workspace-name">
          <span>Workspace name</span>
          <input
            id="workspace-name"
            value={displayName}
            maxLength={96}
            autoFocus
            placeholder="예: 고객 운영 DB"
            onChange={(event) => setDisplayName(event.target.value)}
          />
        </label>
        <p>생성 후 해당 Workspace의 Overview로 이동합니다.</p>
        {error && (
          <p role="alert" className="form-error">
            {error.message} {error.action}
            <span className="mono error-code"> {error.code}</span>
          </p>
        )}
        <div className="dialog-actions">
          <button type="button" className="button button-secondary" onClick={onClose}>취소</button>
          <button type="submit" className="button button-primary" disabled={!displayName.trim() || submitting}>
            {submitting ? "Creating" : "Create"}
          </button>
        </div>
      </form>
    </Dialog>
  );
}

export function workspaceOperationalState(workspace: Workspace): WorkspaceOperationalState {
  if (workspace.state === "archived") {
    return { key: "archived", label: "보관됨", detail: "운영 대상에서 제외됐습니다.", tone: "neutral" };
  }
  if (workspace.activeWorkCount > 0) {
    return {
      key: "running",
      label: "실행 중",
      detail: `${formatCount(workspace.activeWorkCount)}개 작업이 처리 중입니다.`,
      tone: "info",
    };
  }
  if (workspace.databaseTargetCount === 0) {
    return { key: "needs_setup", label: "DB 등록 필요", detail: "보호 대상 DB가 없습니다.", tone: "warning" };
  }
  if (workspace.destinationCount === 0) {
    return { key: "needs_setup", label: "목적지 필요", detail: "백업을 게시할 목적지가 없습니다.", tone: "warning" };
  }
  if (workspace.planCount === 0) {
    return { key: "needs_setup", label: "Plan 필요", detail: "실행할 Backup Plan이 없습니다.", tone: "warning" };
  }
  return {
    key: "configured",
    label: "구성됨",
    detail: "필수 보호 경계가 연결됐습니다. 백업 건강 상태를 의미하지 않습니다.",
    tone: "neutral",
  };
}

function compareWorkspaces(left: Workspace, right: Workspace, sort: WorkspaceSort): number {
  if (sort === "name") return left.displayName.localeCompare(right.displayName, "ko-KR");
  if (sort === "activity") {
    return (right.latestActivityAtMs ?? -1) - (left.latestActivityAtMs ?? -1)
      || left.displayName.localeCompare(right.displayName, "ko-KR");
  }
  const rank: Record<WorkspaceStateKey, number> = {
    needs_setup: 0,
    running: 1,
    configured: 2,
    archived: 3,
  };
  return rank[workspaceOperationalState(left).key] - rank[workspaceOperationalState(right).key]
    || left.displayName.localeCompare(right.displayName, "ko-KR");
}
