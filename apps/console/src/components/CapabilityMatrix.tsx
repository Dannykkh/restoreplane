import { StatusBadge, type StatusTone } from "./StatusBadge";

export interface CapabilityItem {
  capability: string;
  status: "supported" | "limited" | "experimental" | "unsupported" | "unknown";
  evidence: string;
  limitation?: string;
}

const tone: Record<CapabilityItem["status"], StatusTone> = {
  supported: "success",
  limited: "warning",
  experimental: "warning",
  unsupported: "danger",
  unknown: "neutral",
};

export function CapabilityMatrix({ items }: { items: readonly CapabilityItem[] }) {
  return (
    <div className="capability-matrix" role="table" aria-label="Engine capability matrix">
      {items.map((item) => (
        <div className="capability-row" role="row" key={item.capability}>
          <span role="cell" className="capability-name">{item.capability}</span>
          <span role="cell">
            <StatusBadge
              label={item.status}
              tone={tone[item.status]}
              reason={item.limitation}
            />
          </span>
          <span role="cell" className="mono capability-evidence">{item.evidence}</span>
        </div>
      ))}
    </div>
  );
}
