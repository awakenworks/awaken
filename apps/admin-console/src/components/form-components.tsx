import type { ReactNode } from "react";

export function Field({
  label,
  children,
  required = false,
  error,
  overridden = false,
  onReset,
  resetLabel,
}: {
  label: string;
  children: ReactNode;
  required?: boolean;
  error?: string | null;
  overridden?: boolean;
  onReset?: () => void;
  resetLabel?: string;
}) {
  // The required marker is rendered via the ::after pseudo-element so it
  // appears visually without contributing to label.textContent — RTL's
  // getByLabelText matches on textContent, not accessible name.
  const labelClassName = required
    ? "text-sm font-medium text-fg-soft after:ml-0.5 after:text-tone-error after:content-['*']"
    : "text-sm font-medium text-fg-soft";
  // The label-text + reset-button pair lives outside <label> because <button>
  // is a labelable element — nesting it would steal the implicit label
  // association from the wrapped form control.
  if (onReset) {
    return (
      <div className="block">
        <div className="mb-1.5 flex items-center gap-1.5">
          <span className={labelClassName}>{label}</span>
          {overridden ? (
            <button
              type="button"
              onClick={onReset}
              aria-label={resetLabel ?? `Reset ${label} to default`}
              title={resetLabel ?? `Reset ${label} to default`}
              className="inline-flex h-5 w-5 items-center justify-center rounded-full text-xs text-tone-warn transition hover:bg-tone-warn/15"
            >
              ↺
            </button>
          ) : null}
        </div>
        <label className="block">
          <span className="sr-only">{label}</span>
          {children}
        </label>
        {error ? (
          <span role="alert" className="mt-1 block text-xs text-tone-error">
            {error}
          </span>
        ) : null}
      </div>
    );
  }
  return (
    <label className="block">
      <span className={`mb-1.5 block ${labelClassName}`}>{label}</span>
      {children}
      {error ? (
        <span role="alert" className="mt-1 block text-xs text-tone-error">
          {error}
        </span>
      ) : null}
    </label>
  );
}

export function ModeButton({
  active,
  label,
  onClick,
}: {
  active: boolean;
  label: string;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={[
        "rounded-full px-3 py-1.5 text-xs font-medium transition",
        active
          ? "bg-accent text-accent-text"
          : "border border-line-strong bg-surface text-fg hover:bg-muted",
      ].join(" ")}
    >
      {label}
    </button>
  );
}

export function Hint({ children }: { children: ReactNode }) {
  return <div className="text-sm leading-6 text-fg-soft">{children}</div>;
}

export function SectionLabel({ label }: { label: string }) {
  return (
    <div className="text-xs font-semibold uppercase tracking-[0.18em] text-fg-soft">
      {label}
    </div>
  );
}

export function EmptyState({
  message,
  compact = false,
}: {
  message: string;
  compact?: boolean;
}) {
  return (
    <div
      className={[
        "rounded-sm border border-dashed border-line text-sm text-fg-soft",
        compact ? "px-4 py-3" : "mt-4 px-4 py-5",
      ].join(" ")}
    >
      {message}
    </div>
  );
}

export function MetricCard({
  label,
  value,
  detail,
}: {
  label: string;
  value: string;
  detail: string;
}) {
  return (
    <div className="rounded-sm border border-line bg-surface p-4">
      <div className="text-xs font-semibold uppercase tracking-[0.18em] text-fg-soft">
        {label}
      </div>
      <div className="mt-2 text-lg font-semibold text-fg-strong">{value}</div>
      <div className="mt-1 text-sm leading-6 text-fg-soft">{detail}</div>
    </div>
  );
}

export function Pill({
  label,
  active = false,
  tone = "slate",
}: {
  label: string;
  active?: boolean;
  tone?: "slate" | "amber";
}) {
  const palette =
    tone === "amber"
      ? active
        ? "bg-tone-warn/20 text-tone-warn"
        : "bg-tone-warn/15 text-tone-warn"
      : active
        ? "bg-fg text-bg"
        : "bg-muted text-fg-soft";
  return (
    <span className={`rounded-full px-2 py-0.5 text-xs font-medium ${palette}`}>
      {label}
    </span>
  );
}

export function ChoiceGrid<T extends string>({
  value,
  options,
  onChange,
  columns,
}: {
  value: T;
  options: Array<{
    value: T;
    label: string;
    description: string;
  }>;
  onChange: (value: T) => void;
  columns: string;
}) {
  return (
    <div className={["grid gap-2", columns].join(" ")}>
      {options.map((option) => {
        const selected = option.value === value;
        return (
          <button
            key={option.value}
            type="button"
            onClick={() => onChange(option.value)}
            className={[
              "rounded-sm border px-3 py-3 text-left transition",
              selected
                ? "border-accent bg-accent text-accent-text shadow-sm"
                : "border-line bg-surface text-fg-strong hover:border-line-strong hover:bg-soft",
            ].join(" ")}
          >
            <div className="text-sm font-semibold">{option.label}</div>
            <div
              className={[
                "mt-1 text-xs leading-5",
                selected ? "text-fg-faint" : "text-fg-soft",
              ].join(" ")}
            >
              {option.description}
            </div>
          </button>
        );
      })}
    </div>
  );
}
