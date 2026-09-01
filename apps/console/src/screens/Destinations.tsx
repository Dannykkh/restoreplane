import { useMemo, useState } from "react";
import { DestinationWizard } from "../components/DestinationWizard";
import { Dialog } from "../components/Dialog";
import { EvidenceRow, Inspector } from "../components/Inspector";
import { InventoryTable } from "../components/InventoryTable";
import { LoadingState, PageState } from "../components/PageState";
import { FilterField, PageHeader } from "../components/PageHeader";
import { StatusBadge, toneForState } from "../components/StatusBadge";
import {
  getWorkItem,
  listDestinations,
  probeDestination,
} from "../lib/api";
import { asServiceError, ServiceError } from "../lib/errors";
import { formatDateTime, formatFreshness } from "../lib/format";
import { useMutationAttemptRegistry } from "../lib/mutationAttempts";
import {
  collectCursorPages,
  INVENTORY_PAGE_LIMIT,
  type CollectedCursorPage,
} from "../lib/pagination";
import { useServiceQuery } from "../lib/queries";
import { transitionTo } from "../lib/route";
import type { Destination, RouteState } from "../lib/types";

interface DestinationsProps {
  route: RouteState;
}

export function Destinations({ route }: DestinationsProps) {
  const mutationAttempts = useMutationAttemptRegistry();
  const [wizardOpen, setWizardOpen] = useState(route.action === "new");
  const [probeBusy, setProbeBusy] = useState(false);
  const [probeError, setProbeError] = useState<ServiceError | null>(null);
  const query = useServiceQuery<CollectedCursorPage<Destination>>("destinations", () =>
    collectCursorPages((cursor) =>
      listDestinations({ cursor, limit: INVENTORY_PAGE_LIMIT })
    ),
  );
  const filter = route.filter ?? "all";
  const rows = useMemo(
    () =>
      (query.data?.items ?? []).filter(
        (destination) =>
          filter === "all" ||
          destination.kind === filter ||
          destination.state === filter,
      ),
    [filter, query.data],
  );
  const selected =
    rows.find((destination) => destination.id === route.selected) ?? rows[0] ?? null;
  const inventoryComplete = query.status === "ready" && query.data?.complete === true;

  function closeWizard(): void {
    setWizardOpen(false);
    transitionTo({ ...route, action: undefined }, true);
  }

  async function runProbe(destinationId: string): Promise<void> {
    if (!inventoryComplete) return;
    setProbeBusy(true);
    setProbeError(null);
    try {
      const work = await mutationAttempts.run(
        `destination-probe:${destinationId}`,
        "destination.probe",
        { destinationId },
        (key) => probeDestination(destinationId, key),
      );
      if (!isSuccessful(work.state)) {
        await waitForSuccessfulWork(work.workItemId);
      }
      query.refresh();
    } catch (failure) {
      setProbeError(asServiceError(failure));
    } finally {
      setProbeBusy(false);
    }
  }

  if (query.status === "loading" && !query.data) return <LoadingState label="Destination 역할 근거를 불러오는 중입니다." />;
  if (query.status === "error" && !query.data) {
    return (
      <PageState
        kind="error"
        title="Destination inventory를 읽지 못했습니다."
        detail="writer 성공만으로 reader와 delete 권한을 추정하지 않습니다."
        error={query.error}
        action={<button type="button" className="button button-secondary" onClick={query.refresh}>다시 연결</button>}
      />
    );
  }

  return (
    <div className="page-stack">
      <PageHeader
        eyebrow="Replica endpoints"
        title="Destinations"
        description="백업 writer, 복구 reader, 보존 delete 역할을 각각 probe하고 Plan 영향과 불변성 근거를 표시합니다."
        meta={query.fetchedAt && <span>근거 {formatFreshness(query.fetchedAt)}</span>}
        actions={
          <button
            type="button"
            className="button button-primary"
            disabled={!inventoryComplete}
            onClick={() => {
              if (!inventoryComplete) return;
              setWizardOpen(true);
              transitionTo({ ...route, action: "new" }, true);
            }}
          >
            새 Destination
          </button>
        }
      />
      <div className="filter-bar">
        <FilterField label="종류 또는 상태">
          <select
            name="destinationFilter"
            value={filter}
            onChange={(event) =>
              transitionTo(
                {
                  page: "destinations",
                  filter: event.target.value === "all" ? undefined : event.target.value,
                },
                true,
              )
            }
          >
            <option value="all">전체</option>
            <option value="ready">Ready</option>
            <option value="attention">Attention</option>
            <option value="local">Local</option>
            <option value="s3">S3</option>
            <option value="ssh_sftp">SSH/SFTP</option>
          </select>
        </FilterField>
      </div>
      {query.status === "error" && query.data && (
        <PageState
          kind="stale"
          title="마지막 Destination 근거입니다."
          detail="새 probe가 실패했으므로 이 근거의 시각과 역할별 상태를 확인하세요."
          error={query.error}
          action={<button type="button" className="button button-secondary" onClick={query.refresh}>Destination 다시 조회</button>}
        />
      )}
      {query.data?.complete === false && (
        <PageState
          kind="partial"
          title="Destination inventory is partial."
          detail="The bounded cursor scan did not reach a verified end; selection-dependent probes are disabled."
        />
      )}
      {probeError && (
        <PageState
          kind="error"
          title={probeError.message}
          detail="writer 성공만으로 복구 가능 상태를 만들지 않습니다. 실패한 역할의 endpoint 또는 자격 증명을 확인하세요."
          error={probeError}
        />
      )}
      <div className="inventory-layout">
        <section className="inventory-main" aria-labelledby="destination-list-title">
          <header className="section-heading">
            <div>
              <span className="eyebrow">Storage roles</span>
              <h2 id="destination-list-title">{rows.length} destinations</h2>
            </div>
          </header>
          <InventoryTable
            ariaLabel="Destination inventory"
            rows={rows}
            rowKey={(row) => row.id}
            selectedId={selected?.id ?? null}
            onSelect={(row) => transitionTo({ ...route, selected: row.id }, true)}
            emptyMessage="이 필터에 해당하는 Destination이 없습니다."
            columns={[
              {
                key: "destination",
                header: "Destination",
                render: (row) => (
                  <span className="resource-name" title={row.displayName}>
                    <strong>{row.displayName}</strong>
                    <small className="mono">{row.endpoint}</small>
                  </span>
                ),
              },
              { key: "kind", header: "Kind", className: "mono", render: (row) => row.kind },
              {
                key: "writer",
                header: "Writer",
                render: (row) => {
                  const role = row.roles.find((item) => item.role === "backup_writer");
                  return <StatusBadge label={role?.probeStatus ?? (role?.credentialBound ? "configured" : "unknown")} tone={toneForState(role?.probeStatus ?? "unknown")} evidenceAtMs={role?.probedAtMs} />;
                },
              },
              {
                key: "reader",
                header: "Reader",
                render: (row) => {
                  const role = row.roles.find((item) => item.role === "restore_reader");
                  return <StatusBadge label={role?.probeStatus ?? (role?.credentialBound ? "configured" : "unknown")} tone={toneForState(role?.probeStatus ?? "unknown")} evidenceAtMs={role?.probedAtMs} />;
                },
              },
              {
                key: "immutability",
                header: "Immutability",
                render: (row) => <StatusBadge label={row.immutabilityCapability} tone={toneForState(row.immutabilityCapability)} />,
              },
            ]}
          />
        </section>
        {selected ? (
          <Inspector
            eyebrow="Destination"
            title={selected.displayName}
            actions={
              <button
                type="button"
                className="button button-secondary"
                disabled={probeBusy || !inventoryComplete}
                onClick={() => void runProbe(selected.id)}
              >
                {probeBusy ? "역할 Probe 실행 중" : "역할별 Probe"}
              </button>
            }
          >
            <dl className="evidence-list">
              <EvidenceRow label="State" value={<StatusBadge label={selected.state} tone={toneForState(selected.state)} />} />
              <EvidenceRow label="Kind" value={selected.kind} mono />
              <EvidenceRow label="Endpoint" value={selected.endpoint} mono />
              <EvidenceRow label="Latest aggregate probe" value={formatDateTime(selected.lastProbeAtMs)} mono />
              <EvidenceRow
                label="Immutability"
                value={<StatusBadge label={selected.immutabilityCapability} tone={toneForState(selected.immutabilityCapability)} />}
              />
            </dl>
            <section className="inspector-section">
              <h3>Credential roles</h3>
              <ul className="role-list">
                {selected.roles.map((role) => (
                  <li key={role.role}>
                    <div>
                      <strong className="mono">{role.role}</strong>
                      <small>{role.credentialBound ? "별도 credential 연결됨" : "credential 없음 또는 불필요"}</small>
                    </div>
                    <StatusBadge
                      label={role.probeStatus ?? (role.credentialBound ? "configured" : "unknown")}
                      tone={toneForState(role.probeStatus ?? "unknown")}
                      evidenceAtMs={role.probedAtMs}
                    />
                  </li>
                ))}
              </ul>
            </section>
            {selected.roles.some((role) => role.role === "restore_reader" && role.probeStatus !== "passed") && (
              <PageState
                kind="partial"
                title="백업 쓰기와 복구 읽기 근거가 다릅니다."
                detail="writer가 통과했더라도 reader가 통과하기 전에는 복구 가능 Destination으로 간주하지 않습니다."
              />
            )}
          </Inspector>
        ) : (
          <PageState kind="empty" title="Destination을 선택하세요." detail="역할별 credential과 probe 근거를 표시합니다." />
        )}
      </div>
      {wizardOpen && inventoryComplete && (
        <Dialog
          title="Destination onboarding"
          description="Endpoint, 역할별 자격 증명, 실제 객체 probe를 순서대로 진행합니다."
          onClose={closeWizard}
        >
          <DestinationWizard
            onCancel={closeWizard}
            onComplete={(destinationId) => {
              setWizardOpen(false);
              query.refresh();
              transitionTo({
                page: "destinations",
                selected: destinationId,
              });
            }}
          />
        </Dialog>
      )}
    </div>
  );
}

function isSuccessful(state: string): boolean {
  return state === "succeeded" || state === "partially_succeeded";
}

async function waitForSuccessfulWork(workItemId: string) {
  for (let attempt = 0; attempt < 120; attempt += 1) {
    const work = await getWorkItem(workItemId);
    if (isSuccessful(work.state)) return work;
    if (
      work.state === "failed_terminal" ||
      work.state === "failed" ||
      work.state === "cancelled"
    ) {
      throw new ServiceError(
        work.errorCode ?? "DESTINATION_PROBE_FAILED",
        { retryable: false },
      );
    }
    await new Promise((resolve) => window.setTimeout(resolve, 500));
  }
  throw new ServiceError("PROCESS_TIMEOUT", { retryable: true });
}
