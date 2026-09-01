const dateFormatter = new Intl.DateTimeFormat("ko-KR", {
  month: "2-digit",
  day: "2-digit",
  hour: "2-digit",
  minute: "2-digit",
  hour12: false,
});

export function formatBytes(value: number | null): string {
  if (value === null || !Number.isFinite(value) || value < 0) return "근거 없음";
  if (value === 0) return "0 B";
  const units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
  const index = Math.min(
    units.length - 1,
    Math.floor(Math.log(value) / Math.log(1024)),
  );
  const scaled = value / 1024 ** index;
  return `${scaled.toFixed(index === 0 || scaled >= 100 ? 0 : 1)} ${units[index]}`;
}

export function formatDateTime(value: number | null): string {
  if (value === null || !Number.isFinite(value) || value < 0) return "기록 없음";
  return dateFormatter.format(new Date(value));
}

export function formatFreshness(value: number, now = Date.now()): string {
  const elapsed = Math.max(0, now - value);
  if (elapsed < 60_000) return "방금";
  if (elapsed < 3_600_000) return `${Math.floor(elapsed / 60_000)}분 전`;
  if (elapsed < 86_400_000) return `${Math.floor(elapsed / 3_600_000)}시간 전`;
  return `${Math.floor(elapsed / 86_400_000)}일 전`;
}

export function formatDuration(valueMs: number | null): string {
  if (valueMs === null || !Number.isFinite(valueMs) || valueMs < 0) return "설정 안 됨";
  if (valueMs < 60_000) return `${Math.round(valueMs / 1_000)}초`;
  if (valueMs < 3_600_000) return `${Math.round(valueMs / 60_000)}분`;
  if (valueMs < 86_400_000) {
    const hours = Math.floor(valueMs / 3_600_000);
    const minutes = Math.round((valueMs % 3_600_000) / 60_000);
    return minutes ? `${hours}시간 ${minutes}분` : `${hours}시간`;
  }
  return `${Math.round(valueMs / 86_400_000)}일`;
}

export function formatCount(value: number): string {
  return new Intl.NumberFormat("ko-KR").format(value);
}
