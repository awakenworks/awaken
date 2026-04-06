import type { ReactNode } from "react";

export function Field({
  label,
  children,
}: {
  label: string;
  children: ReactNode;
}) {
  return (
    <label className="block">
      <span className="mb-1.5 block text-sm font-medium text-slate-600">{label}</span>
      {children}
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
          ? "bg-slate-950 text-white"
          : "border border-slate-300 bg-white text-slate-700 hover:bg-slate-100",
      ].join(" ")}
    >
      {label}
    </button>
  );
}

export function Hint({ children }: { children: ReactNode }) {
  return <div className="text-sm leading-6 text-slate-500">{children}</div>;
}

export function SectionLabel({ label }: { label: string }) {
  return (
    <div className="text-xs font-semibold uppercase tracking-[0.18em] text-slate-500">
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
        "rounded-2xl border border-dashed border-slate-200 text-sm text-slate-500",
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
    <div className="rounded-2xl border border-slate-200 bg-white p-4">
      <div className="text-xs font-semibold uppercase tracking-[0.18em] text-slate-500">
        {label}
      </div>
      <div className="mt-2 text-lg font-semibold text-slate-950">{value}</div>
      <div className="mt-1 text-sm leading-6 text-slate-500">{detail}</div>
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
        ? "bg-amber-200 text-amber-950"
        : "bg-amber-100 text-amber-700"
      : active
        ? "bg-slate-700 text-slate-100"
        : "bg-slate-200 text-slate-600";
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
              "rounded-2xl border px-3 py-3 text-left transition",
              selected
                ? "border-slate-900 bg-slate-900 text-white shadow-sm"
                : "border-slate-200 bg-white text-slate-900 hover:border-slate-300 hover:bg-slate-50",
            ].join(" ")}
          >
            <div className="text-sm font-semibold">{option.label}</div>
            <div
              className={[
                "mt-1 text-xs leading-5",
                selected ? "text-slate-300" : "text-slate-500",
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
