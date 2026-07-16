import { useEffect, useState } from "react";
import "./Databases.css";
import "./Jobs.css";
import "./Restore.css";
import {
  listDestinations,
  restoreBackup,
  type DestSummary,
  type RunLogLine,
} from "../lib/api";
import { IconLock, IconShield } from "../components/Icons";

export function Restore() {
  const [dests, setDests] = useState<DestSummary[]>([]);
  const [backup, setBackup] = useState("");
  const [from, setFrom] = useState("");
  const [to, setTo] = useState("");
  const [decryptOnly, setDecryptOnly] = useState(false);
  const [withRecovery, setWithRecovery] = useState(false);
  const [ack, setAck] = useState(false);
  const [run, setRun] = useState<{ lines: RunLogLine[]; done: null | boolean } | null>(null);
  const cleanup = useState<{ fn: (() => void) | null }>({ fn: null })[0];

  useEffect(() => {
    listDestinations().then(setDests).catch(() => {});
  }, []);

  const canRun = backup.trim() && from && ack && !(run && run.done === null);

  async function start() {
    cleanup.fn?.();
    setRun({ lines: [], done: null });
    cleanup.fn = await restoreBackup(
      { backup, fromDestination: from, to, decryptOnly, withRecoveryCode: withRecovery },
      (l) => setRun((r) => (r ? { ...r, lines: [...r.lines, l] } : r)),
      (d) => setRun((r) => (r ? { ...r, done: d.ok } : r)),
    );
  }

  return (
    <div className="restore-wrap">
      <div className="section-head">
        <h2>복원</h2>
      </div>

      <div className="restore-gate">
        <span className="gate-icon">
          <IconShield width={22} height={22} />
        </span>
        <div>
          <div className="gate-title">서명 게이트가 먼저 실행됩니다</div>
          <div className="gate-sub">
            복원은 목적지의 ed25519 서명 매니페스트를 <b>복호화 전에</b> 검증합니다. 위조·미서명
            백업은 여기서 거부되어 코드 실행 위험을 차단합니다.
          </div>
        </div>
      </div>

      <div className="card restore-form">
        <div className="field">
          <label className="label">백업 ID</label>
          <input
            className="input mono"
            value={backup}
            onChange={(e) => setBackup(e.target.value)}
            placeholder="bk_2026071702593f"
          />
        </div>
        <div className="field">
          <label className="label">복원 출처(목적지)</label>
          <select className="select" value={from} onChange={(e) => setFrom(e.target.value)}>
            <option value="">— 선택 —</option>
            {dests.map((d) => (
              <option key={d.name} value={d.name}>
                {d.name} ({d.kind})
              </option>
            ))}
          </select>
        </div>
        <div className="field">
          <label className="label">복원 대상(옵션 — 비우면 도구 기본)</label>
          <input
            className="input"
            value={to}
            onChange={(e) => setTo(e.target.value)}
            placeholder="postgres://… 또는 산출 경로"
          />
        </div>

        <label className="check-row">
          <input type="checkbox" checked={decryptOnly} onChange={(e) => setDecryptOnly(e.target.checked)} />
          복호화만(임포트 없이 평문 덤프만 산출)
        </label>
        <label className="check-row" style={{ marginTop: 10 }}>
          <input type="checkbox" checked={withRecovery} onChange={(e) => setWithRecovery(e.target.checked)} />
          복구코드 경로 사용(비밀번호 분실 시 · DBBACKUP_RECOVERY_CODE)
        </label>

        <div className="restore-danger">
          <IconLock width={14} height={14} />
          <span>복원은 데이터를 덮어쓸 수 있는 위험 작업입니다. 격리/샌드박스 대상에 복원하세요.</span>
        </div>
        <label className="ack" style={{ marginTop: "var(--sp-4)" }}>
          <input type="checkbox" checked={ack} onChange={(e) => setAck(e.target.checked)} />
          위 위험을 이해했고, 신뢰하는 대상에 복원합니다.
        </label>

        <div style={{ display: "flex", justifyContent: "flex-end", marginTop: "var(--sp-5)" }}>
          <button className="btn btn-danger" onClick={start} disabled={!canRun}>
            {run && run.done === null ? "복원 중…" : "복원 실행"}
          </button>
        </div>
      </div>

      {run && (
        <div className="run-panel" style={{ marginTop: "var(--section)" }}>
          <div className="run-panel-head">
            <div>
              <span className="mono">{backup || "—"}</span>{" "}
              <span className="faint">· 복원 진행</span>
            </div>
            <div className="run-status">
              {run.done === null ? (
                <span className="badge badge-neutral">진행 중…</span>
              ) : run.done ? (
                <span className="badge badge-success">복원 검증 통과</span>
              ) : (
                <span className="badge badge-danger">중단/거부됨</span>
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
            {run.lines.length === 0
              ? "서명 검증 중…"
              : run.lines.map((l, i) => (
                  <div key={i} className={l.stream === "stderr" ? "log-err" : ""}>
                    {l.line}
                  </div>
                ))}
          </pre>
          <div className="run-note faint">
            실제 복원은 라이브 목적지·keystore 연결이 필요합니다. 브라우저에서는 흐름만 시연합니다.
          </div>
        </div>
      )}
    </div>
  );
}
