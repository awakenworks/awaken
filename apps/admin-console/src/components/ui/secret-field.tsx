import type { ReactNode } from "react";
import { SegmentedControl } from "./segmented";
import { Pill } from "./pill";

export type SecretMode = "keep" | "replace" | "clear";

interface SecretLabels {
  /** Plain title shown above the field. */
  title: string;
  /** One-line subtitle under the title. */
  description: ReactNode;
  /** Label for the "keep" segmented option. Default: "Keep current". */
  keepLabel?: string;
  /** Label for the "replace" segmented option. */
  replaceLabel: string;
  /** Label for the "clear" segmented option. */
  clearLabel: string;
  /** Sentence shown in the keep-mode body. */
  keepBody: ReactNode;
  /** Sentence shown in the clear-mode body (warning tone). */
  clearBody: ReactNode;
}

/**
 * Three-mode secret editor card. Mirrors the design's `.w2b-mode-card`
 * pattern: title row with status pill, segmented control for mode, and a
 * tone-coded body that swaps based on mode (keep = success / replace =
 * editor + hint / clear = warning).
 *
 * `currentlyHasValue` controls whether the "Keep current" option is offered
 * at all — for new records that have no stored value, only `replace` makes
 * sense.
 */
export function SecretField({
  mode,
  onModeChange,
  currentlyHasValue,
  statusPill,
  labels,
  children,
  hint,
}: {
  mode: SecretMode;
  onModeChange: (next: SecretMode) => void;
  currentlyHasValue: boolean;
  statusPill?: ReactNode;
  labels: SecretLabels;
  /** The editor surface (input / textarea) shown when mode === "replace". */
  children: ReactNode;
  hint?: ReactNode;
}) {
  const options = currentlyHasValue
    ? [
        { value: "keep" as const, label: labels.keepLabel ?? "Keep current" },
        { value: "replace" as const, label: labels.replaceLabel },
        { value: "clear" as const, label: labels.clearLabel },
      ]
    : [{ value: "replace" as const, label: labels.replaceLabel }];

  // When there is no stored value, "keep" and "clear" are nonsensical —
  // collapse the mode to "replace" so the body matches the only available
  // option in the segmented control.
  const effectiveMode: SecretMode = currentlyHasValue ? mode : "replace";

  return (
    <div className="rounded-md border border-line bg-surface p-4 shadow-card">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <h4 className="text-sm font-semibold text-fg-strong">{labels.title}</h4>
          <p className="mt-1 max-w-prose text-xs text-fg-soft">
            {labels.description}
          </p>
        </div>
        {statusPill}
      </div>

      <div className="mt-3">
        <SegmentedControl
          value={effectiveMode}
          onChange={onModeChange}
          options={options}
          ariaLabel={labels.title}
        />
      </div>

      <div className="mt-3">
        {effectiveMode === "keep" && (
          <ModeBody tone="success" icon="✓">
            {labels.keepBody}
          </ModeBody>
        )}
        {effectiveMode === "replace" && (
          <div className="space-y-2">
            {children}
            {hint && <p className="text-xs text-fg-soft">{hint}</p>}
          </div>
        )}
        {effectiveMode === "clear" && (
          <ModeBody tone="error" icon="×">
            {labels.clearBody}
          </ModeBody>
        )}
      </div>
    </div>
  );
}

function ModeBody({
  tone,
  icon,
  children,
}: {
  tone: "success" | "error";
  icon: string;
  children: ReactNode;
}) {
  const wrapperClass =
    tone === "success"
      ? "border-tone-success/30 bg-tone-success/10 text-fg"
      : "border-tone-error/30 bg-tone-error/10 text-fg";
  const iconClass =
    tone === "success"
      ? "bg-tone-success/15 text-tone-success"
      : "bg-tone-error/15 text-tone-error";
  return (
    <div
      className={`flex items-start gap-3 rounded-md border px-3 py-2 text-sm ${wrapperClass}`}
    >
      <span
        aria-hidden
        className={`mt-0.5 inline-flex h-5 w-5 items-center justify-center rounded-pill text-xs font-bold ${iconClass}`}
      >
        {icon}
      </span>
      <div>{children}</div>
    </div>
  );
}

/** Convenience pill for "stored / will-clear / no-value" states. */
export function SecretStatusPill({
  state,
  fingerprint,
}: {
  state: "stored" | "no-value" | "will-clear" | "will-set";
  fingerprint?: string;
}) {
  switch (state) {
    case "stored":
      return (
        <Pill tone="success">
          stored{fingerprint ? ` · fp:${fingerprint}` : ""}
        </Pill>
      );
    case "no-value":
      return <Pill tone="neutral">no value</Pill>;
    case "will-clear":
      return <Pill tone="error">will clear</Pill>;
    case "will-set":
      return <Pill tone="info">will set</Pill>;
  }
}
