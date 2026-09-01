import type { ActivityEvent } from "../lib/types";
import { formatDateTime } from "../lib/format";
import { StatusBadge, toneForState } from "./StatusBadge";

interface VirtualActivityListProps {
  items: readonly ActivityEvent[];
  totalCount: number;
  selectedId: string | null;
  onSelect: (event: ActivityEvent) => void;
}

export function VirtualActivityList({
  items,
  totalCount,
  selectedId,
  onSelect,
}: VirtualActivityListProps) {
  return (
    <ol
      className="activity-list"
      aria-label="Activity events"
      aria-setsize={totalCount}
      data-rendered-count={items.length}
    >
      {items.map((event, index) => (
        <li
          key={event.id}
          aria-posinset={Math.max(1, totalCount - index)}
          aria-setsize={totalCount}
          data-selected={event.id === selectedId}
        >
          <button
            type="button"
            className="activity-row"
            aria-pressed={event.id === selectedId}
            onClick={() => onSelect(event)}
          >
            <time dateTime={new Date(event.occurredAtMs).toISOString()} className="mono">
              {formatDateTime(event.occurredAtMs)}
            </time>
            <StatusBadge
              label={event.severity}
              tone={toneForState(event.severity)}
            />
            <span className="activity-message">{event.message}</span>
            <span className="mono activity-code">{event.eventCode}</span>
          </button>
        </li>
      ))}
    </ol>
  );
}
