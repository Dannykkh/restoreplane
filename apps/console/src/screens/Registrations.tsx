import { useState } from "react";
import { Databases } from "./Databases";
import { Destinations } from "./Destinations";

type Tab = "db" | "dest";

export function Registrations() {
  const [tab, setTab] = useState<Tab>("db");
  return (
    <div>
      <div className="section-head">
        <h2>DB · 목적지 등록</h2>
        <div className="tabs">
          <button className={`tab ${tab === "db" ? "active" : ""}`} onClick={() => setTab("db")}>
            데이터베이스
          </button>
          <button className={`tab ${tab === "dest" ? "active" : ""}`} onClick={() => setTab("dest")}>
            목적지
          </button>
        </div>
      </div>
      {tab === "db" ? <Databases /> : <Destinations />}
    </div>
  );
}
