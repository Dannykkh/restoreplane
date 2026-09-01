import { useEffect, useMemo, useState } from "react";
import { ActivityTimeline } from "../components/ActivityTimeline";
import { EvidenceRow, Inspector } from "../components/Inspector";
import { LoadingState, PageState } from "../components/PageState";
import { FilterField, PageHeader } from "../components/PageHeader";
import { StatusBadge, toneForState } from "../components/StatusBadge";
import { VirtualActivityList } from "../components/VirtualActivityList";
import {
  listActivity,
  runtimeEvidenceMode,
  subscribeActivity,
} from "../lib/api";
import { formatCount, formatDateTime, formatFreshness } from "../lib/format";
import { useServiceQuery } from "../lib/queries";
import { routeFor, transitionTo } from "../lib/route";
import type {
  ActivityEvent,
  CursorPage,
  RouteState,
} from "../lib/types";

const SEVERITIES = [
  "debug",
  "info",
  "warning",
  "error",
  "critical",
] as const;

type LiveState = "fixture" | "connecting" | "connected" | "disconnected";

export function Activity({ route }: { route: RouteState }) {
  const severity = parseSeverity(route.filter);
  const query = useServiceQuery<CursorPage<ActivityEvent>>(
    `activity:${route.cursor ?? "first"}:${severity ?? "all"}`,
    () =>
      listActivity({
        cursor: route.cursor ?? null,
        limit: 50,
        severity,
      }),
  );
  const [liveEvents, setLiveEvents] = useState<ActivityEvent[]>([]);
  const [liveState, setLiveState] = useState<LiveState>(
    runtimeEvidenceMode === "browser_fixture" ? "fixture" : "connecting",
  );

  useEffect(() => {
    setLiveEvents([]);
  }, [route.cursor, severity]);

  useEffect(() => {
    if (runtimeEvidenceMode === "browser_fixture" || route.cursor) return;
    let disposed = false;
    let unlisten: (() => void) | null = null;
    const afterSequence = Math.max(
      0,
      ...(query.data?.items.map((event) => event.seq) ?? []),
    );
    setLiveState("connecting");
    void subscribeActivity(afterSequence, (event) => {
      if (disposed || (severity && event.severity !== severity)) return;
      setLiveState("connected");
      setLiveEvents((current) =>
        [event, ...current.filter((item) => item.id !== event.id)].slice(0, 50),
      );
    })
      .then((cleanup) => {
        if (disposed) {
          cleanup();
          return;
        }
        unlisten = cleanup;
        setLiveState("connected");
      })
      .catch(() => {
        if (!disposed) setLiveState("disconnected");
      });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [query.data, route.cursor, severity]);

  const rows = useMemo(() => {
    const seen = new Set<string>();
    return [...liveEvents, ...(query.data?.items ?? [])]
      .filter((event) => {
        if (seen.has(event.id)) return false;
        seen.add(event.id);
        return true;
      })
      .sort((left, right) => right.seq - left.seq)
      .slice(0, 50);
  }, [liveEvents, query.data]);
  const selected =
    rows.find(
      (event) =>
        event.id === route.selected || event.workItemId === route.selected,
    ) ??
    rows[0] ??
    null;
  const timeline = selected
    ? rows
        .filter(
          (event) =>
            selected.workItemId &&
            event.workItemId === selected.workItemId,
        )
        .sort((left, right) => left.seq - right.seq)
    : [];

  return (
    <div className="page-stack">
      <PageHeader
        eyebrow="Append-only operations journal"
        title="Activity"
        description="작업 요청, permit 대기, 실행 단계, 재시도와 종료 근거를 시간순으로 추적합니다."
        meta={
          <span>
            {query.data
              ? `${formatCount(query.data.totalCount)} events · 화면에는 최대 50개`
              : "조회 대기"}
          </span>
        }
        actions={
          <button
            type="button"
            className="button button-secondary"
            onClick={query.refresh}
          >
            새로고침
          </button>
        }
      />

      <div className="filter-bar">
        <FilterField label="Severity">
          <select
            name="activitySeverity"
            value={severity ?? "all"}
            onChange={(event) =>
              transitionTo(
                {
                  page: "activity",
                  filter:
                    event.target.value === "all"
                      ? undefined
                      : event.target.value,
                },
                true,
              )
            }
          >
            <option value="all">전체</option>
            {SEVERITIES.map((value) => (
              <option key={value} value={value}>
                {value}
              </option>
            ))}
          </select>
        </FilterField>
        <StatusBadge
          label={
            liveState === "fixture"
              ? "fixture page"
              : liveState === "connected"
                ? "live connected"
                : liveState === "connecting"
                  ? "live connecting"
                  : "live disconnected"
          }
          tone={
            liveState === "connected"
              ? "success"
              : liveState === "disconnected"
                ? "danger"
                : "neutral"
          }
        />
      </div>

      {liveState === "disconnected" && (
        <PageState
          kind="stale"
          title="실시간 Activity 연결이 끊겼습니다."
          detail="마지막 sequence 이후를 다시 조회해 누락 없이 이어갑니다."
          action={
            <button
              type="button"
              className="button button-secondary"
              onClick={query.refresh}
            >
              sequence 재조회
            </button>
          }
        />
      )}
      {query.status === "loading" && !query.data && (
        <LoadingState label="Activity keyset page를 불러오는 중입니다." />
      )}
      {query.status === "error" && !query.data && (
        <PageState
          kind="error"
          title="Activity를 불러오지 못했습니다."
          detail="실행 결과를 추정하지 않습니다. 서비스 연결을 복구한 뒤 다시 조회하세요."
          error={query.error}
          action={
            <button
              type="button"
              className="button button-secondary"
              onClick={query.refresh}
            >
              다시 조회
            </button>
          }
        />
      )}
      {query.status === "error" && query.data && (
        <PageState
          kind="stale"
          title="마지막 Activity page를 표시합니다."
          detail={`최신 sequence를 가져오지 못했습니다. 마지막 조회 ${
            query.fetchedAt ? formatFreshness(query.fetchedAt) : "기록 없음"
          }.`}
          error={query.error}
        />
      )}

      {query.data && (
        <div className="activity-layout">
          <section className="activity-stream" aria-labelledby="activity-stream-title">
            <header className="section-heading">
              <div>
                <span className="eyebrow">Strict keyset page</span>
                <h2 id="activity-stream-title">
                  {route.cursor ? "이전 events" : "최신 events"}
                </h2>
              </div>
              <span className="mono">{rows.length} DOM rows</span>
            </header>
            <VirtualActivityList
              items={rows}
              totalCount={query.data.totalCount}
              selectedId={selected?.id ?? null}
              onSelect={(event) =>
                transitionTo({ ...route, selected: event.id }, true)
              }
            />
            {rows.length === 0 && (
              <PageState
                kind="empty"
                title="조건에 맞는 Activity가 없습니다."
                detail="필터를 바꾸거나 다음 작업이 기록될 때까지 기다리세요."
              />
            )}
            <nav className="pagination" aria-label="Activity pages">
              <button
                type="button"
                className="button button-secondary"
                disabled={!route.cursor}
                onClick={() => window.history.back()}
              >
                더 최신 page
              </button>
              <button
                type="button"
                className="button button-secondary"
                disabled={!query.data.nextCursor}
                onClick={() =>
                  transitionTo({
                    page: "activity",
                    filter: route.filter,
                    cursor: query.data?.nextCursor ?? undefined,
                  })
                }
              >
                더 이전 page
              </button>
            </nav>
          </section>

          {selected ? (
            <Inspector eyebrow="Activity event" title={selected.eventCode}>
              <dl className="evidence-list">
                <EvidenceRow
                  label="Occurred"
                  value={formatDateTime(selected.occurredAtMs)}
                  mono
                />
                <EvidenceRow
                  label="Severity"
                  value={
                    <StatusBadge
                      label={selected.severity}
                      tone={toneForState(selected.severity)}
                    />
                  }
                />
                <EvidenceRow label="Phase" value={selected.phase ?? "없음"} mono />
                <EvidenceRow label="Work Item" value={selected.workItemId ?? "없음"} mono />
                <EvidenceRow label="Attempt" value={selected.attemptId ?? "없음"} mono />
                <EvidenceRow
                  label="Correlation"
                  value={selected.correlationId ?? "없음"}
                  mono
                />
                <EvidenceRow
                  label="Retryability"
                  value={selected.retryability ?? "해당 없음"}
                />
              </dl>
              <section className="inspector-section">
                <h3>운영 메시지</h3>
                <p>{selected.message}</p>
                {selected.eventCode === "WORK_WAITING_FOR_PERMIT" && (
                  <p className="wait-reason">
                    동시성 permit이 반환될 때 재평가됩니다. 무제한 thread를
                    생성하는 상태가 아닙니다.
                  </p>
                )}
              </section>
              {timeline.length > 1 && (
                <section className="inspector-section">
                  <h3>현재 page의 Work Item 흐름</h3>
                  <ActivityTimeline events={timeline} />
                </section>
              )}
              <section className="related-links" aria-label="Related resources">
                <a
                  href={routeFor(
                    selected.entityType === "destination"
                      ? "destinations"
                      : "databases",
                    { selected: selected.entityId },
                  )}
                >
                  관련 {selected.entityType} 보기
                </a>
              </section>
            </Inspector>
          ) : (
            <PageState
              kind="empty"
              title="Event를 선택하세요."
              detail="Work Item, attempt와 correlation 근거를 표시합니다."
            />
          )}
        </div>
      )}
    </div>
  );
}

function parseSeverity(value: string | undefined): ActivityEvent["severity"] | null {
  return SEVERITIES.includes(value as ActivityEvent["severity"])
    ? (value as ActivityEvent["severity"])
    : null;
}
