import type { ReactNode } from "react";

interface InspectorProps {
  title: string;
  eyebrow: string;
  onClose?: () => void;
  children: ReactNode;
  actions?: ReactNode;
}

export function Inspector({ title, eyebrow, onClose, children, actions }: InspectorProps) {
  return (
    <aside className="inspector" aria-label={`${title} details`}>
      <header className="inspector-head">
        <div>
          <span className="eyebrow">{eyebrow}</span>
          <h2 title={title}>{title}</h2>
        </div>
        {onClose && (
          <button type="button" className="icon-button" onClick={onClose} aria-label="Close details">
            ×
          </button>
        )}
      </header>
      <div
        className="inspector-body"
        tabIndex={0}
        aria-label={`${title} evidence`}
      >
        {children}
      </div>
      {actions && <footer className="inspector-actions">{actions}</footer>}
    </aside>
  );
}

export function EvidenceRow({
  label,
  value,
  mono = false,
}: {
  label: string;
  value: ReactNode;
  mono?: boolean;
}) {
  return (
    <div className="evidence-row">
      <dt>{label}</dt>
      <dd className={mono ? "mono" : undefined}>{value}</dd>
    </div>
  );
}
