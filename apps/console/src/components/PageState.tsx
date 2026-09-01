import type { ReactNode } from "react";
import type { ServiceError } from "../lib/errors";

interface PageStateProps {
  kind: "loading" | "empty" | "error" | "stale" | "partial" | "permission";
  title: string;
  detail: string;
  error?: ServiceError | null;
  action?: ReactNode;
}

export function PageState({ kind, title, detail, error, action }: PageStateProps) {
  return (
    <section className={`page-state page-state-${kind}`} aria-live={kind === "error" ? "assertive" : "polite"}>
      <span className="state-rule" aria-hidden="true" />
      <div>
        <h2>{title}</h2>
        <p>{detail}</p>
        {error && (
          <p className="state-action-copy">
            {error.action}
            <span className="mono error-code"> {error.code}</span>
          </p>
        )}
      </div>
      {action && <div className="page-state-action">{action}</div>}
    </section>
  );
}

export function LoadingState({ label = "근거를 불러오는 중입니다." }: { label?: string }) {
  return (
    <div className="loading-line" role="status">
      <span aria-hidden="true" />
      {label}
    </div>
  );
}
