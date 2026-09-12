// The Quickstart arc as one rail: create the Agent, configure where it runs,
// start the Session, then integrate. It spans two routes — the editor owns the
// first three steps and the durable Session route owns the fourth — so the rail
// is derived from where the operator already is (the editor stage, then the
// Session route's presentational `from=quickstart` provenance). It never reads
// or writes onboarding state, and it is guidance only: nothing about publishing
// or running is gated on it.

import { useApp } from "../../lib/app-state";

export const QUICKSTART_STEPS = ["agent", "environment", "session", "integrate"] as const;
export type QuickstartStep = typeof QUICKSTART_STEPS[number];

/** Ordinal position of a step, used to classify every other step as done,
 * current, or upcoming. Exported so the projection stays testable apart from
 * the markup that renders it. */
export function quickstartStepIndex(step: QuickstartStep): number {
  return QUICKSTART_STEPS.indexOf(step);
}

export type QuickstartStepState = "done" | "current" | "upcoming";

export function quickstartStepState(step: QuickstartStep, current: QuickstartStep): QuickstartStepState {
  const position = quickstartStepIndex(step);
  const active = quickstartStepIndex(current);
  if (position < active) return "done";
  return position === active ? "current" : "upcoming";
}

export default function QuickstartProgress({
  current,
  className,
}: {
  current: QuickstartStep;
  className?: string;
}) {
  const app = useApp();
  const labels: Record<QuickstartStep, string> = {
    agent: app.t("Create Agent", "创建 Agent"),
    environment: app.t("Configure environment", "配置运行环境"),
    session: app.t("Start Session", "启动会话"),
    integrate: app.t("Integrate", "接入"),
  };
  return (
    <ol
      className={className ? `quickstart-progress ${className}` : "quickstart-progress"}
      aria-label={app.t("Quickstart progress", "Quickstart 进度")}
    >
      {QUICKSTART_STEPS.map((step, index) => {
        const state = quickstartStepState(step, current);
        return (
          <li
            key={step}
            className="quickstart-progress-step"
            data-state={state}
            aria-current={state === "current" ? "step" : undefined}
          >
            <span className="quickstart-progress-index" aria-hidden="true">{index + 1}</span>
            <span className="quickstart-progress-label">{labels[step]}</span>
          </li>
        );
      })}
    </ol>
  );
}
