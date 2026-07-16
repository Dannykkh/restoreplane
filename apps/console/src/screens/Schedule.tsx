import { useEffect, useState } from "react";
import "./Databases.css";
import "./Jobs.css";
import "./Schedule.css";
import { listJobs, registerSchedule, type JobSummaryRow, type RunLogLine } from "../lib/api";
import { cronPreview } from "../lib/cron";
import { IconCalendar } from "../components/Icons";

export function Schedule() {
  const [jobs, setJobs] = useState<JobSummaryRow[]>([]);
  const [run, setRun] = useState<{ job: string; lines: RunLogLine[]; done: null | boolean } | null>(
    null,
  );
  const cleanup = useState<{ fn: (() => void) | null }>({ fn: null })[0];

  useEffect(() => {
    listJobs().then(setJobs).catch(() => {});
  }, []);

  async function act(job: string, remove: boolean) {
    cleanup.fn?.();
    setRun({ job, lines: [], done: null });
    cleanup.fn = await registerSchedule(
      job,
      remove,
      (l) => setRun((r) => (r ? { ...r, lines: [...r.lines, l] } : r)),
      (d) => setRun((r) => (r ? { ...r, done: d.ok } : r)),
    );
  }

  return (
    <div className="sched-wrap">
      <div className="section-head">
        <h2>스케줄 · 알림</h2>
        <span className="label">잡 {jobs.length}개</span>
      </div>

      <div className="rpo-banner">
        <IconCalendar width={18} height={18} />
        <span>
          예약은 Windows 작업 스케줄러(schtasks)로 무인 등록됩니다. PC가 꺼져 있던 시간만큼 RPO(복구
          시점 목표)가 늘어나며, 재부팅 후 <b>StartWhenAvailable</b>로 놓친 백업을 따라잡습니다.
        </span>
      </div>

      {run && (
        <div className="run-panel" style={{ marginBottom: "var(--section)" }}>
          <div className="run-panel-head">
            <div>
              <span className="mono">dbbackup-{run.job}</span>{" "}
              <span className="faint">· schtasks</span>
            </div>
            <div className="run-status">
              {run.done === null ? (
                <span className="badge badge-neutral">처리 중…</span>
              ) : run.done ? (
                <span className="badge badge-success">완료</span>
              ) : (
                <span className="badge badge-danger">실패</span>
              )}
              <button
                className="btn btn-ghost"
                onClick={() => {
                  cleanup.fn?.();
                  setRun(null);
                }}
              >
                닫기
              </button>
            </div>
          </div>
          <pre className="run-log">
            {run.lines.map((l, i) => (
              <div key={i} className={l.stream === "stderr" ? "log-err" : ""}>
                {l.line}
              </div>
            ))}
          </pre>
        </div>
      )}

      {jobs.length === 0 ? (
        <div className="empty-note">등록된 잡이 없습니다. 먼저 백업 실행 탭에서 잡을 만드세요.</div>
      ) : (
        <div className="sched-list">
          {jobs.map((j) => (
            <div className="sched-card" key={j.name}>
              <div className="sched-main">
                <div className="db-alias">{j.name}</div>
                <div className="sched-cron">
                  <span className="mono">{j.schedule || "예약 없음"}</span>
                  <span className="sched-human">{cronPreview(j.schedule)}</span>
                </div>
              </div>
              <div className="sched-actions">
                <button
                  className="btn btn-ghost"
                  onClick={() => act(j.name, true)}
                  disabled={run?.done === null}
                >
                  해제
                </button>
                <button
                  className="btn btn-primary"
                  onClick={() => act(j.name, false)}
                  disabled={run?.done === null}
                >
                  예약 등록
                </button>
              </div>
            </div>
          ))}
        </div>
      )}

      <div className="notify-note faint">
        알림 채널(webhook/email)은 config의 [notify]에 설정되며, 실패·RPO 초과 시 발송됩니다.
        채널 편집 UI는 후속 배선 예정입니다.
      </div>
    </div>
  );
}
