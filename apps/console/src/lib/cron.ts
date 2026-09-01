// 아주 가벼운 cron → 사람이 읽는 미리보기. 흔한 케이스만 다루고 나머지는 원문 표시.
// 정밀 파싱/등록은 백엔드(dbbackup-scheduler)가 담당 — 여기선 UX 힌트일 뿐.

const DOW = ["일", "월", "화", "수", "목", "금", "토"];

export function cronPreview(expr: string): string {
  const s = expr.trim();
  if (!s) return "예약 없음";
  const parts = s.split(/\s+/);
  if (parts.length !== 5) return s;
  const [min, hour, dom, mon, dow] = parts;

  const hhmm = (h: string, m: string) => {
    const hi = Number(h);
    const mi = Number(m);
    if (Number.isNaN(hi) || Number.isNaN(mi)) return null;
    return `${String(hi).padStart(2, "0")}:${String(mi).padStart(2, "0")}`;
  };

  // */N * * * *  → N분마다
  const stepMin = min.match(/^\*\/(\d+)$/);
  if (stepMin && hour === "*" && dom === "*" && mon === "*" && dow === "*") {
    return `${stepMin[1]}분마다`;
  }

  if (/^\d+$/.test(min) && hour === "*" && dom === "*" && mon === "*" && dow === "*") {
    return `매시간 ${Number(min)}분`;
  }

  const t = hhmm(hour, min);
  if (t && dom === "*" && mon === "*") {
    if (dow === "*") return `매일 ${t}`;
    const d = Number(dow);
    if (!Number.isNaN(d) && d >= 0 && d <= 6) return `매주 ${DOW[d]} ${t}`;
    return `${s}`;
  }
  if (t && dow === "*" && mon === "*" && /^\d+$/.test(dom)) {
    return `매월 ${dom}일 ${t}`;
  }
  return s;
}
