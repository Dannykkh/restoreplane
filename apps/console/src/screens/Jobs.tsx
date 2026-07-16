import { useEffect, useState } from "react";
import "./Databases.css";
import "./Jobs.css";
import {
  addJob,
  listDatabases,
  listDestinations,
  listJobs,
  type DbSummary,
  type DestSummary,
  type JobSummaryRow,
  type NewJob,
} from "../lib/api";
import { cronPreview } from "../lib/cron";

const IMMUT_OPTS = [
  { value: "none", label: "불변 없음" },
  { value: "server_append_only", label: "추가 전용(SSH)" },
  { value: "governance", label: "Object Lock · Governance" },
  { value: "compliance", label: "Object Lock · Compliance" },
];

const EMPTY: NewJob = {
  name: "",
  source: "",
  destinations: [],
  requiredImmutability: "none",
  retentionDaily: 7,
  retentionWeekly: 4,
  retentionMonthly: 6,
  verifyLevel: 1,
  schedule: "0 3 * * *",
};

export function Jobs() {
  const [jobs, setJobs] = useState<JobSummaryRow[]>([]);
  const [dbs, setDbs] = useState<DbSummary[]>([]);
  const [dests, setDests] = useState<DestSummary[]>([]);
  const [form, setForm] = useState<NewJob>({ ...EMPTY });
  const [busy, setBusy] = useState(false);
  const [flash, setFlash] = useState<{ kind: "ok" | "err"; msg: string } | null>(null);

  function refresh() {
    listJobs().then(setJobs).catch((e) => setFlash({ kind: "err", msg: String(e) }));
    listDatabases().then(setDbs).catch(() => {});
    listDestinations().then(setDests).catch(() => {});
  }
  useEffect(refresh, []);

  function set<K extends keyof NewJob>(k: K, v: NewJob[K]) {
    setForm((f) => ({ ...f, [k]: v }));
  }
  function toggleDest(name: string) {
    setForm((f) => ({
      ...f,
      destinations: f.destinations.includes(name)
        ? f.destinations.filter((d) => d !== name)
        : [...f.destinations, name],
    }));
  }

  const canSubmit =
    form.name.trim() && form.source && form.destinations.length > 0 && !busy;

  async function submit() {
    setBusy(true);
    setFlash(null);
    try {
      await addJob({ ...form, source: form.source || dbs[0]?.alias || "" });
      setFlash({ kind: "ok", msg: `'${form.name}' 잡이 등록되었습니다.` });
      setForm({ ...EMPTY });
      refresh();
    } catch (e) {
      setFlash({ kind: "err", msg: String(e instanceof Error ? e.message : e) });
    } finally {
      setBusy(false);
    }
  }

  const noPrereq = dbs.length === 0 || dests.length === 0;

  return (
    <div>
      <div className="section-head">
        <h2>백업 실행</h2>
        <span className="label">잡 {jobs.length}개</span>
      </div>

      <div className="db-layout">
        <div className="db-list">
          {jobs.length === 0 ? (
            <div className="empty-note">등록된 잡이 없습니다. 오른쪽에서 만드세요.</div>
          ) : (
            jobs.map((j) => (
              <div className="job-row" key={j.name}>
                <div className="job-row-main">
                  <div className="db-alias">{j.name}</div>
                  <div className="db-sub">
                    {j.source} → {j.destinations.join(", ")}
                  </div>
                  <div className="job-row-meta">
                    <span className="badge badge-neutral">L{j.verifyLevel} 검증</span>
                    <span className="mono faint">{cronPreview(j.schedule)}</span>
                  </div>
                </div>
                <button className="btn btn-ghost" disabled title="라이브 DB 연결 필요">
                  실행
                </button>
              </div>
            ))
          )}
        </div>

        <div className="card">
          <h3 style={{ marginBottom: "var(--sp-4)" }}>백업 잡 만들기</h3>
          {flash && <div className={`form-flash ${flash.kind}`}>{flash.msg}</div>}
          {noPrereq && (
            <div className="form-flash err">
              먼저 DB와 목적지를 각각 1개 이상 등록하세요(DB · 목적지 탭).
            </div>
          )}

          <div className="form-grid">
            <div className="col-span">
              <label className="label">잡 이름</label>
              <input className="input" value={form.name} onChange={(e) => set("name", e.target.value)} placeholder="prod-nightly" />
            </div>
            <div className="col-span">
              <label className="label">소스 데이터베이스</label>
              <select className="select" value={form.source} onChange={(e) => set("source", e.target.value)}>
                <option value="">— 선택 —</option>
                {dbs.map((d) => (
                  <option key={d.alias} value={d.alias}>
                    {d.alias} ({d.engine})
                  </option>
                ))}
              </select>
            </div>
            <div className="col-span">
              <label className="label">목적지 (1개 이상 · 팬아웃)</label>
              <div className="dest-picker">
                {dests.map((d) => (
                  <label key={d.name} className={`dest-chip ${form.destinations.includes(d.name) ? "on" : ""}`}>
                    <input
                      type="checkbox"
                      checked={form.destinations.includes(d.name)}
                      onChange={() => toggleDest(d.name)}
                    />
                    {d.name}
                    <span className="chip-kind">{d.kind}</span>
                  </label>
                ))}
                {dests.length === 0 && <span className="faint">등록된 목적지 없음</span>}
              </div>
            </div>
            <div className="col-span">
              <label className="label">요구 불변성</label>
              <select
                className="select"
                value={form.requiredImmutability}
                onChange={(e) => set("requiredImmutability", e.target.value)}
              >
                {IMMUT_OPTS.map((o) => (
                  <option key={o.value} value={o.value}>
                    {o.label}
                  </option>
                ))}
              </select>
              <div className="field-hint">
                목적지 능력이 이 요구를 못 채우면 배선 시점에 실패합니다(랜섬웨어 오인 방지).
              </div>
            </div>
            <div>
              <label className="label">검증 레벨</label>
              <select className="select" value={form.verifyLevel} onChange={(e) => set("verifyLevel", Number(e.target.value))}>
                <option value={1}>L1 파일 무결성</option>
                <option value={2}>L2 포맷 유효성</option>
                <option value={3}>L3 실제 복원</option>
              </select>
            </div>
            <div>
              <label className="label">스케줄(cron)</label>
              <input className="input mono" value={form.schedule} onChange={(e) => set("schedule", e.target.value)} placeholder="0 3 * * *" />
              <div className="field-hint">{cronPreview(form.schedule)}</div>
            </div>
            <div>
              <label className="label">보존 · 일</label>
              <input className="input" type="number" value={form.retentionDaily} onChange={(e) => set("retentionDaily", Number(e.target.value))} />
            </div>
            <div>
              <label className="label">보존 · 주</label>
              <input className="input" type="number" value={form.retentionWeekly} onChange={(e) => set("retentionWeekly", Number(e.target.value))} />
            </div>
            <div>
              <label className="label">보존 · 월</label>
              <input className="input" type="number" value={form.retentionMonthly} onChange={(e) => set("retentionMonthly", Number(e.target.value))} />
            </div>
          </div>

          <div style={{ display: "flex", justifyContent: "flex-end", marginTop: "var(--sp-5)" }}>
            <button className="btn btn-primary" onClick={submit} disabled={!canSubmit}>
              {busy ? "등록 중…" : "잡 만들기"}
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
