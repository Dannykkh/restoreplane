import type { ActivityEvent } from "../lib/types";
import { formatDateTime } from "../lib/format";
import { StatusBadge, toneForState } from "./StatusBadge";

export function ActivityTimeline({ events }: { events: readonly ActivityEvent[] }) {
  return (
    <ol className="timeline" aria-label="Work item timeline">
      {events.map((event) => (
        <li key={event.id}>
          <span className="timeline-rail" aria-hidden="true" />
          <div className="timeline-content">
            <div className="timeline-title">
              <strong>{event.phase ?? event.eventCode}</strong>
              <StatusBadge label={event.severity} tone={toneForState(event.severity)} />
            </div>
            <p>{event.message}</p>
            <time className="mono" dateTime={new Date(event.occurredAtMs).toISOString()}>
              {formatDateTime(event.occurredAtMs)}
            </time>
          </div>
        </li>
      ))}
    </ol>
  );
}
