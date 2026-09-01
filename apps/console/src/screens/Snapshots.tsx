import { useMemo, useState } from "react";
import { EvidenceRow, Inspector } from "../components/Inspector";
import { InventoryTable } from "../components/InventoryTable";
import { LoadingState, PageState } from "../components/PageState";
import { FilterField, PageHeader } from "../components/PageHeader";
import { StatusBadge, toneForState } from "../components/StatusBadge";
import {
  enqueueVerification,
  getSnapshotDetailPage,
  listSnapshots,
} from "../lib/api";
import { asServiceError, type ServiceError } from "../lib/errors";
import {
  formatBytes,
  formatDateTime,
  formatFreshness,
} from "../lib/format";
import { useDebouncedValue, useServiceQuery } from "../lib/queries";
import { useMutationAttemptRegistry } from "../lib/mutationAttempts";
import { collectCursorPages, INVENTORY_PAGE_LIMIT, type CollectedCursorPage } from "../lib/pagination";
import { isDiagnosticSnapshot } from "../lib/recovery";
import { routeFor, transitionTo } from "../lib/route";
import {
  collectSnapshotSectionPages,
  isExactSnapshotInventory,
  selectCompleteReaderReplica,
  snapshotMutationInventoryReady,
  type CollectedSnapshotSection,
} from "../lib/snapshotInventory";
import type {
  RestoreDrillEvidence,
  RouteState,
  Snapshot,
  SnapshotPart,
  SnapshotReplica,
  VerificationEnqueueResult,
  VerificationEvidence,
} from "../lib/types";

interface SnapshotDetails {
  snapshotId: string;
  parts: CollectedSnapshotSection<SnapshotPart, "parts">;
  replicas: CollectedSnapshotSection<SnapshotReplica, "replicas">;
  verificationEvidence: CollectedSnapshotSection<
    VerificationEvidence,
    "verification_evidence"
  >;
  restoreDrills: CollectedSnapshotSection<
    RestoreDrillEvidence,
    "restore_drills"
  >;
}

export function Snapshots({ route }: { route: RouteState }) {
  const mutationAttempts = useMutationAttemptRegistry();
  const scope = route.filter === "diagnostics" ? "diagnostics" : "complete";
  const [search, setSearch] = useState("");
  const [engine, setEngine] = useState("");
  const [connectionId, setConnectionId] = useState("");
  const [artifact, setArtifact] = useState("");
  const [durability, setDurability] = useState("");
  const [compliance, setCompliance] = useState("");
  const [eligibility, setEligibility] = useState("");
  const [signature, setSignature] = useState("");
  const [minimumVerification, setMinimumVerification] = useState("");
  const [fromDate, setFromDate] = useState("");
  const [toDate, setToDate] = useState("");
  const debouncedSearch = useDebouncedValue(search, 250);
  const queryKey = JSON.stringify({
    scope,
    search: debouncedSearch,
    engine,
    connectionId,
    artifact,
    durability,
    compliance,
    eligibility,
    signature,
    minimumVerification,
    fromDate,
    toDate,
  });
  const query = useServiceQuery<CollectedCursorPage<Snapshot>>(
    `snapshots:${queryKey}`,
    () =>
      collectCursorPages((cursor) =>
        listSnapshots({
          cursor,
          limit: INVENTORY_PAGE_LIMIT,
          scope,
          search: debouncedSearch.trim() || null,
          engine: engine || null,
          connectionId: connectionId || null,
          artifactCompleteness: artifact || null,
          replicaDurability: durability || null,
          protectionCompliance: compliance || null,
          restoreEligibility: eligibility || null,
          signatureStatus: signature || null,
          minVerificationLevel: minimumVerification
            ? Number(minimumVerification)
            : null,
          fromMs: fromDate ? new Date(`${fromDate}T00:00:00`).getTime() : null,
          toMs: toDate ? new Date(`${toDate}T23:59:59.999`).getTime() : null,
        }),
      ),
  );
  const [verificationResult, setVerificationResult] =
    useState<VerificationEnqueueResult | null>(null);
  const [actionError, setActionError] = useState<ServiceError | null>(null);
  const [busy, setBusy] = useState(false);
  const rows = query.data?.items ?? [];
  const selected =
    rows.find((snapshot) => snapshot.id === route.selected) ?? rows[0] ?? null;
  const detailsQuery = useServiceQuery<SnapshotDetails | null>(
    `snapshot-details:${selected?.id ?? "none"}`,
    () => (selected ? loadSnapshotDetails(selected.id) : Promise.resolve(null)),
  );
  const details = detailsQuery.data?.snapshotId === selected?.id
    ? detailsQuery.data
    : null;
  const readerReplica = selectCompleteReaderReplica(details?.replicas ?? null);
  const detailInventoryComplete = details !== null &&
    isExactSnapshotInventory(details.parts) &&
    isExactSnapshotInventory(details.replicas) &&
    isExactSnapshotInventory(details.verificationEvidence) &&
    isExactSnapshotInventory(details.restoreDrills);
  const mutationInventoryComplete = snapshotMutationInventoryReady(
    query.status,
    detailsQuery.status,
    query.data?.complete === true,
    detailInventoryComplete,
  );
  const connectionOptions = useMemo(() => {
    const unique = new Map<string, string>();
    for (const snapshot of rows) {
      if (snapshot.sourceConnectionId) {
        unique.set(
          snapshot.sourceConnectionId,
          snapshot.sourceConnectionName ?? snapshot.sourceConnectionId,
        );
      }
    }
    return [...unique.entries()];
  }, [rows]);

  async function verify(snapshot: Snapshot, level: 1 | 2 | 3): Promise<void> {
    const replica = readerReplica;
    if (!replica || !mutationInventoryComplete) return;
    setBusy(true);
    setActionError(null);
    try {
      const payload = {
        snapshotId: snapshot.id,
        level,
        purpose: "manual",
        sourceReplicaId: replica.id,
      };
      setVerificationResult(await mutationAttempts.run(
        `snapshot-verification:${snapshot.id}`,
        "verification.enqueue",
        payload,
        (key) => enqueueVerification(snapshot.id, level, replica.id, key),
      ));
    } catch (error) {
      setActionError(asServiceError(error));
    } finally {
      setBusy(false);
    }
  }

  if (query.status === "loading" && !query.data) {
    return <LoadingState label="Recovery Point와 Replica 근거를 조회하는 중입니다." />;
  }
  if (query.status === "error" && !query.data) {
    return (
      <PageState
        kind="error"
        title="Snapshots를 불러오지 못했습니다."
        detail="파일시스템을 직접 스캔해 성공을 추정하지 않습니다. 서비스 catalog를 복구한 뒤 다시 조회하세요."
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
    );
  }
  if (!query.data) return null;

  return (
    <div className="page-stack">
      <PageHeader
        eyebrow="Recovery truth"
        title="Snapshots"
        description="complete Recovery Point와 ingest diagnostics를 분리하고, Manifest·Part·Replica·L1/L2/L3·restore drill 근거를 같은 inspector에서 확인합니다."
        meta={
          <span>
            {query.data.totalCount} recovery points · 조회{" "}
            {query.fetchedAt
              ? formatFreshness(query.fetchedAt)
              : "기록 없음"}
          </span>
        }
        actions={
          <button
            type="button"
            className="button button-secondary"
            onClick={query.refresh}
          >
            근거 새로고침
          </button>
        }
      />

      <nav className="segmented-nav" aria-label="Snapshot inventory scope">
        <a
          href={routeFor("snapshots")}
          aria-current={scope === "complete" ? "page" : undefined}
        >
          Complete inventory
        </a>
        <a
          href={routeFor("snapshots", { filter: "diagnostics" })}
          aria-current={scope === "diagnostics" ? "page" : undefined}
        >
          Ingest diagnostics
        </a>
      </nav>

      <div className="snapshot-filter-panel">
        <label className="field snapshot-search">
          <span>Target 또는 Connection 검색</span>
          <input
            type="search"
            name="snapshotSearch"
            value={search}
            onChange={(event) => setSearch(event.target.value)}
            placeholder="orders, Production PostgreSQL"
            autoComplete="off"
          />
        </label>
        <FilterField label="Connection">
          <select
            name="snapshotConnection"
            value={connectionId}
            onChange={(event) => setConnectionId(event.target.value)}
          >
            <option value="">전체</option>
            {connectionOptions.map(([id, name]) => (
              <option key={id} value={id}>
                {name}
              </option>
            ))}
          </select>
        </FilterField>
        <FilterField label="Engine">
          <select
            name="snapshotEngine"
            value={engine}
            onChange={(event) => setEngine(event.target.value)}
          >
            <option value="">전체</option>
            <option value="postgresql">PostgreSQL</option>
            <option value="mysql">MySQL</option>
            <option value="mariadb">MariaDB</option>
            <option value="sqlserver">SQL Server</option>
            <option value="sqlite">SQLite</option>
          </select>
        </FilterField>
        <FilterField label="Artifact">
          <select
            name="snapshotArtifact"
            value={artifact}
            onChange={(event) => setArtifact(event.target.value)}
          >
            <option value="">전체</option>
            <option value="complete">Complete</option>
            <option value="incomplete">Incomplete</option>
          </select>
        </FilterField>
        <FilterField label="Replica durability">
          <select
            name="snapshotDurability"
            value={durability}
            onChange={(event) => setDurability(event.target.value)}
          >
            <option value="">전체</option>
            <option value="healthy">Healthy</option>
            <option value="degraded">Degraded</option>
            <option value="unavailable">Unavailable</option>
          </select>
        </FilterField>
        <FilterField label="Compliance">
          <select
            name="snapshotCompliance"
            value={compliance}
            onChange={(event) => setCompliance(event.target.value)}
          >
            <option value="">전체</option>
            <option value="compliant">Compliant</option>
            <option value="non_compliant">Non-compliant</option>
          </select>
        </FilterField>
        <FilterField label="Restore">
          <select
            name="snapshotEligibility"
            value={eligibility}
            onChange={(event) => setEligibility(event.target.value)}
          >
            <option value="">전체</option>
            <option value="eligible">Eligible</option>
            <option value="conditional">Conditional</option>
            <option value="blocked">Blocked</option>
          </select>
        </FilterField>
        <FilterField label="Signature">
          <select
            name="snapshotSignature"
            value={signature}
            onChange={(event) => setSignature(event.target.value)}
          >
            <option value="">전체</option>
            <option value="valid">Valid</option>
            <option value="invalid">Invalid</option>
            <option value="unknown_key">Unknown key</option>
            <option value="not_checked">Not checked</option>
          </select>
        </FilterField>
        <FilterField label="Minimum verify">
          <select
            name="snapshotVerification"
            value={minimumVerification}
            onChange={(event) => setMinimumVerification(event.target.value)}
          >
            <option value="">전체</option>
            <option value="1">L1+</option>
            <option value="2">L2+</option>
            <option value="3">L3</option>
          </select>
        </FilterField>
        <label className="field">
          <span>From</span>
          <input
            type="date"
            name="snapshotFrom"
            value={fromDate}
            onChange={(event) => setFromDate(event.target.value)}
          />
        </label>
        <label className="field">
          <span>To</span>
          <input
            type="date"
            name="snapshotTo"
            value={toDate}
            onChange={(event) => setToDate(event.target.value)}
          />
        </label>
      </div>

      {query.status === "error" && (
        <PageState
          kind="stale"
          title="마지막 Snapshot 근거를 표시합니다."
          detail="새 복구나 검증은 service 재연결 전까지 실행하지 않습니다."
          error={query.error}
        />
      )}
      {query.data.complete === false && (
        <PageState
          kind="partial"
          title="Snapshot 목록이 완전하지 않습니다."
          detail="모든 페이지와 정확한 총계가 확인될 때까지 검증과 복구 작업을 실행하지 않습니다."
        />
      )}
      {selected && detailsQuery.status === "error" && (
        <PageState
          kind="partial"
          title="선택한 Snapshot 상세 목록을 확인하지 못했습니다."
          detail="Replica, Part, 검증 및 restore drill 목록이 완전해질 때까지 변경 작업을 차단합니다."
          error={detailsQuery.error}
        />
      )}
      {selected && details && !detailInventoryComplete && (
        <PageState
          kind="partial"
          title="선택한 Snapshot 상세 목록이 불완전합니다."
          detail="서버 총계와 모든 상세 페이지가 일치하지 않아 검증과 복구 작업을 실행하지 않습니다."
        />
      )}
      {verificationResult && (
        <PageState
          kind={
            verificationResult.state === "unsupported" ? "partial" : "partial"
          }
          title={
            verificationResult.state === "unsupported"
              ? `L${verificationResult.requestedLevel}는 자동 하향하지 않았습니다.`
              : `L${verificationResult.requestedLevel} 검증 작업이 접수되었습니다.`
          }
          detail={
            verificationResult.errorCode ??
            verificationResult.limitation ??
            `Work Item ${verificationResult.workItemId}`
          }
          action={
            verificationResult.workItemId ? (
              <a
                className="button button-secondary"
                href={routeFor("activity", {
                  selected: verificationResult.workItemId,
                })}
              >
                Activity 보기
              </a>
            ) : undefined
          }
        />
      )}
      {actionError && (
        <PageState
          kind={
            actionError.code === "APPLICATION_FORBIDDEN"
              ? "permission"
              : "error"
          }
          title={actionError.message}
          detail="Snapshot의 기존 검증 근거는 변경되지 않았습니다."
          error={actionError}
        />
      )}

      <div className="inventory-layout">
        <section className="inventory-main" aria-labelledby="snapshot-list-title">
          <header className="section-heading">
            <div>
              <span className="eyebrow">
                {scope === "complete"
                  ? "Committed recovery points"
                  : "Incomplete and quarantined ingest"}
              </span>
              <h2 id="snapshot-list-title">{rows.length} snapshots</h2>
            </div>
          </header>
          <InventoryTable
            ariaLabel="Snapshot inventory"
            rows={rows}
            rowKey={(row) => row.id}
            selectedId={selected?.id ?? null}
            onSelect={(row) =>
              transitionTo({ ...route, selected: row.id }, true)
            }
            emptyMessage="조건에 맞는 Recovery Point가 없습니다."
            columns={[
              {
                key: "source",
                header: "Recovery Point",
                render: (row) => (
                  <span className="resource-name" title={row.sourceDisplayName}>
                    <strong>{row.sourceDisplayName}</strong>
                    <small>
                      {row.engine} ·{" "}
                      {row.committedAtMs
                        ? formatDateTime(row.committedAtMs)
                        : `ingest ${formatDateTime(row.lastIngestedAtMs)}`}
                    </small>
                  </span>
                ),
              },
              {
                key: "parts",
                header: "Parts",
                className: "mono",
                render: (row) =>
                  `${row.presentPartCount} / ${row.requiredPartCount}`,
              },
              {
                key: "replicas",
                header: "Replica",
                render: (row) => (
                  <StatusBadge
                    label={row.replicaDurability}
                    tone={toneForState(row.replicaDurability)}
                  />
                ),
              },
              {
                key: "compliance",
                header: "Compliance",
                render: (row) => (
                  <StatusBadge
                    label={row.protectionCompliance}
                    tone={toneForState(row.protectionCompliance)}
                  />
                ),
              },
              {
                key: "verify",
                header: "Verified",
                render: (row) => (
                  <StatusBadge
                    label={
                      row.highestVerifiedLevel
                        ? `L${row.highestVerifiedLevel}`
                        : "not verified"
                    }
                    tone={row.highestVerifiedLevel ? "success" : "warning"}
                    evidenceAtMs={row.lastVerifiedAtMs}
                  />
                ),
              },
              {
                key: "restore",
                header: "Restore",
                render: (row) => (
                  <StatusBadge
                    label={row.restoreEligibility}
                    tone={toneForState(row.restoreEligibility)}
                  />
                ),
              },
            ]}
          />
        </section>

        {selected ? (
          <Inspector
            eyebrow={
              isDiagnosticSnapshot(selected)
                ? "Ingest diagnostic"
                : "Recovery point"
            }
            title={selected.sourceDisplayName}
            actions={
              isDiagnosticSnapshot(selected) ? (
                <a className="button button-secondary" href={routeFor("recovery")}>
                  Catalog rebuild preview
                </a>
              ) : (
                <>
                  <div className="button-row" aria-label="Verification actions">
                    {[1, 2, 3].map((level) => (
                      <button
                        key={level}
                        type="button"
                        className="button button-secondary"
                        disabled={busy || !readerReplica || !mutationInventoryComplete}
                        onClick={() => void verify(selected, level as 1 | 2 | 3)}
                      >
                        L{level}
                      </button>
                    ))}
                  </div>
                  {selected.restoreEligibility !== "blocked" &&
                    mutationInventoryComplete && (
                    <a
                      className="button button-primary"
                      href={routeFor("recovery", { selected: selected.id })}
                    >
                      안전 복구
                    </a>
                  )}
                </>
              )
            }
          >
            <dl className="evidence-list">
              <EvidenceRow label="Snapshot ID" value={selected.id} mono />
              <EvidenceRow
                label="State"
                value={
                  <StatusBadge
                    label={selected.state}
                    tone={toneForState(selected.state)}
                  />
                }
              />
              <EvidenceRow
                label="Connection"
                value={
                  selected.sourceConnectionName ??
                  selected.sourceConnectionId ??
                  "unresolved"
                }
              />
              <EvidenceRow
                label="Committed"
                value={formatDateTime(selected.committedAtMs)}
                mono
              />
              <EvidenceRow
                label="Plain / cipher"
                value={`${formatBytes(selected.plaintextBytes)} / ${formatBytes(selected.cipherBytes)}`}
              />
              <EvidenceRow
                label="Signature"
                value={
                  <StatusBadge
                    label={selected.signatureStatus}
                    tone={toneForState(selected.signatureStatus)}
                  />
                }
              />
              <EvidenceRow
                label="Signing key"
                value={selected.signingKeyId}
                mono
              />
              <EvidenceRow
                label="Manifest schema"
                value={`v${selected.manifestSchemaVersion}`}
                mono
              />
              <EvidenceRow
                label="Manifest checksum"
                value={selected.setManifestChecksumBlake3}
                mono
              />
              <EvidenceRow
                label="Artifact"
                value={
                  <StatusBadge
                    label={selected.artifactCompleteness}
                    tone={toneForState(selected.artifactCompleteness)}
                  />
                }
              />
              <EvidenceRow
                label="Compliance"
                value={
                  <StatusBadge
                    label={selected.protectionCompliance}
                    tone={toneForState(selected.protectionCompliance)}
                  />
                }
              />
              <EvidenceRow
                label="Last ingest"
                value={formatDateTime(selected.lastIngestedAtMs)}
                mono
              />
              {selected.diagnosticReason && (
                <EvidenceRow
                  label="Diagnostic"
                  value={selected.diagnosticReason}
                  mono
                />
              )}
            </dl>
            {selected.diagnosticReason && (
              <PageState
                kind="error"
                title="이 Snapshot은 restore source가 아닙니다."
                detail={diagnosticRemediation(selected.diagnosticReason)}
              />
            )}
            {detailsQuery.status === "loading" && !details && (
              <LoadingState label="선택한 Snapshot 상세 목록을 확인하는 중입니다." />
            )}
            <section className="inspector-section">
              <h3>Replica observations</h3>
              {details?.replicas.items.length ? (
                <ul className="replica-list">
                  {details.replicas.items.map((replica) => (
                    <li key={replica.id}>
                      <div>
                        <strong>{replica.destinationKind}</strong>
                        <small className="mono">
                          {replica.destinationFingerprint}
                        </small>
                        <small>
                          retention {formatDateTime(replica.retentionUntilMs)}
                        </small>
                      </div>
                      <div>
                        <StatusBadge
                          label={replica.requirement}
                          tone={
                            replica.requirement === "required"
                              ? "immutable"
                              : "neutral"
                          }
                        />
                        <StatusBadge
                          label={replica.state}
                          tone={toneForState(replica.state)}
                          evidenceAtMs={replica.observedAtMs}
                        />
                        <StatusBadge
                          label={`reader ${replica.readerStatus}`}
                          tone={toneForState(replica.readerStatus)}
                        />
                        <StatusBadge
                          label={replica.immutabilityCapability}
                          tone={toneForState(
                            replica.immutabilityCapability,
                          )}
                        />
                      </div>
                    </li>
                  ))}
                </ul>
              ) : (
                <p className="muted-copy">
                  {details ? "관측된 Replica가 없습니다." : "Replica 목록 확인 중"}
                </p>
              )}
            </section>
            <section className="inspector-section">
              <h3>Required parts</h3>
              {details?.parts.items.length ? (
                <ul className="part-list">
                  {details.parts.items.map((part) => (
                    <li key={part.id}>
                      <span>
                        <strong>{part.logicalName}</strong>
                        <small>
                          {part.role} · {part.dumpFormat} ·{" "}
                          {part.required ? "required" : "optional"}
                        </small>
                        {part.checksumBlake3 && (
                          <small className="mono">
                            {part.checksumBlake3}
                          </small>
                        )}
                      </span>
                      <span className="mono">
                        {formatBytes(part.plaintextBytes)}
                      </span>
                    </li>
                  ))}
                </ul>
              ) : (
                <PageState
                  kind="partial"
                  title={details ? "Part 상세가 없습니다." : "Part 목록 확인 중"}
                  detail="정확한 Part 목록이 확인되기 전에는 복구 가능성을 추정하지 않습니다."
                />
              )}
            </section>
            <section className="inspector-section">
              <h3>Verification evidence</h3>
              {details?.verificationEvidence.items.length ? (
                <ul className="evidence-records">
                  {details.verificationEvidence.items.map((evidence) => (
                    <li key={evidence.id}>
                      <div>
                        <StatusBadge
                          label={`L${evidence.level} ${evidence.result}`}
                          tone={toneForState(evidence.result)}
                          evidenceAtMs={evidence.verifiedAtMs}
                        />
                        <strong>
                          {[evidence.toolName, evidence.toolVersion]
                            .filter(Boolean)
                            .join(" ") || "integrity verifier"}
                        </strong>
                      </div>
                      <small className="mono">
                        {evidence.evidenceChecksumBlake3 ??
                          "evidence checksum unavailable"}
                      </small>
                    </li>
                  ))}
                </ul>
              ) : (
                <p className="muted-copy">
                  {details ? "검증 evidence가 없습니다." : "검증 목록 확인 중"}
                </p>
              )}
            </section>
            <section className="inspector-section">
              <h3>Restore drill history</h3>
              {details?.restoreDrills.items.length ? (
                <ul className="evidence-records">
                  {details.restoreDrills.items.map((drill) => (
                    <li key={drill.restoreRunId}>
                      <div>
                        <StatusBadge
                          label={drill.state}
                          tone={toneForState(drill.state)}
                          evidenceAtMs={drill.completedAtMs}
                        />
                        <strong>
                          {drill.targetConnectionName} · {drill.targetName}
                        </strong>
                      </div>
                      <small>
                        RTO{" "}
                        {drill.measuredRtoMs === null
                          ? "미측정"
                          : `${(drill.measuredRtoMs / 1_000).toFixed(1)}s`}
                        {drill.errorCode ? ` · ${drill.errorCode}` : ""}
                      </small>
                      <small className="mono">
                        Restore {drill.restoreRunId}
                      </small>
                    </li>
                  ))}
                </ul>
              ) : (
                <p className="muted-copy">
                  {details ? "restore drill evidence가 없습니다." : "restore drill 목록 확인 중"}
                </p>
              )}
            </section>
          </Inspector>
        ) : (
          <PageState
            kind="empty"
            title="Snapshot을 선택하세요."
            detail="Replica, Manifest, verification, restore drill 근거를 표시합니다."
          />
        )}
      </div>
    </div>
  );
}

async function loadSnapshotDetails(snapshotId: string): Promise<SnapshotDetails> {
  const [parts, replicas, verificationEvidence, restoreDrills] =
    await Promise.all([
      collectSnapshotSectionPages<SnapshotPart, "parts">(
        snapshotId,
        "parts",
        (cursor) => getSnapshotDetailPage({
          snapshotId,
          section: "parts",
          cursor,
          limit: INVENTORY_PAGE_LIMIT,
        }),
      ),
      collectSnapshotSectionPages<SnapshotReplica, "replicas">(
        snapshotId,
        "replicas",
        (cursor) => getSnapshotDetailPage({
          snapshotId,
          section: "replicas",
          cursor,
          limit: INVENTORY_PAGE_LIMIT,
        }),
      ),
      collectSnapshotSectionPages<VerificationEvidence, "verification_evidence">(
        snapshotId,
        "verification_evidence",
        (cursor) => getSnapshotDetailPage({
          snapshotId,
          section: "verification_evidence",
          cursor,
          limit: INVENTORY_PAGE_LIMIT,
        }),
      ),
      collectSnapshotSectionPages<RestoreDrillEvidence, "restore_drills">(
        snapshotId,
        "restore_drills",
        (cursor) => getSnapshotDetailPage({
          snapshotId,
          section: "restore_drills",
          cursor,
          limit: INVENTORY_PAGE_LIMIT,
        }),
      ),
    ]);
  return {
    snapshotId,
    parts,
    replicas,
    verificationEvidence,
    restoreDrills,
  };
}

function diagnosticRemediation(reason: string): string {
  if (reason === "manifest_unknown_key") {
    return "신뢰 키 등록과 key rotation 이력을 확인한 뒤 read-only Catalog rebuild preview를 실행하세요.";
  }
  if (reason === "manifest_invalid_signature") {
    return "원본 Manifest를 격리하고 Destination과 signing pipeline을 조사하세요.";
  }
  if (reason === "manifest_incomplete") {
    return "누락 Part/Manifest object를 확인하고 적격 Replica에서 repair하세요.";
  }
  if (reason === "manifest_orphaned") {
    return "source identity와 ingest relation을 복구하거나 retention cleanup 검토로 보냅니다.";
  }
  return "Catalog rebuild preview와 Activity 원본으로 ingest 실패 원인을 확인하세요.";
}
