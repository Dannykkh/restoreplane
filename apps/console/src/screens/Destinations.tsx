import { useEffect, useState } from "react";
import "./Databases.css";
import { addDestination, listDestinations, type DestSummary, type NewDestination } from "../lib/api";
import { ImmutabilityBadge } from "../components/Badges";
import type { ImmutabilityKind } from "../types";

const KINDS = [
  { value: "local", label: "로컬 디스크" },
  { value: "s3", label: "S3 (Object Lock)" },
  { value: "ssh", label: "SSH / SFTP" },
];

type Form = {
  name: string;
  kind: string;
  path: string;
  endpoint: string;
  region: string;
  bucket: string;
  prefix: string;
  accessKeyId: string;
  secretAccessKey: string;
  objectLockMode: string;
  retentionDays: number;
  host: string;
  port: number;
  user: string;
  privateKeyPath: string;
  hostKeyFingerprint: string;
};

const EMPTY: Form = {
  name: "",
  kind: "s3",
  path: "",
  endpoint: "",
  region: "",
  bucket: "",
  prefix: "",
  accessKeyId: "",
  secretAccessKey: "",
  objectLockMode: "governance",
  retentionDays: 30,
  host: "",
  port: 22,
  user: "",
  privateKeyPath: "",
  hostKeyFingerprint: "",
};

// 폼 상태로부터 결과 불변성 등급을 미리 보여준다(DESIGN.md: 불변성 capability 배지).
function previewImmut(f: Form): ImmutabilityKind {
  if (f.kind === "ssh") return "server_append_only";
  if (f.kind === "s3" && f.objectLockMode === "governance") return "object_lock_governance";
  if (f.kind === "s3" && f.objectLockMode === "compliance") return "object_lock_compliance";
  return "none";
}

export function Destinations() {
  const [dests, setDests] = useState<DestSummary[]>([]);
  const [form, setForm] = useState<Form>({ ...EMPTY });
  const [masterPw, setMasterPw] = useState("");
  const [busy, setBusy] = useState(false);
  const [flash, setFlash] = useState<{ kind: "ok" | "err"; msg: string } | null>(null);

  function refresh() {
    listDestinations()
      .then(setDests)
      .catch((e) => setFlash({ kind: "err", msg: String(e) }));
  }
  useEffect(refresh, []);

  function set<K extends keyof Form>(k: K, v: Form[K]) {
    setForm((f) => ({ ...f, [k]: v }));
  }

  const needsMasterPw = form.kind === "s3" && (form.accessKeyId !== "" || form.secretAccessKey !== "");
  const canSubmit =
    form.name.trim() &&
    (form.kind !== "local" || form.path.trim()) &&
    (form.kind !== "s3" || form.bucket.trim()) &&
    (form.kind !== "ssh" || (form.host.trim() && form.hostKeyFingerprint.trim())) &&
    (!needsMasterPw || masterPw.length >= 8) &&
    !busy;

  async function submit() {
    setBusy(true);
    setFlash(null);
    try {
      const payload: NewDestination = { name: form.name, kind: form.kind };
      if (form.kind === "local") payload.path = form.path;
      if (form.kind === "s3") {
        Object.assign(payload, {
          endpoint: form.endpoint || undefined,
          region: form.region || undefined,
          bucket: form.bucket,
          prefix: form.prefix,
          accessKeyId: form.accessKeyId || undefined,
          secretAccessKey: form.secretAccessKey || undefined,
          objectLockMode: form.objectLockMode,
          retentionDays: form.retentionDays,
        });
      }
      if (form.kind === "ssh") {
        Object.assign(payload, {
          host: form.host,
          port: form.port,
          user: form.user,
          privateKeyPath: form.privateKeyPath,
          hostKeyFingerprint: form.hostKeyFingerprint,
          path: form.path,
        });
      }
      await addDestination(masterPw, payload);
      setFlash({ kind: "ok", msg: `'${form.name}' 등록됨.` });
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
    <div className="db-layout">
      <div className="db-list">
        {dests.length === 0 ? (
          <div className="empty-note">등록된 목적지가 없습니다. 오른쪽에서 추가하세요.</div>
        ) : (
          dests.map((d) => (
            <div className="db-row" key={d.name}>
              <span className="db-icon">{d.kind.toUpperCase().slice(0, 1)}</span>
              <div className="db-main">
                <div className="db-alias">
                  {d.name} <span className="engine-tag">{d.kind}</span>
                </div>
                <div className="db-sub">{d.location}</div>
              </div>
              <ImmutabilityBadge kind={d.immutability as ImmutabilityKind} />
            </div>
          ))
        )}
      </div>

      <div className="card">
        <h3 style={{ marginBottom: "var(--sp-4)" }}>목적지 추가</h3>
        {flash && <div className={`form-flash ${flash.kind}`}>{flash.msg}</div>}

        <div className="form-grid">
          <div className="col-span">
            <label className="label">이름</label>
            <input
              className="input"
              value={form.name}
              onChange={(e) => set("name", e.target.value)}
              placeholder="s3-worm"
            />
          </div>
          <div className="col-span">
            <label className="label">종류</label>
            <select className="select" value={form.kind} onChange={(e) => set("kind", e.target.value)}>
              {KINDS.map((k) => (
                <option key={k.value} value={k.value}>
                  {k.label}
                </option>
              ))}
            </select>
          </div>

          {form.kind === "local" && (
            <div className="col-span">
              <label className="label">경로</label>
              <input
                className="input"
                value={form.path}
                onChange={(e) => set("path", e.target.value)}
                placeholder="D:\\backups"
              />
            </div>
          )}

          {form.kind === "s3" && (
            <>
              <div className="col-span">
                <label className="label">버킷</label>
                <input className="input" value={form.bucket} onChange={(e) => set("bucket", e.target.value)} placeholder="my-backups" />
              </div>
              <div>
                <label className="label">리전</label>
                <input className="input" value={form.region} onChange={(e) => set("region", e.target.value)} placeholder="ap-northeast-2" />
              </div>
              <div>
                <label className="label">접두어(prefix)</label>
                <input className="input" value={form.prefix} onChange={(e) => set("prefix", e.target.value)} placeholder="prod/" />
              </div>
              <div className="col-span">
                <label className="label">엔드포인트(옵션)</label>
                <input className="input" value={form.endpoint} onChange={(e) => set("endpoint", e.target.value)} placeholder="https://s3.ap-northeast-2.amazonaws.com" />
              </div>
              <div>
                <label className="label">Access Key ID</label>
                <input className="input" value={form.accessKeyId} onChange={(e) => set("accessKeyId", e.target.value)} />
              </div>
              <div>
                <label className="label">Secret Access Key</label>
                <input className="input" type="password" value={form.secretAccessKey} onChange={(e) => set("secretAccessKey", e.target.value)} />
              </div>
              <div>
                <label className="label">Object Lock</label>
                <select className="select" value={form.objectLockMode} onChange={(e) => set("objectLockMode", e.target.value)}>
                  <option value="governance">Governance</option>
                  <option value="compliance">Compliance</option>
                  <option value="none">없음</option>
                </select>
              </div>
              {form.objectLockMode !== "none" && (
                <div>
                  <label className="label">보존일(days)</label>
                  <input className="input" type="number" value={form.retentionDays} onChange={(e) => set("retentionDays", Number(e.target.value))} />
                </div>
              )}
            </>
          )}

          {form.kind === "ssh" && (
            <>
              <div>
                <label className="label">호스트</label>
                <input className="input" value={form.host} onChange={(e) => set("host", e.target.value)} placeholder="bastion.example.com" />
              </div>
              <div>
                <label className="label">포트</label>
                <input className="input" type="number" value={form.port} onChange={(e) => set("port", Number(e.target.value))} />
              </div>
              <div>
                <label className="label">사용자</label>
                <input className="input" value={form.user} onChange={(e) => set("user", e.target.value)} placeholder="svc-backup" />
              </div>
              <div>
                <label className="label">원격 경로</label>
                <input className="input" value={form.path} onChange={(e) => set("path", e.target.value)} placeholder="/srv/backups" />
              </div>
              <div className="col-span">
                <label className="label">개인키 경로</label>
                <input className="input" value={form.privateKeyPath} onChange={(e) => set("privateKeyPath", e.target.value)} placeholder="C:\\keys\\id_ed25519" />
              </div>
              <div className="col-span">
                <label className="label">호스트키 지문(host key fingerprint)</label>
                <input className="input" value={form.hostKeyFingerprint} onChange={(e) => set("hostKeyFingerprint", e.target.value)} placeholder="SHA256:..." />
                <div className="field-hint">MITM 방어를 위해 필수 — blind-accept는 허용하지 않습니다.</div>
              </div>
            </>
          )}

          <div className="col-span" style={{ display: "flex", alignItems: "center", gap: 10 }}>
            <span className="label" style={{ margin: 0 }}>결과 불변성</span>
            <ImmutabilityBadge kind={previewImmut(form)} />
          </div>
        </div>

        {needsMasterPw && (
          <>
            <hr className="form-divider" />
            <div className="field" style={{ marginBottom: 0 }}>
              <label className="label">마스터 비밀번호 (자격증명 암호화)</label>
              <input className="input" type="password" value={masterPw} onChange={(e) => setMasterPw(e.target.value)} placeholder="초기 설정에서 정한 비밀번호" />
            </div>
          </>
        )}

        <div style={{ display: "flex", justifyContent: "flex-end", marginTop: "var(--sp-5)" }}>
          <button className="btn btn-primary" onClick={submit} disabled={!canSubmit}>
            {busy ? "등록 중…" : "목적지 등록"}
          </button>
        </div>
      </div>
    </div>
  );
}
