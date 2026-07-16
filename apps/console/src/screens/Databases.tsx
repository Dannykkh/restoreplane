import { useEffect, useState } from "react";
import "./Databases.css";
import { addDatabase, listDatabases, type DbSummary } from "../lib/api";
import { IconDatabase } from "../components/Icons";

const ENGINES = [
  { value: "postgresql", label: "PostgreSQL", port: 5432 },
  { value: "mysql", label: "MySQL", port: 3306 },
  { value: "mariadb", label: "MariaDB", port: 3306 },
];

const EMPTY = {
  alias: "",
  engine: "postgresql",
  host: "",
  port: 5432,
  dbname: "",
  user: "",
  password: "",
  dumpGlobals: true,
};

export function Databases() {
  const [dbs, setDbs] = useState<DbSummary[]>([]);
  const [form, setForm] = useState({ ...EMPTY });
  const [masterPw, setMasterPw] = useState("");
  const [busy, setBusy] = useState(false);
  const [flash, setFlash] = useState<{ kind: "ok" | "err"; msg: string } | null>(null);

  function refresh() {
    listDatabases()
      .then(setDbs)
      .catch((e) => setFlash({ kind: "err", msg: String(e) }));
  }
  useEffect(refresh, []);

  const canSubmit =
    form.alias.trim() &&
    form.host.trim() &&
    form.dbname.trim() &&
    form.user.trim() &&
    form.password &&
    masterPw.length >= 8 &&
    !busy;

  function set<K extends keyof typeof form>(k: K, v: (typeof form)[K]) {
    setForm((f) => ({ ...f, [k]: v }));
  }

  async function submit() {
    setBusy(true);
    setFlash(null);
    try {
      await addDatabase(masterPw, form);
      setFlash({ kind: "ok", msg: `'${form.alias}' 등록됨 — 비밀번호는 봉투 암호화되어 저장되었습니다.` });
      setForm({ ...EMPTY });
      setMasterPw("");
      refresh();
    } catch (e) {
      setFlash({ kind: "err", msg: String(e instanceof Error ? e.message : e) });
    } finally {
      setBusy(false);
    }
  }

  return (
    <div>
      <div className="section-head">
        <h2>DB · 목적지 등록</h2>
        <span className="label">데이터베이스 {dbs.length}개</span>
      </div>

      <div className="db-layout">
        <div className="db-list">
          {dbs.length === 0 ? (
            <div className="empty-note">등록된 데이터베이스가 없습니다. 오른쪽에서 추가하세요.</div>
          ) : (
            dbs.map((d) => (
              <div className="db-row" key={d.alias}>
                <span className="db-icon">
                  <IconDatabase width={18} height={18} />
                </span>
                <div className="db-main">
                  <div className="db-alias">
                    {d.alias} <span className="engine-tag">{d.engine}</span>
                  </div>
                  <div className="db-sub">
                    {d.user}@{d.host}:{d.port}/{d.dbname}
                    {d.hasTunnel ? " · 터널" : ""}
                  </div>
                </div>
              </div>
            ))
          )}
        </div>

        <div className="card">
          <h3 style={{ marginBottom: "var(--sp-4)" }}>데이터베이스 추가</h3>
          {flash && <div className={`form-flash ${flash.kind}`}>{flash.msg}</div>}

          <div className="form-grid">
            <div className="col-span">
              <label className="label">별칭(alias)</label>
              <input
                className="input"
                value={form.alias}
                onChange={(e) => set("alias", e.target.value)}
                placeholder="prod-main"
              />
            </div>
            <div>
              <label className="label">엔진</label>
              <select
                className="select"
                value={form.engine}
                onChange={(e) => {
                  const eng = ENGINES.find((x) => x.value === e.target.value)!;
                  setForm((f) => ({ ...f, engine: eng.value, port: eng.port }));
                }}
              >
                {ENGINES.map((x) => (
                  <option key={x.value} value={x.value}>
                    {x.label}
                  </option>
                ))}
              </select>
            </div>
            <div>
              <label className="label">포트</label>
              <input
                className="input"
                type="number"
                value={form.port}
                onChange={(e) => set("port", Number(e.target.value))}
              />
            </div>
            <div className="col-span">
              <label className="label">호스트</label>
              <input
                className="input"
                value={form.host}
                onChange={(e) => set("host", e.target.value)}
                placeholder="10.0.0.5"
              />
            </div>
            <div>
              <label className="label">데이터베이스명</label>
              <input
                className="input"
                value={form.dbname}
                onChange={(e) => set("dbname", e.target.value)}
                placeholder="app"
              />
            </div>
            <div>
              <label className="label">사용자</label>
              <input
                className="input"
                value={form.user}
                onChange={(e) => set("user", e.target.value)}
                placeholder="backup"
              />
            </div>
            <div className="col-span">
              <label className="label">DB 비밀번호</label>
              <input
                className="input"
                type="password"
                value={form.password}
                onChange={(e) => set("password", e.target.value)}
                placeholder="봉투 암호화되어 저장됩니다"
              />
            </div>
            {form.engine === "postgresql" && (
              <label className="check-row col-span">
                <input
                  type="checkbox"
                  checked={form.dumpGlobals}
                  onChange={(e) => set("dumpGlobals", e.target.checked)}
                />
                전역 객체(roles/tablespaces) 동봉 — pg_dumpall --globals-only
              </label>
            )}
          </div>

          <hr className="form-divider" />

          <div className="field" style={{ marginBottom: 0 }}>
            <label className="label">마스터 비밀번호 (키 잠금 해제)</label>
            <input
              className="input"
              type="password"
              value={masterPw}
              onChange={(e) => setMasterPw(e.target.value)}
              placeholder="초기 설정에서 정한 비밀번호"
            />
            <div className="field-hint">
              DB 비밀번호를 봉투 암호화하려면 마스터 키를 잠깐 여는 데 필요합니다.
            </div>
          </div>

          <div style={{ display: "flex", justifyContent: "flex-end", marginTop: "var(--sp-5)" }}>
            <button className="btn btn-primary" onClick={submit} disabled={!canSubmit}>
              {busy ? "등록 중…" : "데이터베이스 등록"}
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
