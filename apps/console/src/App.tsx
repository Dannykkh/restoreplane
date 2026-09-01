import { useEffect, useMemo, useState } from "react";
import "./App.css";
import { StatusBadge, toneForState } from "./components/StatusBadge";
import {
  activeWorkspaceId,
  getServiceHealth,
  listWorkspaces,
  runtimeEvidenceMode,
  setActiveWorkspaceId,
} from "./lib/api";
import { useServiceQuery } from "./lib/queries";
import {
  NAVIGATION,
  parseRoute,
  serializeRoute,
  transitionTo,
} from "./lib/route";
import type { ServiceError } from "./lib/errors";
import type { PageId, RouteState, Workspace } from "./lib/types";
import { Activity } from "./screens/Activity";
import { BackupPlans } from "./screens/BackupPlans";
import { Connections } from "./screens/Connections";
import { Databases } from "./screens/Databases";
import { Destinations } from "./screens/Destinations";
import { Overview } from "./screens/Overview";
import { Recovery } from "./screens/Recovery";
import { Settings } from "./screens/Settings";
import { Snapshots } from "./screens/Snapshots";
import { Workspaces } from "./screens/Workspaces";

type Theme = "dark" | "light";

export function App() {
  const [route, setRoute] = useState<RouteState>(() =>
    parseRoute(window.location.hash),
  );
  const [theme, setTheme] = useState<Theme>(initialTheme);
  const [workspaceId, setWorkspaceId] = useState(activeWorkspaceId);
  const [createdWorkspaces, setCreatedWorkspaces] = useState<Workspace[]>([]);
  const health = useServiceQuery("shell-service-health", getServiceHealth);
  const workspaces = useServiceQuery("shell-workspaces", listWorkspaces);
  const knownWorkspaces = useMemo(() => {
    const byId = new Map(createdWorkspaces.map((workspace) => [workspace.id, workspace]));
    for (const workspace of workspaces.data ?? []) byId.set(workspace.id, workspace);
    return Array.from(byId.values());
  }, [createdWorkspaces, workspaces.data]);

  const openWorkspace = (nextId: string) => {
    setActiveWorkspaceId(nextId);
    setWorkspaceId(nextId);
    transitionTo({ page: "overview" });
  };

  const addWorkspace = (workspace: Workspace) => {
    setCreatedWorkspaces((current) => [...current, workspace]);
    setActiveWorkspaceId(workspace.id);
    setWorkspaceId(workspace.id);
    workspaces.refresh();
    transitionTo({ page: "overview" });
  };

  useEffect(() => {
    if (!knownWorkspaces.length) return;
    const selected = knownWorkspaces.find(
      (workspace) => workspace.id === workspaceId && workspace.state === "active",
    );
    if (selected) return;
    const fallback =
      knownWorkspaces.find((workspace) => workspace.isDefault) ?? knownWorkspaces[0];
    setActiveWorkspaceId(fallback.id);
    setWorkspaceId(fallback.id);
  }, [knownWorkspaces, workspaceId]);

  useEffect(() => {
    const syncRoute = () => setRoute(parseRoute(window.location.hash));
    window.addEventListener("hashchange", syncRoute);
    window.addEventListener("popstate", syncRoute);

    const canonical = serializeRoute(parseRoute(window.location.hash));
    if (window.location.hash !== canonical) {
      transitionTo(parseRoute(window.location.hash), true);
    }

    return () => {
      window.removeEventListener("hashchange", syncRoute);
      window.removeEventListener("popstate", syncRoute);
    };
  }, []);

  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    document.documentElement.style.colorScheme = theme;
    window.localStorage.setItem("dbbackup.theme", theme);
  }, [theme]);

  return (
    <div className="app-shell">
      <a className="skip-link" href="#main-content">
        본문으로 건너뛰기
      </a>
      <aside className="app-rail">
        <a className="brand" href="#/workspaces" aria-label="All Workspaces dashboard">
          <span className="brand-mark" aria-hidden="true">
            DB
          </span>
          <div>
            <strong>DB Backup</strong>
            <span>CONTROL PLANE</span>
          </div>
        </a>

        <WorkspaceSwitcher
          workspaces={knownWorkspaces}
          status={workspaces.status}
          selectedId={workspaceId}
          isFleet={route.page === "workspaces"}
          onSelect={openWorkspace}
        />

        <nav className="primary-nav" aria-label="Primary">
          {NAVIGATION.map((item) => (
            <a
              key={item.id}
              href={serializeRoute({ page: item.id })}
              aria-current={route.page === item.id ? "page" : undefined}
              data-short-label={item.shortLabel}
            >
              <span className="nav-index" aria-hidden="true">
                {item.shortLabel}
              </span>
              <span>{item.label}</span>
            </a>
          ))}
        </nav>

        <footer className="rail-footer">
          <span className="mono">v0.1.0-rc.1</span>
          <span>Service-owned execution</span>
        </footer>
      </aside>

      <div className="app-stage">
        <header className="topbar">
          <div className="topbar-context">
            <span className="eyebrow">
              {route.page === "workspaces"
                ? "Connected Backup Platform"
                : knownWorkspaces.find((workspace) => workspace.id === workspaceId)
                    ?.displayName ?? "Connected Backup Platform"}
            </span>
            <strong>{pageLabel(route.page)}</strong>
          </div>
          <div className="topbar-actions">
            {health.data && (
              <StatusBadge
                label={health.data.phase}
                tone={toneForState(health.data.phase)}
                reason={`${health.data.executionAuthority} · claims ${health.data.claimsEnabled ? "enabled" : "disabled"}`}
              />
            )}
            <button
              type="button"
              className="theme-toggle"
              aria-label={
                theme === "dark"
                  ? "라이트 테마로 전환"
                  : "다크 테마로 전환"
              }
              onClick={() =>
                setTheme((current) => (current === "dark" ? "light" : "dark"))
              }
            >
              <span aria-hidden="true">{theme === "dark" ? "LIGHT" : "DARK"}</span>
            </button>
          </div>
        </header>

        <RuntimeBanner
          status={health.status}
          phase={health.data?.phase ?? null}
          claimsEnabled={health.data?.claimsEnabled ?? false}
          errorCode={health.error?.code ?? null}
          onRetry={health.refresh}
        />

        <main id="main-content" className="app-main" tabIndex={-1}>
          <div className="page-frame" key={`${workspaceId}:${route.page}`}>
            <Page
              route={route}
              workspaces={knownWorkspaces}
              workspaceStatus={workspaces.status}
              workspaceError={workspaces.error}
              workspaceFetchedAt={workspaces.fetchedAt}
              onRefreshWorkspaces={workspaces.refresh}
              onOpenWorkspace={openWorkspace}
              onCreatedWorkspace={addWorkspace}
            />
          </div>
        </main>
      </div>
    </div>
  );
}

function WorkspaceSwitcher({
  workspaces,
  status,
  selectedId,
  isFleet,
  onSelect,
}: {
  workspaces: Workspace[];
  status: "loading" | "ready" | "error";
  selectedId: string;
  isFleet: boolean;
  onSelect: (workspaceId: string) => void;
}) {
  return (
    <section className="workspace-switcher" aria-label="Workspace context">
      <div className="workspace-switcher-head">
        <label htmlFor="workspace-select">Workspace</label>
        <a
          className="workspace-all"
          href="#/workspaces"
          aria-current={isFleet ? "page" : undefined}
        >
          All
        </a>
      </div>
      <select
        id="workspace-select"
        value={selectedId}
        disabled={status !== "ready" || workspaces.length === 0}
        onChange={(event) => onSelect(event.target.value)}
      >
        {workspaces.length === 0 ? (
          <option value={selectedId}>
            {status === "error" ? "Workspace unavailable" : "Loading workspaces"}
          </option>
        ) : (
          workspaces
            .filter((workspace) => workspace.state === "active")
            .map((workspace) => (
              <option key={workspace.id} value={workspace.id}>
                {workspace.displayName}
              </option>
            ))
        )}
      </select>
    </section>
  );
}

function Page({
  route,
  workspaces,
  workspaceStatus,
  workspaceError,
  workspaceFetchedAt,
  onRefreshWorkspaces,
  onOpenWorkspace,
  onCreatedWorkspace,
}: {
  route: RouteState;
  workspaces: Workspace[];
  workspaceStatus: "loading" | "ready" | "error";
  workspaceError: ServiceError | null;
  workspaceFetchedAt: number | null;
  onRefreshWorkspaces: () => void;
  onOpenWorkspace: (workspaceId: string) => void;
  onCreatedWorkspace: (workspace: Workspace) => void;
}) {
  switch (route.page) {
    case "workspaces":
      return (
        <Workspaces
          workspaces={workspaces}
          status={workspaceStatus}
          error={workspaceError}
          fetchedAt={workspaceFetchedAt}
          onRefresh={onRefreshWorkspaces}
          onOpen={onOpenWorkspace}
          onCreated={onCreatedWorkspace}
        />
      );
    case "overview":
      return <Overview />;
    case "connections":
      return <Connections route={route} />;
    case "databases":
      return <Databases route={route} />;
    case "destinations":
      return <Destinations route={route} />;
    case "backup-plans":
      return <BackupPlans route={route} />;
    case "activity":
      return <Activity route={route} />;
    case "snapshots":
      return <Snapshots route={route} />;
    case "recovery":
      return <Recovery route={route} />;
    case "settings":
      return <Settings />;
  }
}

function RuntimeBanner({
  status,
  phase,
  claimsEnabled,
  errorCode,
  onRetry,
}: {
  status: "loading" | "ready" | "error";
  phase: string | null;
  claimsEnabled: boolean;
  errorCode: string | null;
  onRetry: () => void;
}) {
  if (runtimeEvidenceMode === "browser_fixture") {
    return (
      <div className="runtime-banner runtime-fixture" role="status">
        <strong>브라우저 검증 데이터</strong>
        <span>
          레이아웃과 상호작용 검증용 fixture입니다. 서비스 실행·백업 성공
          증거가 아닙니다.
        </span>
      </div>
    );
  }
  if (status === "loading") {
    return (
      <div className="runtime-banner" role="status">
        <strong>서비스 연결 확인 중</strong>
        <span>이전 조회값이 있어도 새 작업은 상태 확인 후 실행됩니다.</span>
      </div>
    );
  }
  if (status === "error") {
    return (
      <div className="runtime-banner runtime-error" role="alert">
        <strong>서비스 연결 끊김</strong>
        <span>
          새 작업을 실행할 수 없습니다.
          {errorCode ? ` 오류 코드 ${errorCode}.` : ""}
        </span>
        <button type="button" className="button button-secondary" onClick={onRetry}>
          다시 연결
        </button>
      </div>
    );
  }
  if (phase !== "production_ready" || !claimsEnabled) {
    return (
      <div className="runtime-banner runtime-warning" role="status">
        <strong>조회 전용 운영 상태</strong>
        <span>
          현재 단계는 {phase ?? "unknown"}이며 서비스 work claim이 비활성입니다.
          이 상태를 정상 보호로 계산하지 않습니다.
        </span>
      </div>
    );
  }
  return null;
}

function initialTheme(): Theme {
  const saved = window.localStorage.getItem("dbbackup.theme");
  if (saved === "light" || saved === "dark") return saved;
  return window.matchMedia?.("(prefers-color-scheme: light)").matches
    ? "light"
    : "dark";
}

function pageLabel(page: PageId): string {
  if (page === "workspaces") return "Workspace Dashboard";
  return NAVIGATION.find((item) => item.id === page)?.label ?? "Overview";
}
