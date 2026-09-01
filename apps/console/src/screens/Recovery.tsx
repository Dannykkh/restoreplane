import { useEffect, useMemo, useRef, useState } from "react";
import { Dialog } from "../components/Dialog";
import { GuidedWorkspace } from "../components/GuidedWorkspace";
import { InventoryTable } from "../components/InventoryTable";
import { LoadingState, PageState } from "../components/PageState";
import { PageHeader } from "../components/PageHeader";
import { StatusBadge, toneForState } from "../components/StatusBadge";
import {
  cancelWork,
  enqueueOverwriteRestore,
  enqueueRepair,
  enqueueSafeRestore,
  enqueueVerification,
  getCatalogRebuildPreview,
  getSnapshotDetailPage,
  getWorkItem,
  listConnections,
  listDestinations,
  listSnapshots,
  preflightRestore,
  prepareRestoreOverwrite,
} from "../lib/api";
import { asServiceError, ServiceError } from "../lib/errors";
import { formatBytes, formatDateTime } from "../lib/format";
import {
  isMutationOutcomeUncertain,
  useMutationAttemptRegistry,
} from "../lib/mutationAttempts";
import { useServiceQuery } from "../lib/queries";
import {
  INVENTORY_MAX_PAGES,
  INVENTORY_PAGE_LIMIT,
  collectCursorPages,
  type CollectedCursorPage,
} from "../lib/pagination";
import {
  restoreRequestFromPreflight,
  restoreTimeline,
  recoveryInventoryAllowsMutation,
  targetConnectionsFor,
} from "../lib/recovery";
import { routeFor, transitionTo } from "../lib/route";
import {
  collectSnapshotSectionPages,
  isExactSnapshotInventory,
  selectCompleteReaderReplica,
  type CollectedSnapshotSection,
  type SnapshotReplicaInventory,
} from "../lib/snapshotInventory";
import type {
  CatalogRebuildPreview,
  ConnectionProfile,
  Destination,
  ReplicaRepairResult,
  RestoreEnqueueResult,
  RestoreMode,
  RestoreOverwriteConfirmation,
  RestorePreflightRequest,
  RestorePreflightResult,
  RestorePurpose,
  RouteState,
  Snapshot,
  SnapshotReplica,
  VerificationEnqueueResult,
  WorkItemStatus,
} from "../lib/types";

const STEPS = ["Recovery Point", "Target", "Service preflight", "Evidence"] as const;
const TERMINAL_WORK_STATES = new Set([
  "succeeded",
  "partially_succeeded",
  "failed",
  "failed_terminal",
  "cancelled",
]);

export function Recovery({ route }: { route: RouteState }) {
  const mutationAttempts = useMutationAttemptRegistry();
  const snapshotsQuery = useServiceQuery<CollectedCursorPage<Snapshot>>(
    "recovery-snapshots",
    () =>
      collectCursorPages(
        (cursor) =>
          listSnapshots({
            cursor,
            limit: INVENTORY_PAGE_LIMIT,
            scope: "complete",
          }),
        INVENTORY_MAX_PAGES,
      ),
  );
  const connectionsQuery = useServiceQuery<CollectedCursorPage<ConnectionProfile>>(
    "recovery-connections",
    () =>
      collectCursorPages(
        (cursor) => listConnections({ cursor, limit: INVENTORY_PAGE_LIMIT }),
        INVENTORY_MAX_PAGES,
      ),
  );
  const destinationsQuery = useServiceQuery<CollectedCursorPage<Destination>>(
    "recovery-destinations",
    () =>
      collectCursorPages(
        (cursor) => listDestinations({ cursor, limit: INVENTORY_PAGE_LIMIT }),
        INVENTORY_MAX_PAGES,
      ),
  );
  const snapshots = snapshotsQuery.data?.items ?? [];
  const selected =
    snapshots.find((snapshot) => snapshot.id === route.selected) ??
    (route.selected ? null : snapshots[0] ?? null);
  const selectedSnapshotId = selected?.id ?? null;
  const replicasQuery = useServiceQuery<
    CollectedSnapshotSection<SnapshotReplica, "replicas"> | null
  >(
    `recovery-replicas:${selectedSnapshotId ?? "none"}`,
    () =>
      selectedSnapshotId
        ? collectSnapshotSectionPages(
            selectedSnapshotId,
            "replicas",
            (cursor) =>
              getSnapshotDetailPage({
                snapshotId: selectedSnapshotId,
                section: "replicas",
                cursor,
                limit: INVENTORY_PAGE_LIMIT,
              }),
          )
        : Promise.resolve(null),
  );
  const [step, setStep] = useState(route.selected ? 1 : 0);
  const [mode, setMode] = useState<RestoreMode>("create_new");
  const [purpose, setPurpose] =
    useState<RestorePurpose>("incident_recovery");
  const [sourceReplicaId, setSourceReplicaId] = useState("");
  const [targetConnectionId, setTargetConnectionId] = useState("");
  const [targetName, setTargetName] = useState("");
  const [preflight, setPreflight] = useState<RestorePreflightResult | null>(
    null,
  );
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<ServiceError | null>(null);
  const [submitted, setSubmitted] = useState<RestoreEnqueueResult | null>(null);
  const resumedWorkItemId = route.action?.startsWith("work:")
    ? route.action.slice("work:".length)
    : null;
  const trackedWorkItemId = submitted?.workItemId ?? resumedWorkItemId;
  const [workStatus, setWorkStatus] = useState<WorkItemStatus | null>(null);
  const [overwriteOpen, setOverwriteOpen] = useState(false);
  const [confirmedTargetName, setConfirmedTargetName] = useState("");
  const [confirmationSummary, setConfirmationSummary] = useState<
    RestoreOverwriteConfirmation["finalSummary"] | null
  >(null);
  const overwriteRequestRef = useRef<Record<string, unknown> | null>(null);
  const [verificationResult, setVerificationResult] =
    useState<VerificationEnqueueResult | null>(null);
  const [repairResult, setRepairResult] =
    useState<ReplicaRepairResult | null>(null);
  const [repairSourceReplicaId, setRepairSourceReplicaId] = useState("");
  const [repairDestinationId, setRepairDestinationId] = useState("");
  const [rebuildDestinationId, setRebuildDestinationId] = useState("");
  const [rebuildJobFilter, setRebuildJobFilter] = useState("");
  const [rebuildPreview, setRebuildPreview] =
    useState<CatalogRebuildPreview | null>(null);

  const snapshotInventoryExact =
    snapshotsQuery.data !== null &&
    typeof snapshotsQuery.data.totalCount === "number" &&
    isExactSnapshotInventory({
      items: snapshotsQuery.data.items,
      nextCursor: snapshotsQuery.data.nextCursor,
      totalCount: snapshotsQuery.data.totalCount,
      complete: snapshotsQuery.data.complete,
    });
  const replicaInventory = useMemo<SnapshotReplicaInventory | null>(() => {
    const inventory = replicasQuery.data;
    if (
      !inventory ||
      inventory.snapshot.id !== selectedSnapshotId ||
      inventory.snapshot.setManifestChecksumBlake3 !==
        selected?.setManifestChecksumBlake3 ||
      typeof inventory.totalCount !== "number"
    ) {
      return null;
    }
    return {
      items: inventory.items,
      nextCursor: inventory.nextCursor,
      totalCount: inventory.totalCount,
      complete: inventory.complete,
    };
  }, [replicasQuery.data, selected?.setManifestChecksumBlake3, selectedSnapshotId]);
  const replicaInventoryExact =
    replicaInventory !== null && isExactSnapshotInventory(replicaInventory);
  const recoveryInventoryComplete =
    selected !== null &&
    snapshotsQuery.status === "ready" &&
    replicasQuery.status === "ready" &&
    connectionsQuery.status === "ready" &&
    destinationsQuery.status === "ready" &&
    recoveryInventoryAllowsMutation(
      snapshotsQuery.data,
      replicaInventory,
      connectionsQuery.data,
      destinationsQuery.data,
    );
  const defaultReaderReplica = selectCompleteReaderReplica(replicaInventory);
  const readerReplicas = useMemo(
    () =>
      replicaInventoryExact
        ? replicaInventory.items.filter(
            (replica) =>
              replica.state === "committed" &&
              replica.readerStatus === "eligible",
          )
        : [],
    [replicaInventory, replicaInventoryExact],
  );
  const sourceReplicaEligible =
    recoveryInventoryComplete &&
    readerReplicas.some((replica) => replica.id === sourceReplicaId);
  const repairSourceReplicaEligible =
    recoveryInventoryComplete &&
    readerReplicas.some((replica) => replica.id === repairSourceReplicaId);
  const confirmationSourceReplicaEligible =
    recoveryInventoryComplete &&
    confirmationSummary !== null &&
    readerReplicas.some(
      (replica) => replica.id === confirmationSummary.source.replicaId,
    );
  const targetConnections = useMemo(
    () =>
      selected
        ? targetConnectionsFor(
            connectionsQuery.data?.items ?? [],
            selected,
            purpose,
          )
        : [],
    [connectionsQuery.data, purpose, selected],
  );
  const repairDestinations = useMemo(
    () => {
      if (!replicaInventoryExact) return [];
      return (destinationsQuery.data?.items ?? []).filter(
        (destination) =>
          destination.state === "ready" &&
          !replicaInventory.items.some((replica) =>
            replica.destinationFingerprint.endsWith(destination.id),
          ),
      );
    },
    [destinationsQuery.data, replicaInventory, replicaInventoryExact],
  );
  const repairDestinationEligible = repairDestinations.some(
    (destination) => destination.id === repairDestinationId,
  );

  useEffect(() => {
    setSourceReplicaId(defaultReaderReplica?.id ?? "");
    setRepairSourceReplicaId(defaultReaderReplica?.id ?? "");
    setTargetName("");
    setPreflight(null);
    setSubmitted(null);
    setWorkStatus(null);
    setError(null);
  }, [defaultReaderReplica?.id, readerReplicas, selectedSnapshotId]);

  useEffect(() => {
    if (
      !targetConnections.some(
        (connection) => connection.id === targetConnectionId,
      )
    ) {
      setTargetConnectionId(targetConnections[0]?.id ?? "");
    }
    setPreflight(null);
    setConfirmationSummary(null);
    overwriteRequestRef.current = null;
  }, [purpose, selected?.id, targetConnections, targetConnectionId]);

  useEffect(() => {
    if (
      !repairDestinations.some(
        (destination) => destination.id === repairDestinationId,
      )
    ) {
      setRepairDestinationId(repairDestinations[0]?.id ?? "");
    }
  }, [repairDestinationId, repairDestinations]);

  useEffect(() => {
    const destinations = destinationsQuery.data?.items ?? [];
    if (
      !destinations.some(
        (destination) => destination.id === rebuildDestinationId,
      )
    ) {
      setRebuildDestinationId(destinations[0]?.id ?? "");
    }
  }, [destinationsQuery.data, rebuildDestinationId]);

  useEffect(() => {
    if (!trackedWorkItemId) return;
    let active = true;
    let timer: number | null = null;
    const refresh = async () => {
      try {
        const work = await getWorkItem(trackedWorkItemId);
        if (!active) return;
        setWorkStatus(work);
        if (!TERMINAL_WORK_STATES.has(work.state)) {
          timer = window.setTimeout(() => void refresh(), 2_000);
        }
      } catch (failure) {
        if (active) setError(asServiceError(failure));
      }
    };
    void refresh();
    return () => {
      active = false;
      if (timer !== null) window.clearTimeout(timer);
    };
  }, [trackedWorkItemId]);

  function invalidatePreparedRestore(): void {
    setPreflight(null);
    setConfirmationSummary(null);
    overwriteRequestRef.current = null;
  }

  function preflightRequest(): RestorePreflightRequest | null {
    if (
      !recoveryInventoryComplete ||
      !selected ||
      !sourceReplicaEligible ||
      !targetConnectionId ||
      !targetName.trim()
    ) {
      return null;
    }
    return {
      snapshotId: selected.id,
      sourceReplicaId,
      mode,
      purpose,
      targetConnectionId,
      targetName: targetName.trim(),
    };
  }

  async function evaluatePreflight(): Promise<RestorePreflightResult | null> {
    if (!recoveryInventoryComplete) return null;
    const request = preflightRequest();
    if (!request) return null;
    setBusy(true);
    setError(null);
    try {
      const result = await preflightRestore(request);
      setPreflight(result);
      setStep(2);
      return result;
    } catch (failure) {
      setError(asServiceError(failure));
      return null;
    } finally {
      setBusy(false);
    }
  }

  async function submitSafe(): Promise<void> {
    if (!recoveryInventoryComplete) return;
    const latest = await evaluatePreflight();
    if (!latest?.eligible || latest.mode !== "create_new") return;
    setBusy(true);
    setError(null);
    try {
      const request = restoreRequestFromPreflight(
        latest,
        latest.purpose === "restore_drill" ? 3 : 2,
      );
      const result = await mutationAttempts.run(
        "restore-enqueue-safe",
        "restore.enqueue_safe",
        request,
        (key) => enqueueSafeRestore(request, key),
      );
      acceptRestore(result);
    } catch (failure) {
      setError(asServiceError(failure));
    } finally {
      setBusy(false);
    }
  }

  async function prepareOverwrite(): Promise<void> {
    if (
      !recoveryInventoryComplete ||
      !selected ||
      confirmedTargetName !== targetName.trim()
    ) {
      return;
    }
    const latest = await evaluatePreflight();
    if (!latest?.eligible || latest.mode !== "overwrite") return;
    setBusy(true);
    setError(null);
    try {
      const request = {
        action: "restore_overwrite",
        snapshotId: latest.source.snapshotId,
        sourceReplicaId: latest.source.replicaId,
        targetConnectionId: latest.targetConnectionId,
        targetName: latest.targetName,
        confirmedTargetName,
        preflightVersion: latest.preflightVersion,
        requiredVerificationLevel: 2,
        ttlMs: 60_000,
      } as const;
      const confirmation = await mutationAttempts.run(
        "restore-overwrite-prepare",
        "confirmation.prepare",
        request,
        (key) => prepareRestoreOverwrite(request, key),
      );
      if (
        !confirmation.tokenAvailable ||
        !confirmation.confirmationToken ||
        !confirmation.overwriteRequest
      ) {
        throw new ServiceError("CONFIRMATION_REPLAYED");
      }
      overwriteRequestRef.current = confirmation.overwriteRequest;
      setConfirmationSummary(confirmation.finalSummary);
    } catch (failure) {
      overwriteRequestRef.current = null;
      setError(asServiceError(failure));
    } finally {
      setBusy(false);
    }
  }

  async function executeOverwrite(): Promise<void> {
    if (!confirmationSourceReplicaEligible) return;
    const request = overwriteRequestRef.current;
    if (!request || !confirmationSummary) return;
    setBusy(true);
    setError(null);
    let clearPreparedRequest = false;
    try {
      const result = await mutationAttempts.run(
        "restore-enqueue-overwrite",
        "restore.enqueue_overwrite",
        request,
        (key) => enqueueOverwriteRestore(request, key),
      );
      acceptRestore(result);
      setOverwriteOpen(false);
      clearPreparedRequest = true;
    } catch (failure) {
      const serviceError = asServiceError(failure);
      clearPreparedRequest = serviceError.requestId !== null
        && !isMutationOutcomeUncertain(serviceError);
      setError(serviceError);
    } finally {
      if (clearPreparedRequest) {
        overwriteRequestRef.current = null;
        setConfirmationSummary(null);
        setConfirmedTargetName("");
      }
      setBusy(false);
    }
  }

  function acceptRestore(result: RestoreEnqueueResult): void {
    setSubmitted(result);
    setStep(3);
    transitionTo(
      {
        page: "recovery",
        selected: selected?.id,
        action: `work:${result.workItemId}`,
      },
      true,
    );
  }

  async function verify(level: 1 | 2 | 3): Promise<void> {
    if (!sourceReplicaEligible || !selected) return;
    setBusy(true);
    setError(null);
    try {
      const payload = {
        snapshotId: selected.id,
        level,
        purpose: "manual",
        sourceReplicaId,
      };
      setVerificationResult(await mutationAttempts.run(
        `recovery-verification:${selected.id}`,
        "verification.enqueue",
        payload,
        (key) => enqueueVerification(selected.id, level, sourceReplicaId, key),
      ));
    } catch (failure) {
      setError(asServiceError(failure));
    } finally {
      setBusy(false);
    }
  }

  async function repairReplica(): Promise<void> {
    if (
      !recoveryInventoryComplete ||
      !selected ||
      !repairSourceReplicaEligible ||
      !repairDestinationEligible
    ) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const payload = {
        snapshotId: selected.id,
        sourceReplicaId: repairSourceReplicaId,
        targetDestinationId: repairDestinationId,
      };
      setRepairResult(await mutationAttempts.run(
        `replica-repair:${selected.id}`,
        "repair.enqueue",
        payload,
        (key) => enqueueRepair(
          selected.id,
          repairSourceReplicaId,
          repairDestinationId,
          key,
        ),
      ));
    } catch (failure) {
      setError(asServiceError(failure));
    } finally {
      setBusy(false);
    }
  }

  async function inspectCatalog(): Promise<void> {
    if (!rebuildDestinationId) return;
    setBusy(true);
    setError(null);
    try {
      setRebuildPreview(
        await getCatalogRebuildPreview(
          rebuildDestinationId,
          rebuildJobFilter.trim() || undefined,
        ),
      );
    } catch (failure) {
      setError(asServiceError(failure));
    } finally {
      setBusy(false);
    }
  }

  async function requestCancel(): Promise<void> {
    if (!trackedWorkItemId || !workStatus) return;
    setBusy(true);
    setError(null);
    try {
      const payload = {
        workItemId: trackedWorkItemId,
        fencingToken: workStatus.fencingToken,
      };
      await mutationAttempts.run(
        `work-cancel:${trackedWorkItemId}`,
        "work.cancel",
        payload,
        (key) => cancelWork(trackedWorkItemId, workStatus.fencingToken, key),
      );
      setWorkStatus(await getWorkItem(trackedWorkItemId));
    } catch (failure) {
      setError(asServiceError(failure));
    } finally {
      setBusy(false);
    }
  }

  const loading =
    (snapshotsQuery.status === "loading" && !snapshotsQuery.data) ||
    (connectionsQuery.status === "loading" && !connectionsQuery.data) ||
    (destinationsQuery.status === "loading" && !destinationsQuery.data) ||
    (selected !== null &&
      replicasQuery.status === "loading" &&
      !replicasQuery.data);
  if (loading) {
    return (
      <LoadingState label="Recovery Point, target, Destination 근거를 조회하는 중입니다." />
    );
  }
  if (!snapshotsQuery.data || !connectionsQuery.data || !destinationsQuery.data) {
    const queryError =
      snapshotsQuery.error ?? connectionsQuery.error ?? destinationsQuery.error;
    return (
      <PageState
        kind="error"
        title="Recovery workspace를 열 수 없습니다."
        detail="source, target, reader 근거 중 하나라도 없으면 복구 입력을 추정하지 않습니다."
        error={queryError}
        action={
          <button
            type="button"
            className="button button-secondary"
            onClick={() => {
              snapshotsQuery.refresh();
              connectionsQuery.refresh();
              destinationsQuery.refresh();
            }}
          >
            다시 조회
          </button>
        }
      />
    );
  }

  const preflightBlocked =
    !recoveryInventoryComplete || !preflight?.eligible;
  const currentConnection = targetConnections.find(
    (connection) => connection.id === targetConnectionId,
  );
  const timeline = restoreTimeline(workStatus);

  return (
    <div className="page-stack">
      <PageHeader
        eyebrow="Evidence-bound restore"
        title="Recovery"
        description="Recovery Point에서 시작해 서비스가 source, target, tool, space, permission을 다시 판정합니다. 새 대상이 기본이며 overwrite는 R-OWNER 확인 토큰을 요구합니다."
        actions={
          <a className="button button-secondary" href={routeFor("snapshots")}>
            Snapshots 보기
          </a>
        }
      />

      {(snapshotsQuery.status === "error" ||
        connectionsQuery.status === "error" ||
        destinationsQuery.status === "error" ||
        replicasQuery.status === "error") && (
        <PageState
          kind="stale"
          title="마지막 Recovery 근거를 표시합니다."
          detail="새 복구 요청은 Snapshot, Replica, Connection, Destination query가 모두 최신 상태가 될 때까지 실행하지 마세요."
          error={
            snapshotsQuery.error ??
            connectionsQuery.error ??
            destinationsQuery.error ??
            replicasQuery.error
          }
        />
      )}
      {snapshotsQuery.data &&
        (!snapshotInventoryExact ||
          connectionsQuery.data?.complete === false ||
          destinationsQuery.data?.complete === false ||
          snapshotsQuery.status !== "ready" ||
          (selected !== null &&
            (!replicaInventoryExact || replicasQuery.status !== "ready"))) && (
          <PageState
            kind="partial"
            title="복구 인벤토리가 완전하지 않습니다."
            detail="Snapshot, 선택한 Replica, Connection, Destination 목록이 정확한 전체 수량으로 확인될 때까지 preflight, 복구, 검증, Replica repair를 실행하지 않습니다."
            action={
              <button
                type="button"
                className="button button-secondary"
                onClick={() => {
                  snapshotsQuery.refresh();
                  replicasQuery.refresh();
                  connectionsQuery.refresh();
                  destinationsQuery.refresh();
                }}
              >
                인벤토리 다시 조회
              </button>
            }
          />
        )}
      {error && (
        <PageState
          kind={
            error.code === "APPLICATION_FORBIDDEN" ? "permission" : "error"
          }
          title={error.message}
          detail="source와 target 선택은 아래에 고정해 두었습니다. 실패가 복구 성공으로 바뀌지 않았습니다."
          error={error}
        />
      )}

      <GuidedWorkspace
        title={
          purpose === "restore_drill"
            ? "격리 restore drill"
            : mode === "overwrite"
              ? "소유자 승인 overwrite"
              : "새 대상으로 안전 복구"
        }
        description="각 단계는 backend의 현재 evidence를 다시 읽으며, 입력이 바뀌면 이전 preflight와 confirmation은 폐기됩니다."
        steps={STEPS}
        currentStep={step}
        footer={
          <>
            <button
              type="button"
              className="button button-secondary"
              disabled={step === 0 || step === 3 || busy}
              onClick={() => setStep((value) => Math.max(0, value - 1))}
            >
              이전
            </button>
            {step === 0 && (
              <button
                type="button"
                className="button button-primary"
                disabled={!selected || !recoveryInventoryComplete}
                onClick={() => setStep(1)}
              >
                Target 선택
              </button>
            )}
            {step === 1 && (
              <button
                type="button"
                className="button button-primary"
                disabled={
                  busy ||
                  !recoveryInventoryComplete ||
                  !selected ||
                  !sourceReplicaEligible ||
                  !targetConnectionId ||
                  !targetName.trim()
                }
                onClick={() => void evaluatePreflight()}
              >
                {busy ? "서비스 판정 중" : "서비스 preflight 실행"}
              </button>
            )}
            {step === 2 && mode === "create_new" && (
              <button
                type="button"
                className="button button-primary"
                disabled={preflightBlocked || busy || !recoveryInventoryComplete}
                onClick={() => void submitSafe()}
              >
                {busy
                  ? "현재 근거 재확인 중"
                  : purpose === "restore_drill"
                    ? "L3 restore drill 요청"
                    : "새 대상 복구 요청"}
              </button>
            )}
            {step === 2 && mode === "overwrite" && (
              <button
                type="button"
                className="button button-danger"
                disabled={preflightBlocked || busy || !recoveryInventoryComplete}
                onClick={() => {
                  setConfirmedTargetName("");
                  setConfirmationSummary(null);
                  overwriteRequestRef.current = null;
                  setOverwriteOpen(true);
                }}
              >
                R-OWNER overwrite 확인
              </button>
            )}
            {step === 3 && trackedWorkItemId && (
              <a
                className="button button-primary"
                href={routeFor("activity", { selected: trackedWorkItemId })}
              >
                Activity 원본 보기
              </a>
            )}
          </>
        }
      >
        <div className="live-region" aria-live="polite">
          {trackedWorkItemId
            ? `Work Item ${trackedWorkItemId}, ${workStatus?.state ?? "조회 중"}`
            : `단계 ${step + 1} / ${STEPS.length}`}
        </div>

        {selected && step > 0 && (
          <div
            className="recovery-boundaries"
            aria-label="고정된 복구 source와 target"
          >
            <section>
              <span className="eyebrow">Source snapshot</span>
              <strong>{selected.sourceDisplayName}</strong>
              <small className="mono">{selected.id}</small>
              <StatusBadge
                label={selected.restoreEligibility}
                tone={toneForState(selected.restoreEligibility)}
              />
            </section>
            <section>
              <span className="eyebrow">Target boundary</span>
              <strong>{currentConnection?.displayName ?? "선택 필요"}</strong>
              <small className="mono">{targetName || "이름 입력 필요"}</small>
              <StatusBadge
                label={mode}
                tone={mode === "overwrite" ? "danger" : "info"}
              />
            </section>
          </div>
        )}

        {step === 0 && (
          <InventoryTable
            ariaLabel="Recovery Point selection"
            rows={snapshots}
            rowKey={(row) => row.id}
            selectedId={selected?.id ?? null}
            onSelect={(row) =>
              transitionTo({ page: "recovery", selected: row.id }, true)
            }
            emptyMessage="complete Recovery Point가 없습니다. Diagnostics에서 ingest 문제를 확인하세요."
            columns={[
              {
                key: "source",
                header: "Recovery Point",
                render: (row) => (
                  <span className="resource-name">
                    <strong>{row.sourceDisplayName}</strong>
                    <small>
                      {row.engine} · {formatDateTime(row.committedAtMs)}
                    </small>
                  </span>
                ),
              },
              {
                key: "eligibility",
                header: "Eligibility",
                render: (row) => (
                  <StatusBadge
                    label={row.restoreEligibility}
                    tone={toneForState(row.restoreEligibility)}
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
                        : "none"
                    }
                    tone={row.highestVerifiedLevel ? "success" : "warning"}
                    evidenceAtMs={row.lastVerifiedAtMs}
                  />
                ),
              },
              {
                key: "replica",
                header: "Replica",
                render: (row) => (
                  <StatusBadge
                    label={row.replicaDurability}
                    tone={toneForState(row.replicaDurability)}
                  />
                ),
              },
            ]}
          />
        )}

        {step === 1 && selected && (
          <div className="recovery-target-form">
            <fieldset className="choice-fieldset">
              <legend>작업 목적</legend>
              <label>
                <input
                  type="radio"
                  name="restorePurpose"
                  value="incident_recovery"
                  checked={purpose === "incident_recovery"}
                  onChange={() => {
                    setPurpose("incident_recovery");
                    invalidatePreparedRestore();
                  }}
                />
                <span>
                  <strong>Incident recovery</strong>
                  create-new 또는 R-OWNER overwrite
                </span>
              </label>
              <label>
                <input
                  type="radio"
                  name="restorePurpose"
                  value="restore_drill"
                  checked={purpose === "restore_drill"}
                  onChange={() => {
                    setPurpose("restore_drill");
                    setMode("create_new");
                    invalidatePreparedRestore();
                  }}
                />
                <span>
                  <strong>L3 restore drill</strong>
                  등록된 격리 restore-test target에서만 실행
                </span>
              </label>
            </fieldset>
            <fieldset className="choice-fieldset">
              <legend>복구 mode</legend>
              <label>
                <input
                  type="radio"
                  name="restoreMode"
                  value="create_new"
                  checked={mode === "create_new"}
                  onChange={() => {
                    setMode("create_new");
                    invalidatePreparedRestore();
                  }}
                />
                <span>
                  <strong>새 Database 또는 파일</strong>
                  기존 target을 건드리지 않는 기본 경로
                </span>
              </label>
              <label>
                <input
                  type="radio"
                  name="restoreMode"
                  value="overwrite"
                  checked={mode === "overwrite"}
                  disabled={purpose === "restore_drill"}
                  onChange={() => {
                    setMode("overwrite");
                    invalidatePreparedRestore();
                  }}
                />
                <span>
                  <strong>기존 Database overwrite</strong>
                  R-OWNER, target 재입력, 짧은 수명의 bound token 필요
                </span>
              </label>
            </fieldset>
            <div className="form-grid">
              <label className="field">
                <span>Source Replica</span>
                <select
                  name="sourceReplica"
                  value={sourceReplicaId}
                  disabled={!recoveryInventoryComplete}
                  onChange={(event) => {
                    setSourceReplicaId(event.target.value);
                    invalidatePreparedRestore();
                  }}
                >
                  {readerReplicas.map((replica) => (
                    <option key={replica.id} value={replica.id}>
                      {replica.destinationKind} · {replica.requirement} ·{" "}
                      {replica.immutabilityCapability}
                    </option>
                  ))}
                </select>
                <small>committed + reader eligible Replica만 표시합니다.</small>
              </label>
              <label className="field">
                <span>Target Connection</span>
                <select
                  name="targetConnection"
                  value={targetConnectionId}
                  disabled={!recoveryInventoryComplete}
                  onChange={(event) => {
                    setTargetConnectionId(event.target.value);
                    invalidatePreparedRestore();
                  }}
                >
                  {targetConnections.map((connection) => (
                    <option key={connection.id} value={connection.id}>
                      {connection.displayName}
                      {connection.restoreTestTarget ? " · restore-test" : ""}
                    </option>
                  ))}
                </select>
                <small>
                  같은 engine만 표시하며 drill은 restore-test target만 표시합니다.
                </small>
              </label>
            </div>
            {!targetConnections.length && (
              <PageState
                kind="error"
                title="사용 가능한 target Connection이 없습니다."
                detail={
                  purpose === "restore_drill"
                    ? "같은 engine의 격리 restore-test Connection을 먼저 등록하고 probe하세요."
                    : "같은 engine의 target Connection을 먼저 등록하세요."
                }
              />
            )}
            <label className="field">
              <span>
                {mode === "overwrite"
                  ? "존재하는 target database 이름"
                  : "새 database 또는 파일 이름/경로"}
              </span>
              <input
                required
                name="restoreTarget"
                value={targetName}
                onChange={(event) => {
                  setTargetName(event.target.value);
                  invalidatePreparedRestore();
                }}
                placeholder={
                  mode === "overwrite"
                    ? "orders"
                    : selected.engine === "sqlite"
                      ? "D:\\Restore\\orders-restored.db"
                      : "orders_restore_20260729"
                }
                autoComplete="off"
                aria-describedby="restore-target-description"
              />
              <small id="restore-target-description">
                서비스가 server identity, 존재 여부, system database 정책을 현재
                probe evidence로 판정합니다.
              </small>
            </label>
          </div>
        )}

        {step === 2 && selected && preflight && (
          <div className="preflight-grid">
            <section>
              <div className="section-heading">
                <div>
                  <span className="eyebrow">Authoritative checks</span>
                  <h3>서비스 preflight</h3>
                </div>
                <StatusBadge
                  label={preflight.eligible ? "eligible" : "blocked"}
                  tone={preflight.eligible ? "success" : "danger"}
                />
              </div>
              <ul className="preflight-list">
                {preflight.checks.map((item) => (
                  <li key={item.key}>
                    <div>
                      <strong>{preflightLabel(item.key)}</strong>
                      <small>{item.detail}</small>
                      {item.state !== "passed" && (
                        <small className="remediation">
                          조치: {item.remediation}
                        </small>
                      )}
                    </div>
                    <StatusBadge
                      label={item.state}
                      tone={toneForState(item.state)}
                    />
                  </li>
                ))}
              </ul>
            </section>
            <section className="preflight-summary" aria-label="Preflight summary">
              <h3>판정 경계</h3>
              <dl>
                <div>
                  <dt>Source</dt>
                  <dd className="mono">{preflight.source.snapshotId}</dd>
                </div>
                <div>
                  <dt>Replica</dt>
                  <dd className="mono">{preflight.source.replicaId}</dd>
                </div>
                <div>
                  <dt>Target</dt>
                  <dd>
                    {preflight.targetConnectionName}
                    <small className="mono">{preflight.targetName}</small>
                  </dd>
                </div>
                <div>
                  <dt>Target exists</dt>
                  <dd>{String(preflight.targetExists)}</dd>
                </div>
                <div>
                  <dt>Target role</dt>
                  <dd>{preflight.targetRole}</dd>
                </div>
                <div>
                  <dt>Required spool</dt>
                  <dd>{formatBytes(preflight.requiredSpoolBytes)}</dd>
                </div>
                <div>
                  <dt>Available spool</dt>
                  <dd>{formatBytes(preflight.availableSpoolBytes)}</dd>
                </div>
                <div>
                  <dt>Expires</dt>
                  <dd>{formatDateTime(preflight.expiresAtMs)}</dd>
                </div>
              </dl>
              {!preflight.eligible && (
                <PageState
                  kind="error"
                  title="복구 process를 시작하지 않습니다."
                  detail="failed check를 해결한 뒤 현재 근거로 preflight를 다시 실행하세요."
                />
              )}
            </section>
          </div>
        )}

        {step === 3 && trackedWorkItemId && (
          <div className="restore-evidence">
            <div className="section-heading">
              <div>
                <span className="eyebrow">Durable execution</span>
                <h3>복구 실행 근거</h3>
              </div>
              <StatusBadge
                label={workStatus?.state ?? "loading"}
                tone={toneForState(workStatus?.state ?? "unknown")}
              />
            </div>
            <p>
              요청 접수나 native process exit 0만으로 성공을 표시하지 않습니다.
              validation과 terminal evidence가 모두 필요합니다.
            </p>
            <ol className="restore-timeline">
              {timeline.map((entry) => (
                <li key={entry.key} data-state={entry.state}>
                  <span className="timeline-rail" aria-hidden="true" />
                  <div>
                    <strong>{entry.label}</strong>
                    <small>{entry.detail}</small>
                  </div>
                  <StatusBadge
                    label={entry.state}
                    tone={
                      entry.state === "failed"
                        ? "danger"
                        : entry.state === "attention"
                          ? "warning"
                          : entry.state === "complete"
                            ? "success"
                            : "neutral"
                    }
                  />
                </li>
              ))}
            </ol>
            <dl className="evidence-list">
              <div>
                <dt>Work Item</dt>
                <dd className="mono">{trackedWorkItemId}</dd>
              </div>
              <div>
                <dt>Restore Run</dt>
                <dd className="mono">
                  {submitted?.restoreRunId ?? "Work Item에서 복구 중"}
                </dd>
              </div>
              <div>
                <dt>Validation</dt>
                <dd>{String(workStatus?.result?.validation ?? "pending")}</dd>
              </div>
              <div>
                <dt>Cleanup required</dt>
                <dd>
                  {String(workStatus?.result?.cleanupRequired ?? "pending")}
                </dd>
              </div>
            </dl>
            {workStatus && !TERMINAL_WORK_STATES.has(workStatus.state) && (
              <button
                type="button"
                className="button button-secondary"
                disabled={busy}
                onClick={() => void requestCancel()}
              >
                복구 작업 취소 요청
              </button>
            )}
          </div>
        )}
      </GuidedWorkspace>

      {selected && (
        <section className="recovery-operations" aria-labelledby="recovery-operations-title">
          <header className="section-heading">
            <div>
              <span className="eyebrow">Evidence operations</span>
              <h2 id="recovery-operations-title">Verify, repair, rebuild</h2>
            </div>
            <span className="muted-copy">
              선택한 {selected.sourceDisplayName} Recovery Point
            </span>
          </header>
          <div className="recovery-operation-grid">
            <section>
              <h3>Exact-level verification</h3>
              <p>
                요청 레벨을 지원하지 않으면 낮은 레벨로 바꾸지 않고 unsupported로
                기록합니다.
              </p>
              <div className="button-row" aria-label="Manual verification levels">
                {[1, 2, 3].map((level) => (
                  <button
                    key={level}
                    type="button"
                    className="button button-secondary"
                    disabled={
                      busy || !sourceReplicaEligible
                    }
                    onClick={() => void verify(level as 1 | 2 | 3)}
                  >
                    L{level} 요청
                  </button>
                ))}
              </div>
              {verificationResult && (
                <div className="operation-result" aria-live="polite">
                  <StatusBadge
                    label={verificationResult.state}
                    tone={toneForState(verificationResult.state)}
                  />
                  <strong>L{verificationResult.requestedLevel}</strong>
                  <small>
                    {verificationResult.errorCode ??
                      verificationResult.limitation ??
                      verificationResult.workItemId}
                  </small>
                </div>
              )}
            </section>
            <section>
              <h3>Replica repair</h3>
              <p>
                적격 source에서 아직 이 Recovery Point가 없는 ready Destination으로
                복제합니다.
              </p>
              <label className="field">
                <span>Repair source</span>
                <select
                  name="repairSourceReplica"
                  value={repairSourceReplicaId}
                  disabled={!recoveryInventoryComplete}
                  onChange={(event) =>
                    setRepairSourceReplicaId(event.target.value)
                  }
                >
                  {readerReplicas.map((replica) => (
                    <option key={replica.id} value={replica.id}>
                      {replica.destinationKind} · {replica.id}
                    </option>
                  ))}
                </select>
              </label>
              <label className="field">
                <span>Target Destination</span>
                <select
                  name="repairDestination"
                  value={repairDestinationId}
                  disabled={!recoveryInventoryComplete}
                  onChange={(event) =>
                    setRepairDestinationId(event.target.value)
                  }
                >
                  {repairDestinations.map((destination) => (
                    <option key={destination.id} value={destination.id}>
                      {destination.displayName} · {destination.kind}
                    </option>
                  ))}
                </select>
              </label>
              <button
                type="button"
                className="button button-secondary"
                disabled={
                  busy ||
                  !recoveryInventoryComplete ||
                  !repairSourceReplicaEligible ||
                  !repairDestinationEligible
                }
                onClick={() => void repairReplica()}
              >
                Repair enqueue
              </button>
              {repairResult && (
                <div className="operation-result" aria-live="polite">
                  <StatusBadge label={repairResult.state} tone="neutral" />
                  <small className="mono">{repairResult.workItemId}</small>
                </div>
              )}
            </section>
            <section>
              <h3>Catalog rebuild preview</h3>
              <p>
                Destination의 signed Manifest를 읽고 apply 전에 accepted, rejected,
                duplicate, unknown key를 분리합니다.
              </p>
              <label className="field">
                <span>Manifest Destination</span>
                <select
                  name="rebuildDestination"
                  value={rebuildDestinationId}
                  onChange={(event) => {
                    setRebuildDestinationId(event.target.value);
                    setRebuildPreview(null);
                  }}
                >
                  {destinationsQuery.data.items.map((destination) => (
                    <option key={destination.id} value={destination.id}>
                      {destination.displayName} · {destination.kind}
                    </option>
                  ))}
                </select>
              </label>
              <label className="field">
                <span>Job prefix filter</span>
                <input
                  name="rebuildJobFilter"
                  value={rebuildJobFilter}
                  onChange={(event) => setRebuildJobFilter(event.target.value)}
                  placeholder="선택 사항"
                  autoComplete="off"
                />
              </label>
              <button
                type="button"
                className="button button-secondary"
                disabled={busy || !rebuildDestinationId}
                onClick={() => void inspectCatalog()}
              >
                Read-only preview
              </button>
              {rebuildPreview && (
                <CatalogPreviewResult preview={rebuildPreview} />
              )}
            </section>
          </div>
        </section>
      )}

      {overwriteOpen && selected && preflight && (
        <Dialog
          title="기존 target overwrite"
          description="source와 target을 다시 읽고, target identity와 결합된 1회성 확인 토큰으로만 실행합니다."
          onClose={() => {
            overwriteRequestRef.current = null;
            setConfirmationSummary(null);
            setConfirmedTargetName("");
            setOverwriteOpen(false);
          }}
        >
          <div className="destructive-confirmation">
            <div className="confirmation-boundaries">
              <section>
                <span className="eyebrow">Source preserved</span>
                <strong>{selected.sourceDisplayName}</strong>
                <small className="mono">{preflight.source.snapshotId}</small>
              </section>
              <section>
                <span className="eyebrow">Target replaced</span>
                <strong>{preflight.targetConnectionName}</strong>
                <small className="mono">{preflight.targetName}</small>
              </section>
            </div>
            <PageState
              kind="error"
              title="기존 database 내용이 교체됩니다."
              detail="실패 또는 취소 시 target을 자동 삭제하지 않습니다. cleanup required 상태와 validation evidence를 확인해야 합니다."
            />
            {!confirmationSummary ? (
              <>
                <label className="field">
                  <span>
                    계속하려면 target 이름 “{preflight.targetName}”을 다시 입력
                  </span>
                  <input
                    autoFocus
                    name="confirmedTargetName"
                    value={confirmedTargetName}
                    onChange={(event) =>
                      setConfirmedTargetName(event.target.value)
                    }
                    autoComplete="off"
                    aria-describedby="overwrite-target-description"
                  />
                  <small id="overwrite-target-description">
                    입력은 대소문자까지 target identity와 정확히 일치해야 합니다.
                  </small>
                </label>
                <div className="dialog-actions">
                  <button
                    type="button"
                    className="button button-secondary"
                    onClick={() => setOverwriteOpen(false)}
                  >
                    취소
                  </button>
                  <button
                    type="button"
                    className="button button-danger"
                    disabled={
                      busy ||
                      !recoveryInventoryComplete ||
                      confirmedTargetName !== preflight.targetName
                    }
                    onClick={() => void prepareOverwrite()}
                  >
                    {busy ? "현재 identity 재확인 중" : "1회성 확인 준비"}
                  </button>
                </div>
              </>
            ) : (
              <>
                <section className="final-summary" aria-label="Service final summary">
                  <h3>Service final summary</h3>
                  <dl>
                    <div>
                      <dt>Source</dt>
                      <dd className="mono">
                        {confirmationSummary.source.snapshotId}
                      </dd>
                    </div>
                    <div>
                      <dt>Target</dt>
                      <dd className="mono">
                        {confirmationSummary.target.name}
                      </dd>
                    </div>
                    <div>
                      <dt>Effect</dt>
                      <dd>{confirmationSummary.destructiveEffect}</dd>
                    </div>
                    <div>
                      <dt>Cleanup</dt>
                      <dd>{confirmationSummary.cleanupPolicy}</dd>
                    </div>
                  </dl>
                </section>
                <div className="dialog-actions">
                  <button
                    type="button"
                    className="button button-secondary"
                    onClick={() => {
                      overwriteRequestRef.current = null;
                      setConfirmationSummary(null);
                    }}
                  >
                    토큰 폐기
                  </button>
                  <button
                    type="button"
                    className="button button-danger"
                    disabled={busy || !confirmationSourceReplicaEligible}
                    onClick={() => void executeOverwrite()}
                  >
                    {busy ? "요청 중" : "기존 target overwrite 실행"}
                  </button>
                </div>
              </>
            )}
          </div>
        </Dialog>
      )}
    </div>
  );
}

function CatalogPreviewResult({
  preview,
}: {
  preview: CatalogRebuildPreview;
}) {
  if (!preview.available) {
    return (
      <PageState
        kind="error"
        title="Catalog preview를 사용할 수 없습니다."
        detail={preview.reasonCode ?? "source unavailable"}
      />
    );
  }
  return (
    <div className="catalog-preview-result" aria-live="polite">
      <dl>
        <div>
          <dt>Accepted</dt>
          <dd>{preview.acceptedCount ?? 0}</dd>
        </div>
        <div>
          <dt>Rejected</dt>
          <dd>{preview.rejectedCount ?? 0}</dd>
        </div>
        <div>
          <dt>Duplicate</dt>
          <dd>{preview.duplicateCount ?? 0}</dd>
        </div>
        <div>
          <dt>Unknown key</dt>
          <dd>{preview.unknownKeyCount ?? 0}</dd>
        </div>
      </dl>
      <ul>
        {preview.candidates.map((candidate) => (
          <li key={candidate.objectKey}>
            <StatusBadge
              label={candidate.disposition}
              tone={toneForState(candidate.disposition)}
            />
            <span className="mono" title={candidate.objectKey}>
              {candidate.objectKey}
            </span>
            <small>
              {candidate.reasonCode ??
                candidate.signingKeyId ??
                candidate.recoveryPointId}
            </small>
          </li>
        ))}
      </ul>
      <p>
        Preview는 catalog projection을 변경하지 않습니다. 적용은 별도 durable
        rebuild 승인 경계입니다.
      </p>
    </div>
  );
}

function preflightLabel(key: string): string {
  return (
    {
      source_snapshot: "Source snapshot",
      source_replica: "Source Replica",
      tool_and_validation: "Tool and validation",
      space: "Spool capacity",
      permission: "Target permission",
      connection: "Connection probe",
      server_version: "Server version",
      target_existence: "Target existence",
      engine_policy: "Engine policy",
      restore_target_role: "Restore target role",
    }[key] ?? key
  );
}
