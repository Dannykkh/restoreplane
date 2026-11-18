import type { PageId, RouteState } from "./types";

export interface NavigationItem {
  id: PageId;
  label: string;
  shortLabel: string;
}

export const NAVIGATION: readonly NavigationItem[] = [
  { id: "overview", label: "Overview", shortLabel: "OV" },
  { id: "connections", label: "Connections", shortLabel: "CN" },
  { id: "databases", label: "Databases", shortLabel: "DB" },
  { id: "workbench", label: "데이터베이스 작업", shortLabel: "WB" },
  { id: "destinations", label: "Destinations", shortLabel: "DS" },
  { id: "backup-plans", label: "Backup Plans", shortLabel: "PL" },
  { id: "activity", label: "Activity", shortLabel: "AC" },
  { id: "snapshots", label: "Snapshots", shortLabel: "SN" },
  { id: "recovery", label: "Recovery", shortLabel: "RC" },
  { id: "settings", label: "Settings", shortLabel: "ST" },
] as const;

const VALID_PAGES = new Set<PageId>(NAVIGATION.map((item) => item.id));
const LEGACY_ROUTES: Record<string, PageId> = {
  dashboard: "overview",
  jobs: "backup-plans",
  schedule: "backup-plans",
  restore: "recovery",
  run: "activity",
};

export function parseRoute(value: string): RouteState {
  const raw = value.startsWith("#") ? value.slice(1) : value;
  const normalized = raw.startsWith("/") ? raw.slice(1) : raw;
  const [path = "", query = ""] = normalized.split("?", 2);
  const requested = path || "workspaces";
  const page =
    requested === "workspaces"
      ? "workspaces"
      : LEGACY_ROUTES[requested] ??
        (VALID_PAGES.has(requested as PageId)
          ? (requested as PageId)
          : "workspaces");
  const params = new URLSearchParams(query);
  return compactRoute({
    page,
    filter: safeParam(params.get("filter")),
    cursor: safeParam(params.get("cursor")),
    selected: safeParam(params.get("selected")),
    target: safeParam(params.get("target")),
    scope: safeParam(params.get("scope")),
    action: safeParam(params.get("action")),
  });
}

export function serializeRoute(route: RouteState): string {
  const params = new URLSearchParams();
  appendParam(params, "filter", route.filter);
  appendParam(params, "cursor", route.cursor);
  appendParam(params, "selected", route.selected);
  appendParam(params, "target", route.target);
  appendParam(params, "scope", route.scope);
  appendParam(params, "action", route.action);
  const query = params.toString();
  return `#/${route.page}${query ? `?${query}` : ""}`;
}

export function routeFor(page: PageId, state?: Omit<RouteState, "page">): string {
  return serializeRoute({ page, ...state });
}

function safeParam(value: string | null): string | undefined {
  if (!value || value.length > 512 || value.includes("\0")) return undefined;
  return value;
}

function appendParam(params: URLSearchParams, key: string, value: string | undefined): void {
  if (value) params.set(key, value);
}

function compactRoute(route: RouteState): RouteState {
  return Object.fromEntries(
    Object.entries(route).filter(([, value]) => value !== undefined),
  ) as unknown as RouteState;
}

export function transitionTo(route: RouteState, replace = false): void {
  const navigate = () => {
    const href = serializeRoute(route);
    if (replace) {
      window.history.replaceState(null, "", href);
      window.dispatchEvent(new HashChangeEvent("hashchange"));
    } else {
      window.location.hash = href.slice(1);
    }
  };
  const documentWithTransition = document as Document & {
    startViewTransition?: (callback: () => void) => void;
  };
  if (
    documentWithTransition.startViewTransition &&
    !window.matchMedia("(prefers-reduced-motion: reduce)").matches
  ) {
    documentWithTransition.startViewTransition(navigate);
  } else {
    navigate();
  }
}
