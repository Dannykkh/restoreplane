import { formatDateTime } from "../lib/format";
import type { WorkItemProgress, WorkItemStatus } from "../lib/types";
import { StatusBadge, toneForState } from "./StatusBadge";

interface BackupTaskCenterProps {
  items: readonly WorkItemStatus[];
  onCancel: (workItemId: string, fencingToken: number) => void;
  cancellingId?: string | null;
}

const terminalStates = new Set([
  "succeeded",
  "partially_succeeded",
  "failed",
  "failed_terminal",
  "cancelled",
]);

export function BackupTaskCenter({
  items,
  onCancel,
  cancellingId = null,
}: BackupTaskCenterProps) {
  if (items.length === 0) return null;
  const activeCount = items.filter((item) => !isTerminal(item.state)).length;

  return (
    <section className="backup-task-center" aria-labelledby="backup-task-center-title">
      <header className="backup-task-header">
        <div>
          <span className="eyebrow">Durable work queue</span>
          <h2 id="backup-task-center-title">
            {activeCount > 0 ? "실행 중인 백업" : "최근 백업 작업"}
          </h2>
        </div>
        <span className="mono">{activeCount} active / {items.length} tracked</span>
      </header>
      <ol className="backup-task-list">
        {items.map((item) => {
          const title = item.subject?.planName ?? operationLabel(item);
          const accessibleTitle = item.subject?.targetName
            ? `${title} ${item.subject.targetName}`
            : title;
          const target = [item.subject?.targetName, item.subject?.engine]
            .filter(Boolean)
            .join(" · ");
          const progress = measuredProgress(item.progress);
          const cancellable =
            !isTerminal(item.state) &&
            item.state !== "cancel_requested" &&
            item.state !== "committing" &&
            Number.isSafeInteger(item.fencingToken) &&
            item.fencingToken > 0;

          return (
            <li key={item.workItemId} className="backup-task-row">
              <div className="backup-task-identity">
                <strong>{title}</strong>
                <span>{target || "대상 근거를 불러오는 중"}</span>
                <small className="mono">{item.workItemId}</small>
              </div>
              <div className="backup-task-state">
                <StatusBadge
                  label={stateLabel(item.state)}
                  tone={toneForState(item.state)}
                  reason={item.waitReason ?? item.errorCode ?? undefined}
                  evidenceAtMs={item.updatedAtMs}
                />
                <span className="backup-task-phase">
                  {phaseLabel(item.progress?.phase ?? item.state)}
                  <small>attempt {item.currentAttemptNo}</small>
                </span>
              </div>
              <div className="backup-task-progress">
                {progress ? (
                  <>
                    <progress
                      max={100}
                      value={progress.percent}
                      aria-label={`${accessibleTitle} 진행률`}
                      aria-valuenow={progress.percent}
                    />
                    <span>{formatProgress(item.progress!, progress.percent)}</span>
                  </>
                ) : (
                  <>
                    <progress aria-label={`${accessibleTitle} 단계 진행`} />
                    <span>총량을 측정하지 않는 단계입니다.</span>
                  </>
                )}
                {item.progress?.currentObject && (
                  <small className="mono">{item.progress.currentObject}</small>
                )}
                {item.progress?.updatedAtMs && (
                  <small>진행 근거 {formatDateTime(item.progress.updatedAtMs)}</small>
                )}
              </div>
              <div className="backup-task-actions">
                {cancellable && (
                  <button
                    type="button"
                    className="button button-danger"
                    disabled={cancellingId === item.workItemId}
                    aria-label={`${accessibleTitle} 취소`}
                    onClick={() => onCancel(item.workItemId, item.fencingToken)}
                  >
                    {cancellingId === item.workItemId ? "취소 요청 중" : "취소"}
                  </button>
                )}
                {item.state === "committing" && (
                  <small>커밋이 시작되어 취소할 수 없습니다.</small>
                )}
              </div>
            </li>
          );
        })}
      </ol>
    </section>
  );
}

function isTerminal(state: string): boolean {
  return terminalStates.has(state);
}

function operationLabel(item: WorkItemStatus): string {
  if (item.kind === "backup") return "Backup Work";
  return item.operationKind?.replaceAll("_", " ") ?? `${item.kind} work`;
}

function stateLabel(state: string): string {
  return state.replaceAll("_", " ");
}

function phaseLabel(phase: string): string {
  const labels: Record<string, string> = {
    queued: "Queue 대기",
    delayed: "Permit 대기",
    leased: "Worker 할당",
    starting: "Preflight",
    running: "백업 실행",
    dumping: "Native dump",
    encoded: "압축·암호화",
    replicating: "Replica 전송",
    verifying: "검증",
    committing: "Recovery Point commit",
    cancel_requested: "안전하게 중단 중",
    succeeded: "완료",
    partially_succeeded: "부분 완료",
    failed: "실패",
    failed_terminal: "최종 실패",
    cancelled: "취소됨",
  };
  return labels[phase] ?? phase.replaceAll("_", " ");
}

function measuredProgress(progress: WorkItemProgress | null | undefined) {
  if (
    !progress ||
    progress.completedUnits === null ||
    progress.totalUnits === null ||
    !Number.isFinite(progress.completedUnits) ||
    !Number.isFinite(progress.totalUnits) ||
    progress.completedUnits < 0 ||
    progress.totalUnits <= 0
  ) {
    return null;
  }
  return {
    percent: Math.min(100, Math.max(0, Math.round((progress.completedUnits / progress.totalUnits) * 100))),
  };
}

function formatProgress(progress: WorkItemProgress, percent: number): string {
  const completed = new Intl.NumberFormat("ko-KR").format(progress.completedUnits ?? 0);
  const total = new Intl.NumberFormat("ko-KR").format(progress.totalUnits ?? 0);
  const unit = progress.unit === "mib" ? "MiB" : progress.unit;
  return `${completed} / ${total} ${unit} · ${percent}%`;
}
