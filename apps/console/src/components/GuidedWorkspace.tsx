import type { ReactNode } from "react";

interface GuidedWorkspaceProps {
  title: string;
  description: string;
  steps: readonly string[];
  currentStep: number;
  children: ReactNode;
  footer: ReactNode;
}

export function GuidedWorkspace({
  title,
  description,
  steps,
  currentStep,
  children,
  footer,
}: GuidedWorkspaceProps) {
  return (
    <section className="guided-workspace" aria-labelledby="guided-title">
      <header className="guided-head">
        <div>
          <span className="eyebrow">Guided setup</span>
          <h2 id="guided-title">{title}</h2>
          <p>{description}</p>
        </div>
        <ol className="guided-steps" aria-label="Progress">
          {steps.map((step, index) => (
            <li
              key={step}
              aria-current={index === currentStep ? "step" : undefined}
              data-state={index < currentStep ? "complete" : index === currentStep ? "current" : "upcoming"}
            >
              <span className="mono">{String(index + 1).padStart(2, "0")}</span>
              {step}
            </li>
          ))}
        </ol>
      </header>
      <div className="guided-content">{children}</div>
      <footer className="guided-footer">{footer}</footer>
    </section>
  );
}
