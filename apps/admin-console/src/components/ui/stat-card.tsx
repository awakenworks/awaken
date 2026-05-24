import type { ReactNode } from "react";

export type StatTone = "neutral" | "info" | "success" | "warn" | "error";

/** Visual layout.
 *  - `block` (default): large value on top, soft label below. Used in
 *    full-bleed dashboards (per-agent runtime stats, eval reports) where
 *    each card stands alone.
 *  - `compact`: small uppercase label on top, value below in monospace.
 *    Used inside dense grids on the main dashboard where four-or-more
 *    tiles share a row. */
type StatLayout = "block" | "compact";

interface BaseProps {
  label: string;
  value: ReactNode;
  /** Secondary line under the value. Surface a breakdown ("3,200 in · 1,800 out")
   *  or a derived rate ("2.1% errors"). */
  sub?: ReactNode;
  tone?: StatTone;
  /** Force monospace + tabular numbers on the value. Defaults to true for
   *  the compact layout (it's a number grid) and false for block. */
  mono?: boolean;
}

interface BlockStatCardProps extends BaseProps {
  layout?: "block";
  emphasis?: never;
}

interface CompactStatCardProps extends BaseProps {
  layout: "compact";
  /** Size of the value text. `md` (default) is `text-2xl`; `lg` is
   *  `text-4xl` and is used to make a single tile the hero of a row
   *  (e.g. HITL waiting count). */
  emphasis?: "md" | "lg";
}

export type StatCardProps = BlockStatCardProps | CompactStatCardProps;

const TONE_TEXT: Record<StatTone, string> = {
  neutral: "text-fg-strong",
  info: "text-tone-info",
  success: "text-tone-success",
  warn: "text-tone-warn",
  error: "text-tone-error",
};

export function StatCard(props: StatCardProps) {
  const { label, value, sub, tone = "neutral" } = props;
  const layout: StatLayout = props.layout ?? "block";
  const valueColor = TONE_TEXT[tone];

  if (layout === "compact") {
    const emphasis = props.emphasis ?? "md";
    const sizeClass = emphasis === "lg" ? "text-4xl" : "text-2xl";
    const monoClass = (props.mono ?? true) ? "font-mono tabular-nums" : "";
    return (
      <div className="rounded-sm border border-line bg-soft px-3 py-3">
        <div className="text-[10px] font-medium uppercase tracking-[0.18em] text-fg-faint">
          {label}
        </div>
        <div className={`mt-1 font-semibold ${monoClass} ${sizeClass} ${valueColor}`.trim()}>
          {value}
        </div>
        {sub && <div className="mt-1 text-xs text-fg-soft">{sub}</div>}
      </div>
    );
  }

  const monoClass = props.mono ? "font-mono tabular-nums" : "";
  return (
    <div className="rounded-sm border border-line bg-surface p-5 shadow-sm">
      <div className={`text-3xl font-semibold ${monoClass} ${valueColor}`.trim()}>{value}</div>
      <div className="mt-2 text-sm text-fg-soft">{label}</div>
      {sub && <div className="mt-1 text-xs text-fg-faint">{sub}</div>}
    </div>
  );
}
