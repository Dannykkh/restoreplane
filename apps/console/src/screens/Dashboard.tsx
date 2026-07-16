import { useEffect, useState } from "react";
import type { DashboardData, JobSummary } from "../types";
import { getDashboard } from "../lib/api";
import { bytes, duration, ratio } from "../lib/format";
import { IconShield } from "../components/Icons";
import { ImmutabilityBadge, SignedBadge, StatusBadge, VerifyBadge } from "../components/Badges";

export function Dashboard() {
  const [data, setData] = useState<DashboardData | null>(null);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    getDashboard().then(setData).catch((e) => setErr(String(e)));
  }, []);

  if (err) return <div className="placeholder">대시보드 로드 실패: {err}</div>;
  if (!data) return <div className="placeholder muted">불러오는 중…</div>;

  const okCount = data.jobs.filter((j) => j.health === "ok").length;
  const total = data.jobs.length;
  const lockedCount = data.jobs.filter((j) =>
    j.lastBackup?.artifacts.some((a) => a.immutability !== "none"),
  ).length;

  // 전역 상태는 잡 health에서 유도한다 — 헤더가 카드와 모순되지 않게(하나라도 danger면 danger).
  const overall: DashboardData["overall"] =
    total === 0
      ? "empty"
      : data.jobs.some((j) => j.health === "danger")
        ? "danger"
        : data.jobs.some((j) => j.health === "warn")
          ? "warn"
          : "ok";

  const reassureClass = overall === "danger" ? "danger" : overall === "warn" ? "warn" : "";
  const title =
    overall === "ok"
      ? "모든 백업이 안전합니다"
      : overall === "warn"
        ? "대체로 안전 — 확인할 항목이 있습니다"
        : overall === "danger"
          ? "주의가 필요한 백업이 있습니다"
          : "아직 백업 이력이 없습니다";
  const sub =
    overall === "empty"
      ? "잡을 등록하고 첫 백업을 실행하세요."
      : `${total}개 잡 중 ${okCount}개 정상 · 최근 백업은 서명·검증되어 오프사이트에 보관됩니다.`;

  return (
    <div>
      <div className={`reassure ${reassureClass}`}>
        <div className="reassure-shield">
          <IconShield width={30} height={30} />
        </div>
        <div>
          <div className="reassure-title">{title}</div>
          <div className="reassure-sub">{sub}</div>
        </div>
        <div className="reassure-stats">
          <div>
            <div className="stat-num">
              {okCount}/{total}
            </div>
            <div className="stat-lbl">정상 잡</div>
          </div>
          <div>
            <div className="stat-num">{lockedCount}</div>
            <div className="stat-lbl">불변 보관</div>
          </div>
        </div>
      </div>

      <div className="section-head">
        <h2>백업 잡</h2>
        <span className="label">{total}개</span>
      </div>

      <div className="job-grid">
        {data.jobs.map((j) => (
          <JobCard key={j.name} job={j} />
        ))}
      </div>
    </div>
  );
}

function JobCard({ job }: { job: JobSummary }) {
  const b = job.lastBackup;
  return (
    <div className={`job-card ${job.health}`}>
      <div className="job-top">
        <div>
          <div className="job-name">{job.name}</div>
          <div className="job-meta">
            <span className="engine-tag">{job.engine}</span> · {job.source}
          </div>
        </div>
        {b && <StatusBadge status={b.status} />}
      </div>

      {b ? (
        <>
          <div className="badge-row">
            <VerifyBadge level={b.lastVerifyLevel} result={b.lastVerifyResult} />
            <SignedBadge signed={b.manifestSigned} />
          </div>

          <div>
            <div className="kv-row">
              <span className="k">마지막 백업</span>
              <span className="v">{b.startedAt}</span>
            </div>
            <div className="kv-row">
              <span className="k">소요 · 압축</span>
              <span className="v">
                {duration(b.startedAt, b.finishedAt)} · {ratio(b.plaintextBytes, b.cipherBytes)}
              </span>
            </div>
            <div className="kv-row">
              <span className="k">크기(원본 → 암호문)</span>
              <span className="v">
                {bytes(b.plaintextBytes)} → {bytes(b.cipherBytes)}
              </span>
            </div>
            <div className="kv-row">
              <span className="k">다음 예약</span>
              <span className="v">{job.schedule}</span>
            </div>
          </div>

          <div className="badge-row">
            {b.artifacts.map((a) => (
              <ImmutabilityBadge key={a.destName} kind={a.immutability} />
            ))}
          </div>
        </>
      ) : (
        <div className="muted" style={{ fontSize: "var(--fs-body-sm)" }}>
          아직 백업 이력이 없습니다.
        </div>
      )}
    </div>
  );
}
