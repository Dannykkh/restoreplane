import { useMemo, useRef, useState } from "react";
import { publishPlan, savePlan } from "../lib/api";
import { asServiceError, ServiceError } from "../lib/errors";
import {
  mutationSignatureDigest,
  useMutationAttemptRegistry,
} from "../lib/mutationAttempts";
import type {
  BackupScope,
  BackupPlanSaveRequest,
  BackupScopeMode,
  DatabaseTarget,
  Destination,
  EngineCapability,
  EngineMatrix,
  EngineMatrixRow,
} from "../lib/types";
import { CapabilityMatrix, type CapabilityItem } from "./CapabilityMatrix";
import { GuidedWorkspace } from "./GuidedWorkspace";
import { PageState } from "./PageState";
import { ScopeCoverage } from "./ScopeCoverage";
import { StatusBadge, toneForState } from "./StatusBadge";

const STEPS = ["Sources", "Destinations", "Schedule / RPO", "Retention / Verify", "Preflight"] as const;

interface PlanScopeSource {
  id: string;
  databaseTargetId: string;
  databaseName: string;
  connectionName: string;
  engine: string;
  displayName: string;
  mode: BackupScopeMode;
  state: BackupScope["state"];
  selectedObjectCount: number | null;
  eligibleObjectCount: number | null;
  newObjectCount: number | null;
  missingObjectCount: number | null;
  protectionCompliance: BackupScope["protectionCompliance"] | "partial";
}

interface BackupPlanWizardProps {
  targets: readonly DatabaseTarget[];
  scopes?: readonly BackupScope[];
  destinations: readonly Destination[];
  engineMatrix: EngineMatrix;
  preselectedTargetId?: string;
  preselectedScopeId?: string;
  onCancel: () => void;
  onComplete: (workItemId: string) => void;
}

export function BackupPlanWizard({
  scopes,
  destinations,
  engineMatrix,
  preselectedTargetId,
  preselectedScopeId,
  onCancel,
  onComplete,
}: BackupPlanWizardProps) {
  const mutationAttempts = useMutationAttemptRegistry();
  const savedDraft = useRef<{
    signatureDigest: string;
    planId: string;
    revisionId: string;
  } | null>(null);
  const availableScopes = useMemo<PlanScopeSource[]>(
    () => (scopes ?? []).map(scopeSource),
    [scopes],
  );
  const initiallySelectedScopeId =
    preselectedScopeId ??
    availableScopes.find((scope) => scope.databaseTargetId === preselectedTargetId)?.id;
  const [step, setStep] = useState(0);
  const [name, setName] = useState("");
  const [selectedScopes, setSelectedScopes] = useState<string[]>(
    initiallySelectedScopeId ? [initiallySelectedScopeId] : [],
  );
  const [selectedDestinations, setSelectedDestinations] = useState<string[]>([]);
  const [schedule, setSchedule] = useState("15 * * * *");
  const [timezone, setTimezone] = useState("Asia/Seoul");
  const [rpoHours, setRpoHours] = useState(2);
  const [retentionDaily, setRetentionDaily] = useState(30);
  const [retentionWeekly, setRetentionWeekly] = useState(8);
  const [retentionMonthly, setRetentionMonthly] = useState(12);
  const [verifyLevel, setVerifyLevel] = useState<1 | 2 | 3>(2);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<ServiceError | null>(null);

  const selectedScopeRows = availableScopes.filter((scope) => selectedScopes.includes(scope.id));
  const selectedDestinationRows = destinations.filter((destination) =>
    selectedDestinations.includes(destination.id),
  );
  const capabilities = useMemo<CapabilityItem[]>(() => {
    const engineModes = new Map(
      selectedScopeRows.map((scope) => [`${scope.engine}:${scope.mode}`, scope] as const),
    );
    return Array.from(engineModes.values()).flatMap((scope) =>
      engineCapabilitiesFromMatrix(scope.engine, verifyLevel, engineMatrix, scope.mode),
    );
  }, [engineMatrix, selectedScopeRows, verifyLevel]);
  const blockingCapability = capabilities.some(
    (item) =>
      item.status === "experimental" ||
      item.status === "unsupported" ||
      item.status === "unknown",
  );
  const missingReader = selectedDestinationRows.some(
    (destination) =>
      destination.roles.find((role) => role.role === "restore_reader")?.probeStatus !== "passed",
  );

  function toggle(values: string[], id: string, setter: (next: string[]) => void): void {
    setter(values.includes(id) ? values.filter((value) => value !== id) : [...values, id]);
  }

  function toggleScope(scope: PlanScopeSource): void {
    setSelectedScopes((current) => {
      if (current.includes(scope.id)) {
        return current.filter((id) => id !== scope.id);
      }
      const sameDatabaseScopeIds = new Set(
        availableScopes
          .filter((candidate) => candidate.databaseTargetId === scope.databaseTargetId)
          .map((candidate) => candidate.id),
      );
      return [...current.filter((id) => !sameDatabaseScopeIds.has(id)), scope.id];
    });
  }

  async function publish(): Promise<void> {
    setBusy(true);
    setError(null);
    try {
      const payload: BackupPlanSaveRequest = {
        displayName: name,
        scopeIds: selectedScopes,
        destinations: selectedDestinations.map((destinationId) => ({
          destinationId,
          requirement: "required",
        })),
        schedule: { kind: "cron", expression: schedule, timezone },
        rpoMs: rpoHours * 3_600_000,
        retention: {
          daily: retentionDaily,
          weekly: retentionWeekly,
          monthly: retentionMonthly,
        },
        verification: { requestedLevel: verifyLevel },
      };
      const signatureDigest = mutationSignatureDigest("plan.save", payload);
      if (savedDraft.current?.signatureDigest !== signatureDigest) {
        const saved = await mutationAttempts.run(
          "plan-save",
          "plan.save",
          payload,
          (key) => savePlan(payload, key),
        );
        savedDraft.current = { signatureDigest, ...saved };
      }
      const saved = savedDraft.current;
      if (!saved) throw new ServiceError("APPLICATION_PAYLOAD_INVALID");
      const publishPayload = { planId: saved.planId, revisionId: saved.revisionId };
      const result = await mutationAttempts.run(
        "plan-publish",
        "plan.publish",
        publishPayload,
        (key) => publishPlan(saved.planId, saved.revisionId, key),
      );
      savedDraft.current = null;
      onComplete(result.planId);
    } catch (failure) {
      setError(asServiceError(failure));
    } finally {
      setBusy(false);
    }
  }

  const nextDisabled =
    (step === 0 && (!name.trim() || selectedScopes.length === 0)) ||
    (step === 1 && selectedDestinations.length === 0) ||
    (step === 4 && (blockingCapability || missingReader));

  return (
    <GuidedWorkspace
      title="Backup Plan"
      description="각 단계에서 하나의 정책 축을 결정하고 마지막에 실제 capability와 복구 reader 근거를 다시 확인합니다."
      steps={STEPS}
      currentStep={step}
      footer={
        <>
          <button type="button" className="button button-secondary" onClick={step === 0 ? onCancel : () => setStep((value) => value - 1)}>
            {step === 0 ? "취소" : "이전"}
          </button>
          {step < STEPS.length - 1 ? (
            <button type="button" className="button button-primary" disabled={nextDisabled} onClick={() => setStep((value) => value + 1)}>
              다음: {STEPS[step + 1]}
            </button>
          ) : (
            <button type="button" className="button button-primary" disabled={nextDisabled || busy} onClick={() => void publish()}>
              {busy ? "Plan 게시 중" : "검증 후 Plan 게시"}
            </button>
          )}
        </>
      }
    >
      <div aria-live="polite" className="live-region">
        {error ? error.message : `단계 ${step + 1} / ${STEPS.length}`}
      </div>
      {error && (
        <PageState
          kind={error.code === "APPLICATION_FORBIDDEN" ? "permission" : "error"}
          title={error.message}
          detail="Draft 입력은 화면에 남아 있어 수정 후 재시도할 수 있습니다."
          error={error}
        />
      )}
      {step === 0 && (
        <div className="form-stack">
          <label className="field">
            <span>Plan 이름</span>
            <input
              required
              name="planName"
              value={name}
              onChange={(event) => setName(event.target.value)}
              placeholder="Production hourly"
              autoComplete="off"
            />
          </label>
          <fieldset className="target-picker">
            <legend>Database → Backup Scope</legend>
            <p>데이터베이스 전체 또는 명시적으로 저장된 테이블 범위를 선택하세요.</p>
            {availableScopes.length === 0 && (
              <PageState
                kind="empty"
                title="선택할 Backup Scope가 없습니다."
                detail="Databases 화면에서 전체 DB 또는 명시 테이블 범위를 먼저 확인하세요."
              />
            )}
            {availableScopes.map((scope) => (
              <label key={scope.id}>
                <input
                  type="checkbox"
                  name="planScopes"
                  value={scope.id}
                  aria-label={`${scope.displayName} ${scope.databaseName} ${scope.connectionName} ${scope.engine}`}
                  checked={selectedScopes.includes(scope.id)}
                  disabled={scope.state === "retired" || scope.state === "disabled"}
                  onChange={() => toggleScope(scope)}
                />
                <span>
                  <strong>{scope.displayName}</strong>
                  <small>
                    {scope.displayName === scope.databaseName ? "" : `${scope.databaseName} · `}
                    {scope.connectionName} · {scope.engine}
                  </small>
                </span>
                <ScopeCoverage
                  mode={scope.mode}
                  selectedObjectCount={scope.selectedObjectCount}
                  eligibleObjectCount={scope.eligibleObjectCount}
                  newObjectCount={scope.newObjectCount}
                  missingObjectCount={scope.missingObjectCount}
                />
              </label>
            ))}
          </fieldset>
        </div>
      )}
      {step === 1 && (
        <fieldset className="target-picker">
          <legend>Required Destinations</legend>
          <p>writer와 reader가 모두 검증된 Destination을 required Replica로 선택합니다.</p>
          {destinations.map((destination) => {
            const writer = destination.roles.find((role) => role.role === "backup_writer");
            const reader = destination.roles.find((role) => role.role === "restore_reader");
            return (
              <label key={destination.id}>
                <input
                  type="checkbox"
                  name="planDestinations"
                  value={destination.id}
                  checked={selectedDestinations.includes(destination.id)}
                  onChange={() => toggle(selectedDestinations, destination.id, setSelectedDestinations)}
                />
                <span>
                  <strong>{destination.displayName}</strong>
                  <small>{destination.kind} · {destination.endpoint}</small>
                </span>
                <span className="role-pair">
                  <StatusBadge label={`W ${writer?.probeStatus ?? "unknown"}`} tone={toneForState(writer?.probeStatus ?? "unknown")} />
                  <StatusBadge label={`R ${reader?.probeStatus ?? "unknown"}`} tone={toneForState(reader?.probeStatus ?? "unknown")} />
                </span>
              </label>
            );
          })}
        </fieldset>
      )}
      {step === 2 && (
        <div className="form-stack">
          <div className="form-grid">
            <label className="field">
              <span>Cron schedule</span>
              <input
                required
                name="schedule"
                className="mono"
                value={schedule}
                onChange={(event) => setSchedule(event.target.value)}
                aria-describedby="schedule-help"
              />
              <small id="schedule-help">5-field cron, 서비스가 publish 전에 다음 실행 시각을 검증합니다.</small>
            </label>
            <label className="field">
              <span>Timezone</span>
              <select name="timezone" value={timezone} onChange={(event) => setTimezone(event.target.value)}>
                <option value="Asia/Seoul">Asia/Seoul</option>
                <option value="UTC">UTC</option>
              </select>
            </label>
          </div>
          <label className="field">
            <span>RPO 목표</span>
            <select name="rpoHours" value={rpoHours} onChange={(event) => setRpoHours(Number(event.target.value))}>
              <option value={1}>1시간</option>
              <option value={2}>2시간</option>
              <option value={6}>6시간</option>
              <option value={24}>24시간</option>
            </select>
          </label>
        </div>
      )}
      {step === 3 && (
        <div className="form-stack">
          <div className="form-grid">
            <label className="field">
              <span>일별 복구 지점</span>
              <input
                type="number"
                name="retentionDaily"
                min={1}
                value={retentionDaily}
                onChange={(event) => setRetentionDaily(Number(event.target.value))}
              />
            </label>
            <label className="field">
              <span>주별 복구 지점</span>
              <input
                type="number"
                name="retentionWeekly"
                min={0}
                value={retentionWeekly}
                onChange={(event) => setRetentionWeekly(Number(event.target.value))}
              />
            </label>
            <label className="field">
              <span>월별 복구 지점</span>
              <input
                type="number"
                name="retentionMonthly"
                min={0}
                value={retentionMonthly}
                onChange={(event) => setRetentionMonthly(Number(event.target.value))}
              />
            </label>
          </div>
          <fieldset className="choice-fieldset">
            <legend>요구 검증 레벨</legend>
            {([1, 2, 3] as const).map((level) => (
              <label key={level}>
                <input
                  type="radio"
                  name="verifyLevel"
                  value={level}
                  checked={verifyLevel === level}
                  onChange={() => setVerifyLevel(level)}
                />
                <span><strong>L{level}</strong> {level === 1 ? "서명·checksum" : level === 2 ? "native restore" : "격리 restore drill"}</span>
              </label>
            ))}
          </fieldset>
        </div>
      )}
      {step === 4 && (
        <div className="preflight-grid">
          <section>
            <h3>Capability preflight</h3>
            <CapabilityMatrix items={capabilities} />
          </section>
          <section className="preflight-summary">
            <h3>Publish summary</h3>
            <dl>
              <div><dt>Scopes</dt><dd>{selectedScopes.length}</dd></div>
              <div><dt>Required destinations</dt><dd>{selectedDestinations.length}</dd></div>
              <div><dt>RPO</dt><dd>{rpoHours}시간</dd></div>
              <div><dt>Retention</dt><dd>{retentionDaily}D · {retentionWeekly}W · {retentionMonthly}M</dd></div>
              <div><dt>Verification</dt><dd>L{verifyLevel}</dd></div>
            </dl>
            {missingReader && (
              <PageState
                kind="partial"
                title="복구 reader 근거가 부족합니다."
                detail="선택한 required Destination의 reader probe가 통과해야 Plan을 게시할 수 있습니다."
              />
            )}
            {blockingCapability && (
              <PageState
                kind="error"
                title="요구 capability를 충족하지 못합니다."
                detail="검증 레벨을 낮추는 silent downgrade는 하지 않습니다. 엔진 제한을 해결하거나 정책을 명시적으로 바꾸세요."
              />
            )}
          </section>
        </div>
      )}
    </GuidedWorkspace>
  );
}

function scopeSource(scope: BackupScope): PlanScopeSource {
  return {
    id: scope.id,
    databaseTargetId: scope.databaseTargetId,
    databaseName: scope.databaseName,
    connectionName: scope.connectionName,
    engine: scope.engine,
    displayName: scope.displayName,
    mode: scope.mode,
    state: scope.state,
    selectedObjectCount: scope.selectedObjectCount,
    eligibleObjectCount: scope.eligibleObjectCount,
    newObjectCount: scope.newObjectCount,
    missingObjectCount: scope.missingObjectCount,
    protectionCompliance: scope.protectionCompliance,
  };
}

export function engineCapabilitiesFromMatrix(
  engine: string,
  verifyLevel: 1 | 2 | 3,
  matrix: EngineMatrix,
  scopeMode: "whole_database" | "explicit_tables" = "whole_database",
): CapabilityItem[] {
  const row = matrix.rows.find(
    (candidate) => candidate.engine.toLocaleLowerCase() === engine.toLocaleLowerCase(),
  );
  const selectiveCapabilities: Array<{
    capability: EngineCapability["capability"];
    label: string;
  }> = scopeMode === "explicit_tables"
    ? [
        { capability: "discovery", label: `${engine} object discovery` },
      ]
    : [];
  const requested: Array<{
    capability: EngineCapability["capability"];
    label: string;
  }> = [
    ...selectiveCapabilities,
    {
      capability: "backup",
      label:
        scopeMode === "explicit_tables"
          ? `${engine} selective backup`
          : `${engine} backup`,
    },
    {
      capability: verifyLevel === 1 ? "verify_l1" : verifyLevel === 2 ? "verify_l2" : "restore_l3",
      label: `${engine} verify L${verifyLevel}`,
    },
  ];

  return requested.map(({ capability, label }) =>
    matrixCapabilityItem(label, capability, row),
  );
}

function matrixCapabilityItem(
  label: string,
  capabilityName: EngineCapability["capability"],
  row: EngineMatrixRow | undefined,
): CapabilityItem {
  if (!row) {
    return {
      capability: label,
      status: "unknown",
      evidence: "certification row 없음",
      limitation: "Engine Matrix에 이 엔진의 인증 행이 없습니다.",
    };
  }

  const capability = row.capabilities.find(
    (candidate) => candidate.capability === capabilityName,
  );
  const status = capabilityStatus(row, capability);
  return {
    capability: label,
    status,
    evidence:
      capability?.evidenceLocator ??
      row.evidenceLocator ??
      "현재 빌드의 관리형 인증 근거 없음",
    limitation:
      capability?.limitation ??
      (status === "experimental"
        ? `엔진 인증 상태가 ${row.supportStatus}이므로 Plan 게시가 차단됩니다.`
        : status === "unknown"
          ? `엔진 인증 상태가 ${row.supportStatus}이거나 capability 근거가 없습니다.`
          : undefined),
  };
}

function capabilityStatus(
  row: EngineMatrixRow,
  capability: EngineCapability | undefined,
): CapabilityItem["status"] {
  if (row.supportStatus === "candidate" || row.supportStatus === "experimental") {
    return "experimental";
  }
  if (row.supportStatus === "unsupported") return "unsupported";
  if (
    row.supportStatus === "unavailable" ||
    row.supportStatus === "suspended" ||
    row.supportStatus === "expired"
  ) {
    return "unknown";
  }
  if (!capability) return "unknown";

  const capabilityStatus: Record<
    EngineCapability["supportStatus"],
    CapabilityItem["status"]
  > = {
    passed: "supported",
    limited: "limited",
    failed: "unsupported",
    not_applicable: "unsupported",
    not_run: "unknown",
  };
  const mapped = capabilityStatus[capability.supportStatus];
  return row.supportStatus === "limited" && mapped === "supported"
    ? "limited"
    : mapped;
}
