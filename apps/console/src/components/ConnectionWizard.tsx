import { useRef, useState, type FormEvent } from "react";
import {
  discoverConnection,
  getWorkItem,
  listDiscoveryResults,
  probeConnection,
  saveConnection,
  selectConnectionTargets,
} from "../lib/api";
import { asServiceError, ServiceError } from "../lib/errors";
import { useMutationAttemptRegistry } from "../lib/mutationAttempts";
import { collectCursorPages, INVENTORY_PAGE_LIMIT } from "../lib/pagination";
import type {
  ConnectionDraft,
  DiscoveredDatabase,
} from "../lib/types";
import { GuidedWorkspace } from "./GuidedWorkspace";
import { PageState } from "./PageState";
import { StatusBadge } from "./StatusBadge";

const STEPS = ["Profile", "Probe", "Targets", "Ready"] as const;

interface ConnectionWizardProps {
  onCancel: () => void;
  onComplete: (connectionId: string, targetId: string) => void;
}

const initialDraft: ConnectionDraft = {
  displayName: "",
  engine: "postgresql",
  transport: "tcp",
  host: "",
  port: 5432,
  filePath: "",
  tlsMode: "verify_full",
  username: "",
  password: "",
  restoreTestTarget: false,
};

export function ConnectionWizard({ onCancel, onComplete }: ConnectionWizardProps) {
  const mutationAttempts = useMutationAttemptRegistry();
  const [step, setStep] = useState(0);
  const [draft, setDraft] = useState(initialDraft);
  const [connectionId, setConnectionId] = useState<string | null>(null);
  const [workItemId, setWorkItemId] = useState<string | null>(null);
  const [discoveryRunId, setDiscoveryRunId] = useState<string | null>(null);
  const [discovered, setDiscovered] = useState<DiscoveredDatabase[]>([]);
  const [discoverySearch, setDiscoverySearch] = useState("");
  const [inventoryComplete, setInventoryComplete] = useState(false);
  const [inventoryTotalCount, setInventoryTotalCount] = useState<number | null>(null);
  const [selectedTargets, setSelectedTargets] = useState<string[]>([]);
  const [createdTargetIds, setCreatedTargetIds] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<ServiceError | null>(null);
  const completedProbeConnectionId = useRef<string | null>(null);
  const descriptionId = "connection-form-help";

  function update<K extends keyof ConnectionDraft>(key: K, value: ConnectionDraft[K]): void {
    setDraft((current) => ({ ...current, [key]: value }));
  }

  async function submitProfile(event: FormEvent): Promise<void> {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const result = await mutationAttempts.run(
        "connection-save",
        "connection.save",
        draft,
        (key) => saveConnection(draft, key),
      );
      setConnectionId(result.connectionId);
      setDraft((current) => ({ ...current, password: "" }));
      setStep(1);
    } catch (failure) {
      setError(asServiceError(failure));
    } finally {
      setBusy(false);
    }
  }

  async function runProbe(): Promise<void> {
    if (!connectionId) return;
    setBusy(true);
    setError(null);
    try {
      if (completedProbeConnectionId.current !== connectionId) {
        const work = await mutationAttempts.run(
          "connection-probe",
          "connection.probe",
          { connectionId },
          (key) => probeConnection(connectionId, key),
        );
        setWorkItemId(work.workItemId);
        if (!isSuccessful(work.state)) {
          await waitForSuccessfulWork(work.workItemId);
        }
        completedProbeConnectionId.current = connectionId;
      }
      const discovery = await mutationAttempts.run(
        "connection-discovery",
        "discovery.run",
        { connectionId },
        (key) => discoverConnection(connectionId, key),
      );
      setWorkItemId(discovery.workItemId);
      const completed = isSuccessful(discovery.state)
        ? null
        : await waitForSuccessfulWork(discovery.workItemId);
      const completedDiscoveryRunId = discovery.discoveryRunId || completed?.operationRunId || null;
      if (!completedDiscoveryRunId) {
        throw new ServiceError("APPLICATION_PAYLOAD_INVALID");
      }
      const inventory = await collectCursorPages((cursor) =>
        listDiscoveryResults({
          discoveryRunId: completedDiscoveryRunId,
          cursor,
          limit: INVENTORY_PAGE_LIMIT,
          search: null,
          eligibility: null,
        })
      );
      const discoveredRows = inventory.items;
      setDiscoveryRunId(completedDiscoveryRunId);
      setDiscovered(discoveredRows);
      setInventoryComplete(inventory.complete);
      setInventoryTotalCount(inventory.totalCount);
      setSelectedTargets(
        discoveredRows
          .filter((item) => item.eligibility === "eligible")
          .slice(0, 1_000)
          .map((item) => item.engineObjectId),
      );
      setStep(2);
    } catch (failure) {
      setError(asServiceError(failure));
    } finally {
      setBusy(false);
    }
  }

  function toggleTarget(id: string): void {
    setSelectedTargets((current) =>
      current.includes(id) ? current.filter((value) => value !== id) : [...current, id],
    );
  }

  async function commitTargets(): Promise<void> {
    if (
      !connectionId ||
      !discoveryRunId ||
      selectedTargets.length === 0 ||
      selectedTargets.length > 1_000 ||
      !inventoryComplete
    ) return;
    setBusy(true);
    setError(null);
    try {
      const payload = { connectionId, discoveryRunId, engineObjectIds: selectedTargets };
      const result = await mutationAttempts.run(
        "connection-target-select",
        "target.select",
        payload,
        (key) => selectConnectionTargets(
          connectionId,
          discoveryRunId,
          selectedTargets,
          key,
        ),
      );
      if (result.targets.length === 0) {
        throw new ServiceError("APPLICATION_PAYLOAD_INVALID");
      }
      setCreatedTargetIds(result.targets.map((item) => item.id));
      setStep(3);
    } catch (failure) {
      setError(asServiceError(failure));
    } finally {
      setBusy(false);
    }
  }

  async function searchDiscovery(): Promise<void> {
    if (!discoveryRunId) return;
    setBusy(true);
    setError(null);
    try {
      const page = await collectCursorPages((cursor) =>
        listDiscoveryResults({
          discoveryRunId,
          cursor,
          limit: INVENTORY_PAGE_LIMIT,
          search: discoverySearch.trim() || null,
          eligibility: null,
        })
      );
      setDiscovered(page.items);
    } catch (failure) {
      setError(asServiceError(failure));
    } finally {
      setBusy(false);
    }
  }

  return (
    <GuidedWorkspace
      title="새 연결"
      description="서버 연결과 보호 대상 데이터베이스를 분리해 등록합니다."
      steps={STEPS}
      currentStep={step}
      footer={
        <>
          <button type="button" className="button button-secondary" onClick={onCancel}>
            취소
          </button>
          {step === 1 && (
            <button type="button" className="button button-primary" disabled={busy} onClick={() => void runProbe()}>
              {busy ? "Probe 실행 중" : "Probe 및 검색"}
            </button>
          )}
          {step === 2 && (
            <button
              type="button"
              className="button button-primary"
              disabled={
                selectedTargets.length === 0 ||
                selectedTargets.length > 1_000 ||
                !inventoryComplete ||
                busy
              }
              onClick={() => void commitTargets()}
            >
              {busy ? "Target 저장 중" : `${selectedTargets.length}개 Target 선택`}
            </button>
          )}
          {step === 3 && connectionId && createdTargetIds[0] && (
            <button type="button" className="button button-primary" onClick={() => onComplete(connectionId, createdTargetIds[0])}>
              Backup Plan 시작
            </button>
          )}
        </>
      }
    >
      <div className="live-region" aria-live="polite">
        {busy ? "서비스 작업을 기다리고 있습니다." : workItemId ? `Work Item ${workItemId}` : ""}
      </div>
      {error && (
        <PageState
          kind={error.code === "APPLICATION_FORBIDDEN" ? "permission" : "error"}
          title={error.message}
          detail="입력한 비밀값은 화면과 URL에 보존되지 않았습니다."
          error={error}
        />
      )}
      {step === 0 && (
        <form className="form-stack" onSubmit={(event) => void submitProfile(event)}>
          <p id={descriptionId} className="form-intro">
            인증값은 이 요청에서만 서비스로 전달되며 브라우저 저장소에 기록하지 않습니다.
          </p>
          <div className="form-grid">
            <label className="field">
              <span>연결 이름</span>
              <input
                required
                name="displayName"
                value={draft.displayName}
                onChange={(event) => update("displayName", event.target.value)}
                aria-describedby={descriptionId}
                autoComplete="off"
              />
            </label>
            <label className="field">
              <span>데이터베이스 엔진</span>
              <select
                name="engine"
                value={draft.engine}
                onChange={(event) => {
                  const engine = event.target.value;
                  update("engine", engine);
                  update("transport", engine === "sqlite" ? "file" : "tcp");
                  update(
                    "port",
                    engine === "postgresql"
                      ? 5432
                      : engine === "sqlserver"
                        ? 1433
                        : 3306,
                  );
                }}
              >
                <option value="postgresql">PostgreSQL</option>
                <option value="mysql">MySQL</option>
                <option value="mariadb">MariaDB</option>
                <option value="sqlserver">SQL Server candidate</option>
                <option value="sqlite">SQLite</option>
              </select>
            </label>
          </div>
          {draft.transport === "tcp" ? (
            <>
              <div className="form-grid form-grid-endpoint">
                <label className="field">
                  <span>호스트</span>
                  <input
                    required
                    name="host"
                    value={draft.host}
                    onChange={(event) => update("host", event.target.value)}
                    placeholder="db.internal"
                    autoComplete="off"
                  />
                </label>
                <label className="field field-port">
                  <span>포트</span>
                  <input
                    required
                    name="port"
                    type="number"
                    min={1}
                    max={65535}
                    value={draft.port}
                    onChange={(event) => update("port", Number(event.target.value))}
                  />
                </label>
              </div>
              <label className="field">
                <span>TLS 모드</span>
                <select
                  name="tlsMode"
                  value={draft.tlsMode}
                  onChange={(event) => update("tlsMode", event.target.value as ConnectionDraft["tlsMode"])}
                >
                  <option value="verify_full">호스트와 CA 검증</option>
                  <option value="verify_ca">CA 검증</option>
                  <option value="required">암호화 필수, 인증서 미검증</option>
                  <option value="preferred">가능하면 TLS</option>
                  <option value="disabled">TLS 사용 안 함</option>
                </select>
              </label>
              <div className="form-grid">
                <label className="field">
                  <span>사용자</span>
                  <input
                    required
                    name="username"
                    value={draft.username}
                    onChange={(event) => update("username", event.target.value)}
                    autoComplete="username"
                  />
                </label>
                <label className="field">
                  <span>비밀번호</span>
                  <input
                    required
                    name="password"
                    type="password"
                    value={draft.password}
                    onChange={(event) => update("password", event.target.value)}
                    autoComplete="new-password"
                  />
                </label>
              </div>
            </>
          ) : (
            <label className="field">
              <span>SQLite 파일</span>
              <input
                required
                name="filePath"
                value={draft.filePath}
                onChange={(event) => update("filePath", event.target.value)}
                placeholder="D:\data\application.db"
                autoComplete="off"
              />
              <small>서비스가 file identity, WAL, 잠금 상태를 probe합니다.</small>
            </label>
          )}
          <label className="safety-option">
            <input
              type="checkbox"
              name="restoreTestTarget"
              checked={draft.restoreTestTarget}
              onChange={(event) =>
                update("restoreTestTarget", event.target.checked)
              }
            />
            <span>
              <strong>격리된 restore-test target</strong>
              <small>
                운영 트래픽과 분리된 테스트 전용 서버일 때만 선택합니다. L3 restore drill은
                이 표식이 있는 Connection에서만 허용됩니다.
              </small>
            </span>
          </label>
          <div className="form-submit">
            <button type="submit" className="button button-primary" disabled={busy}>
              {busy ? "Draft 저장 중" : "Draft 저장"}
            </button>
          </div>
        </form>
      )}
      {step === 1 && (
        <div className="probe-stage">
          <StatusBadge label="Draft 저장됨" tone="neutral" />
          <h3>연결 가능성과 도구 신뢰를 확인합니다.</h3>
          <p>
            DNS, 네트워크, TLS, 인증, 서버 버전, 고정된 native tool을 순서대로 검사합니다.
            실패하면 draft는 유지되며 비밀값은 다시 표시되지 않습니다.
          </p>
        </div>
      )}
      {step === 2 && (
        <fieldset className="target-picker">
          <legend>보호할 Database Target 선택</legend>
          <p>Connection 아래에서 발견됐지만, 선택한 데이터베이스만 보호 대상이 됩니다.</p>
          <div className="form-grid form-grid-endpoint">
            <label className="field">
              <span>Discovery search</span>
              <input
                value={discoverySearch}
                onChange={(event) => setDiscoverySearch(event.target.value)}
                placeholder="database name"
              />
            </label>
            <button
              type="button"
              className="button button-secondary"
              disabled={busy}
              onClick={() => void searchDiscovery()}
            >
              Search
            </button>
          </div>
          {!inventoryComplete && (
            <PageState
              kind="error"
              title="Discovery inventory is incomplete."
              detail="Target selection is blocked until every cursor page is verified."
            />
          )}
          <p className="mono">{discovered.length} / {inventoryTotalCount ?? "?"}</p>
          {discovered.map((database) => (
            <label key={database.engineObjectId} data-eligibility={database.eligibility}>
              <input
                type="checkbox"
                name="databaseTargets"
                value={database.engineObjectId}
                checked={selectedTargets.includes(database.engineObjectId)}
                disabled={database.eligibility !== "eligible"}
                onChange={() => toggleTarget(database.engineObjectId)}
              />
              <span>
                <strong>{database.databaseName}</strong>
                <small>{database.exclusionReason ?? database.eligibility}</small>
              </span>
            </label>
          ))}
        </fieldset>
      )}
      {step === 3 && (
        <div className="ready-summary">
          <StatusBadge label="Profile ready" tone="success" />
          <h3>{createdTargetIds.length}개 Database Target을 선택했습니다.</h3>
          <p>
            아직 백업이 구성된 것은 아닙니다. 다음 단계에서 Destination과 Backup Plan을
            연결해야 보호가 시작됩니다.
          </p>
          <dl>
            <div><dt>Connection ID</dt><dd className="mono">{connectionId}</dd></div>
            <div><dt>Targets</dt><dd>{createdTargetIds.length}</dd></div>
            <div><dt>Work Item</dt><dd className="mono">{workItemId ?? "기록 없음"}</dd></div>
          </dl>
        </div>
      )}
    </GuidedWorkspace>
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
      throw new ServiceError(work.errorCode ?? "QUEUE_RETRY_EXHAUSTED", {
        retryable: false,
      });
    }
    await new Promise((resolve) => window.setTimeout(resolve, 500));
  }
  throw new ServiceError("PROCESS_TIMEOUT", { retryable: true });
}
