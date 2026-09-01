import { formatFreshness } from "../lib/format";

export type StatusTone = "success" | "warning" | "danger" | "immutable" | "neutral" | "info";

interface StatusBadgeProps {
  label: string;
  tone?: StatusTone;
  reason?: string;
  evidenceAtMs?: number | null;
}

export function StatusBadge({
  label,
  tone = "neutral",
  reason,
  evidenceAtMs,
}: StatusBadgeProps) {
  const freshness = evidenceAtMs ? formatFreshness(evidenceAtMs) : null;
  const description = [reason, freshness ? `근거 ${freshness}` : null]
    .filter(Boolean)
    .join(" · ");
  return (
    <span
      className={`status-badge status-${tone}`}
      title={description || undefined}
      data-tone={tone}
    >
      <span className="status-marker" aria-hidden="true" />
      <span>{label}</span>
      {freshness && <span className="status-freshness">{freshness}</span>}
      {description && <span className="sr-only">. {description}</span>}
    </span>
  );
}

export function toneForState(state: string): StatusTone {
  if (["passed", "succeeded", "ready", "compliant", "healthy", "eligible", "complete", "enabled", "committed", "valid"].includes(state)) {
    return "success";
  }
  if (["failed", "critical", "error", "blocked", "unavailable", "non_compliant", "invalid", "failed_terminal"].includes(state)) {
    return "danger";
  }
  if (["warning", "attention", "partial", "degraded", "conditional", "delayed", "verification_overdue", "rpo_violation"].includes(state)) {
    return "warning";
  }
  if (["governance", "compliance", "server_append_only", "signed"].includes(state)) {
    return "immutable";
  }
  return "neutral";
}
