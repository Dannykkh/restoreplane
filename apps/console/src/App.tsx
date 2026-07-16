import { useState } from "react";
import "./App.css";
import { Dashboard } from "./screens/Dashboard";
import {
  IconCalendar,
  IconDashboard,
  IconDatabase,
  IconPlay,
  IconRestore,
  IconShield,
} from "./components/Icons";

type View = "dashboard" | "databases" | "run" | "restore" | "schedule";

const NAV: { id: View; label: string; icon: typeof IconDashboard }[] = [
  { id: "dashboard", label: "대시보드", icon: IconDashboard },
  { id: "databases", label: "DB · 목적지", icon: IconDatabase },
  { id: "run", label: "백업 실행", icon: IconPlay },
  { id: "restore", label: "복원", icon: IconRestore },
  { id: "schedule", label: "스케줄 · 알림", icon: IconCalendar },
];

export function App() {
  const [view, setView] = useState<View>("dashboard");

  return (
    <div className="shell">
      <aside className="sidebar">
        <div className="brand">
          <span className="brand-mark">
            <IconShield width={16} height={16} />
          </span>
          <div>
            <div className="brand-name">dbbackup</div>
            <div className="brand-sub">SECURE CONSOLE</div>
          </div>
        </div>

        <nav className="nav">
          {NAV.map((n) => {
            const Icon = n.icon;
            return (
              <button
                key={n.id}
                className={`nav-item ${view === n.id ? "active" : ""}`}
                onClick={() => setView(n.id)}
              >
                <Icon className="ico" />
                {n.label}
              </button>
            );
          })}
        </nav>

        <div className="nav-spacer" />
        <div className="sidebar-foot">
          <span className="mono">v0.1.0</span>
          <span>랜섬웨어 저항 백업</span>
        </div>
      </aside>

      <main className="main">
        <div className="main-inner">
          {view === "dashboard" && <Dashboard />}
          {view !== "dashboard" && <Placeholder view={view} />}
        </div>
      </main>
    </div>
  );
}

const TITLES: Record<Exclude<View, "dashboard">, string> = {
  databases: "DB · 목적지 등록",
  run: "백업 실행",
  restore: "복원 마법사",
  schedule: "스케줄 · 알림",
};

function Placeholder({ view }: { view: Exclude<View, "dashboard"> }) {
  return (
    <div className="placeholder">
      <div className="p-mark">
        <IconShield width={26} height={26} />
      </div>
      <div>
        <h2 style={{ marginBottom: 6 }}>{TITLES[view]}</h2>
        <div className="muted">다음 증분에서 연결됩니다.</div>
      </div>
    </div>
  );
}
