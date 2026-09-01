import { useMemo, useState } from "react";
import { ConnectionWizard } from "../components/ConnectionWizard";
import { Dialog } from "../components/Dialog";
import { EvidenceRow, Inspector } from "../components/Inspector";
import { InventoryTable } from "../components/InventoryTable";
import { LoadingState, PageState } from "../components/PageState";
import { FilterField, PageHeader } from "../components/PageHeader";
import { StatusBadge, toneForState } from "../components/StatusBadge";
import { listConnections, listDatabaseTargets } from "../lib/api";
import { formatDateTime, formatFreshness } from "../lib/format";
import { useServiceQuery } from "../lib/queries";
import {
  collectCursorPages,
  INVENTORY_PAGE_LIMIT,
  type CollectedCursorPage,
} from "../lib/pagination";
import { transitionTo } from "../lib/route";
import type { ConnectionProfile, DatabaseTarget, RouteState } from "../lib/types";

interface ConnectionsProps {
  route: RouteState;
}

export function Connections({ route }: ConnectionsProps) {
  const [wizardOpen, setWizardOpen] = useState(route.action === "new");
  const query = useServiceQuery<CollectedCursorPage<ConnectionProfile>>(
    "connections",
    () => collectCursorPages((cursor) =>
      listConnections({ cursor, limit: INVENTORY_PAGE_LIMIT })
    ),
  );
  const filter = route.filter ?? "all";
  const rows = useMemo(
    () =>
      (query.data?.items ?? []).filter(
        (connection) => filter === "all" || connection.state === filter,
      ),
    [filter, query.data],
  );
  const selected =
    rows.find((connection) => connection.id === route.selected) ?? rows[0] ?? null;
  const targetQuery = useServiceQuery<CollectedCursorPage<DatabaseTarget>>(
    `connection-targets:${selected?.id ?? "none"}`,
    () => selected
      ? collectCursorPages((cursor) =>
          listDatabaseTargets({
            cursor,
            limit: INVENTORY_PAGE_LIMIT,
            connectionId: selected.id,
          })
        )
      : Promise.resolve({
          items: [],
          nextCursor: null,
          totalCount: 0,
          complete: true,
          pageCount: 0,
        }),
  );
  const selectedTargets = targetQuery.data?.items ?? [];

  function select(connection: ConnectionProfile): void {
    transitionTo({ ...route, selected: connection.id, action: undefined }, true);
  }

  function closeWizard(): void {
    setWizardOpen(false);
    transitionTo({ ...route, action: undefined }, true);
  }

  return (
    <div className="page-stack">
      <PageHeader
        eyebrow="Server inventory"
        title="Connections"
        description="서버 접속 프로필과 그 아래에서 발견·선택된 Database Target을 분리해 관리합니다."
        meta={
          query.fetchedAt && (
            <span>서비스 근거 {formatFreshness(query.fetchedAt)}</span>
          )
        }
        actions={
          <button
            type="button"
            className="button button-primary"
            onClick={() => {
              setWizardOpen(true);
              transitionTo({ ...route, action: "new" }, true);
            }}
          >
            새 Connection
          </button>
        }
      />

      <div className="filter-bar">
        <FilterField label="상태">
          <select
            name="connectionState"
            value={filter}
            onChange={(event) =>
              transitionTo(
                {
                  page: "connections",
                  filter: event.target.value === "all" ? undefined : event.target.value,
                  selected: undefined,
                },
                true,
              )
            }
          >
            <option value="all">전체</option>
            <option value="ready">Ready</option>
            <option value="attention">Attention</option>
            <option value="draft">Draft</option>
            <option value="disabled">Disabled</option>
          </select>
        </FilterField>
        <button type="button" className="button button-quiet" onClick={query.refresh}>
          새로고침
        </button>
      </div>

      {query.status === "loading" && !query.data && <LoadingState />}
      {query.status === "error" && !query.data && (
        <PageState
          kind="error"
          title="Connection inventory를 읽지 못했습니다."
          detail="마지막 probe 상태를 추정하지 않고 서비스 재연결을 기다립니다."
          error={query.error}
          action={<button type="button" className="button button-secondary" onClick={query.refresh}>다시 연결</button>}
        />
      )}
      {query.status === "error" && query.data && (
        <PageState
          kind="stale"
          title="마지막 읽기 결과를 표시합니다."
          detail={`새 근거를 가져오지 못했습니다. 마지막 조회 ${query.fetchedAt ? formatDateTime(query.fetchedAt) : "기록 없음"}.`}
          error={query.error}
        />
      )}
      {(query.data?.complete === false || targetQuery.data?.complete === false) && (
        <PageState
          kind="partial"
          title="Connection inventory is partial."
          detail="Actions that require a complete target selection are disabled until every cursor page is read."
        />
      )}

      {query.data && (
        <div className="inventory-layout">
          <section className="inventory-main" aria-labelledby="connection-list-title">
            <header className="section-heading">
              <div>
                <span className="eyebrow">Profiles</span>
                <h2 id="connection-list-title">{rows.length} connections</h2>
              </div>
            </header>
            <InventoryTable
              ariaLabel="Connection inventory"
              rows={rows}
              rowKey={(row) => row.id}
              selectedId={selected?.id ?? null}
              onSelect={select}
              emptyMessage="이 필터에 해당하는 Connection이 없습니다."
              columns={[
                {
                  key: "name",
                  header: "Connection",
                  render: (row) => (
                    <span className="resource-name" title={row.displayName}>
                      <strong>{row.displayName}</strong>
                      <small className="mono">{row.endpoint}</small>
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
                  key: "probe",
                  header: "Latest probe",
                  render: (row) => (
                    <StatusBadge
                      label={row.lastProbeStatus ?? "not probed"}
                      tone={toneForState(row.lastProbeStatus ?? "unknown")}
                      evidenceAtMs={row.lastProbeAtMs}
                    />
                  ),
                },
                {
                  key: "targets",
                  header: "Targets",
                  className: "mono",
                  render: (row) => row.targetCount,
                },
              ]}
            />
          </section>

          {selected ? (
            <Inspector
              eyebrow="Connection profile"
              title={selected.displayName}
              actions={
                <>
                  <button type="button" className="button button-secondary">Probe 다시 실행</button>
                  <button
                    type="button"
                    className="button button-primary"
                    disabled={targetQuery.data?.complete !== true}
                    onClick={() =>
                      transitionTo({
                        page: "databases",
                        filter: selected.id,
                        selected: selectedTargets[0]?.id,
                      })
                    }
                  >
                    Databases 보기
                  </button>
                </>
              }
            >
              <dl className="evidence-list">
                <EvidenceRow label="상태" value={<StatusBadge label={selected.state} tone={toneForState(selected.state)} />} />
                <EvidenceRow label="Engine" value={selected.engine} mono />
                <EvidenceRow label="Transport" value={selected.transport} mono />
                <EvidenceRow label="Endpoint" value={selected.endpoint} mono />
                <EvidenceRow label="Latest probe" value={formatDateTime(selected.lastProbeAtMs)} mono />
                <EvidenceRow
                  label="Restore target role"
                  value={
                    <StatusBadge
                      label={
                        selected.restoreTestTarget
                          ? "isolated restore-test"
                          : "general"
                      }
                      tone={selected.restoreTestTarget ? "info" : "neutral"}
                    />
                  }
                />
                <EvidenceRow label="Targets" value={selected.targetCount} mono />
              </dl>
              <section className="inspector-section">
                <h3>Database Targets</h3>
                {selectedTargets.length ? (
                  <ul className="compact-list">
                    {selectedTargets.map((item) => (
                      <li key={item.id}>
                        <span title={item.displayName}>{item.displayName}</span>
                        <StatusBadge
                          label={item.protectionCompliance}
                          tone={toneForState(item.protectionCompliance)}
                        />
                      </li>
                    ))}
                  </ul>
                ) : (
                  <p className="muted-copy">
                    검색된 DB를 아직 Target으로 선택하지 않았습니다. Connection ready는 보호 완료를 뜻하지 않습니다.
                  </p>
                )}
              </section>
            </Inspector>
          ) : (
            <PageState
              kind="empty"
              title="Connection을 선택하세요."
              detail="선택한 프로필의 probe 근거와 Database Target을 여기에 표시합니다."
            />
          )}
        </div>
      )}

      {wizardOpen && (
        <Dialog
          title="Connection onboarding"
          description="Profile, probe, Database Target 선택을 한 단계씩 진행합니다."
          onClose={closeWizard}
        >
          <ConnectionWizard
            onCancel={closeWizard}
            onComplete={(_connectionId, targetId) => {
              setWizardOpen(false);
              query.refresh();
              targetQuery.refresh();
              transitionTo({
                page: "backup-plans",
                selected: targetId,
                action: "new",
              });
            }}
          />
        </Dialog>
      )}
    </div>
  );
}
