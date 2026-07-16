// 기술값 포매팅(모노스페이스로 렌더되는 값들).

export function bytes(n: number | null): string {
  if (n === null || n === undefined) return "—";
  if (n === 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  const i = Math.min(units.length - 1, Math.floor(Math.log(n) / Math.log(1024)));
  const v = n / Math.pow(1024, i);
  return `${v.toFixed(i === 0 ? 0 : v >= 100 ? 0 : 1)} ${units[i]}`;
}

export function ratio(plain: number | null, cipher: number | null): string {
  if (!plain || !cipher) return "—";
  return `${(plain / cipher).toFixed(1)}×`;
}

export function duration(startIso: string, endIso: string | null): string {
  if (!endIso) return "—";
  const ms = new Date(endIso.replace(" ", "T")).getTime() - new Date(startIso.replace(" ", "T")).getTime();
  if (Number.isNaN(ms) || ms < 0) return "—";
  const s = Math.round(ms / 1000);
  if (s < 60) return `${s}초`;
  const m = Math.floor(s / 60);
  const rs = s % 60;
  return rs ? `${m}분 ${rs}초` : `${m}분`;
}

export function agoFrom(nowIso: string, thenIso: string): string {
  const then = new Date(thenIso.replace(" ", "T")).getTime();
  const now = new Date(nowIso.replace(" ", "T")).getTime();
  const s = Math.max(0, Math.round((now - then) / 1000));
  if (s < 3600) return `${Math.max(1, Math.floor(s / 60))}분 전`;
  if (s < 86400) return `${Math.floor(s / 3600)}시간 전`;
  return `${Math.floor(s / 86400)}일 전`;
}
