import { InventoryTable } from "../components/InventoryTable";
import { LoadingState, PageState } from "../components/PageState";
import { PageHeader } from "../components/PageHeader";
import { StatusBadge, toneForState } from "../components/StatusBadge";
import { readSettings } from "../lib/api";
import {
  formatBytes,
  formatDateTime,
  formatFreshness,
} from "../lib/format";
import { useServiceQuery } from "../lib/queries";

export function Settings() {
  const query = useServiceQuery("settings", readSettings);

  if (query.status === "loading" && !query.data) {
    return <LoadingState label="서비스 실행 자원과 도구 신뢰 근거를 조회하는 중입니다." />;
  }
  if (query.status === "error" && !query.data) {
    return (
      <PageState
        kind="error"
        title="Settings 근거를 불러오지 못했습니다."
        detail="로컬 설정 파일을 직접 읽지 않습니다. 서비스 연결을 복구한 뒤 다시 조회하세요."
        error={query.error}
        action={
          <button type="button" className="button button-secondary" onClick={query.refresh}>
            다시 조회
          </button>
        }
      />
    );
  }
  if (!query.data) return null;

  return (
    <div className="page-stack">
      <PageHeader
        eyebrow="Runtime evidence"
        title="Settings"
        description="실행 pool, spool 여유량, 고정 native tool 신뢰, 알림과 control-plane 백업을 서비스 근거로 확인합니다."
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

      {query.status === "error" && (
        <PageState
          kind="stale"
          title="마지막 Settings 근거를 표시합니다."
          detail="새 작업 허용 여부는 이 화면의 오래된 값만으로 판단하지 않습니다."
          error={query.error}
        />
      )}
      {!query.data.complete && (
        <PageState
          kind="partial"
          title="Settings inventory is partial."
          detail={`Database-enforced caps returned resource pools ${query.data.resourcePools.length}/${query.data.inventoryCounts.resourcePools}, spool volumes ${query.data.spoolVolumes.length}/${query.data.inventoryCounts.spoolVolumes}, tools ${query.data.tools.length}/${query.data.inventoryCounts.tools}, and notification channels ${query.data.notificationChannels.length}/${query.data.inventoryCounts.notificationChannels}. Missing rows are not treated as healthy defaults.`}
        />
      )}

      <section aria-labelledby="pool-title">
        <header className="section-heading">
          <div>
            <span className="eyebrow">Bounded concurrency</span>
            <h2 id="pool-title">Resource pools</h2>
          </div>
        </header>
        <InventoryTable
          ariaLabel="Runtime resource pools"
          rows={query.data.resourcePools}
          rowKey={(row) => row.id}
          selectedId={null}
          onSelect={() => undefined}
          emptyMessage="등록된 resource pool이 없습니다."
          columns={[
            {
              key: "resource",
              header: "Resource",
              render: (row) => (
                <span className="resource-name">
                  <strong>{row.resourceType}</strong>
                  <small>{row.resourceKey}</small>
                </span>
              ),
            },
            {
              key: "capacity",
              header: "In use / Capacity",
              className: "mono",
              render: (row) => `${row.inUse} / ${row.capacity}`,
            },
            {
              key: "state",
              header: "Admission",
              render: (row) => (
                <StatusBadge
                  label={row.enabled ? "enabled" : "disabled"}
                  tone={row.enabled ? "success" : "warning"}
                />
              ),
            },
          ]}
        />
      </section>

      <div className="settings-split">
        <section aria-labelledby="spool-title">
          <header className="section-heading">
            <div>
              <span className="eyebrow">Disk admission</span>
              <h2 id="spool-title">Spool volumes</h2>
            </div>
          </header>
          <ul className="evidence-cards">
            {query.data.spoolVolumes.map((volume) => (
              <li key={volume.id}>
                <div>
                  <strong className="mono">{volume.rootPath}</strong>
                  <StatusBadge
                    label={volume.state}
                    tone={toneForState(volume.state)}
                    evidenceAtMs={volume.probedAtMs}
                  />
                </div>
                <dl>
                  <div><dt>Free</dt><dd>{formatBytes(volume.freeBytes)}</dd></div>
                  <div><dt>Soft limit</dt><dd>{formatBytes(volume.softLimitBytes)}</dd></div>
                  <div><dt>Hard limit</dt><dd>{formatBytes(volume.hardLimitBytes)}</dd></div>
                </dl>
              </li>
            ))}
          </ul>
        </section>

        <section aria-labelledby="control-backup-title">
          <header className="section-heading">
            <div>
              <span className="eyebrow">Control plane</span>
              <h2 id="control-backup-title">Catalog backup</h2>
            </div>
          </header>
          <div className="control-backup">
            <StatusBadge
              label={query.data.controlBackup.lastStatus}
              tone={toneForState(query.data.controlBackup.lastStatus)}
              evidenceAtMs={query.data.controlBackup.lastCompletedAtMs}
            />
            <p>
              마지막 완료{" "}
              <span className="mono">
                {formatDateTime(query.data.controlBackup.lastCompletedAtMs)}
              </span>
            </p>
            <small>온라인 SQLite backup API 결과만 성공 근거로 사용합니다.</small>
          </div>
        </section>
      </div>

      <section aria-labelledby="tools-title">
        <header className="section-heading">
          <div>
            <span className="eyebrow">Pinned toolchain</span>
            <h2 id="tools-title">Native tools</h2>
          </div>
        </header>
        <InventoryTable
          ariaLabel="Native tool inventory"
          rows={query.data.tools}
          rowKey={(row) => row.id}
          selectedId={null}
          onSelect={() => undefined}
          emptyMessage="검증된 native tool이 없습니다."
          columns={[
            {
              key: "tool",
              header: "Tool",
              render: (row) => (
                <span className="resource-name">
                  <strong>{row.name}</strong>
                  <small>{row.architecture}</small>
                </span>
              ),
            },
            { key: "version", header: "Version", className: "mono", render: (row) => row.version },
            {
              key: "trust",
              header: "Trust",
              render: (row) => (
                <StatusBadge
                  label={row.trustStatus}
                  tone={toneForState(row.trustStatus)}
                  evidenceAtMs={row.verifiedAtMs}
                />
              ),
            },
            {
              key: "signature",
              header: "Signature",
              render: (row) => <StatusBadge label={row.signatureStatus} tone={toneForState(row.signatureStatus)} />,
            },
          ]}
        />
      </section>

      <section aria-labelledby="notifications-title">
        <header className="section-heading">
          <div>
            <span className="eyebrow">Delivery evidence</span>
            <h2 id="notifications-title">Notification channels</h2>
          </div>
        </header>
        <InventoryTable
          ariaLabel="Notification channels"
          rows={query.data.notificationChannels}
          rowKey={(row) => row.id}
          selectedId={null}
          onSelect={() => undefined}
          emptyMessage="설정된 알림 채널이 없습니다."
          columns={[
            {
              key: "channel",
              header: "Channel",
              render: (row) => (
                <span className="resource-name">
                  <strong>{row.displayName}</strong>
                  <small>{row.kind}</small>
                </span>
              ),
            },
            {
              key: "state",
              header: "State",
              render: (row) => <StatusBadge label={row.state} tone={toneForState(row.state)} />,
            },
            {
              key: "delivery",
              header: "Latest test",
              render: (row) => (
                <StatusBadge
                  label={row.lastTestStatus ?? "not tested"}
                  tone={toneForState(row.lastTestStatus ?? "unknown")}
                  evidenceAtMs={row.lastTestAtMs}
                />
              ),
            },
          ]}
        />
      </section>
    </div>
  );
}
