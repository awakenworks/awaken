import type { ReactNode } from "react";

export type PillTone = "neutral" | "info" | "warn" | "success" | "error" | "agent";

const TONE_CLASS: Record<PillTone, string> = {
  neutral: "bg-muted text-fg-soft border-line",
  info: "bg-tone-info/10 text-tone-info border-tone-info/30",
  warn: "bg-tone-warn/15 text-tone-warn border-tone-warn/35",
  success: "bg-tone-success/15 text-tone-success border-tone-success/35",
  error: "bg-tone-error/15 text-tone-error border-tone-error/35",
  agent: "bg-agent-tint text-agent-fg border-agent-stripe/30",
};

export function Pill({
  tone = "neutral",
  className = "",
  title,
  children,
}: {
  tone?: PillTone;
  className?: string;
  title?: string;
  children: ReactNode;
}) {
  return (
    <span
      title={title}
      className={`inline-flex h-[22px] items-center gap-1.5 rounded-pill border px-2 text-xs font-medium ${TONE_CLASS[tone]} ${className}`.trim()}
    >
      {children}
    </span>
  );
}

/** Truncating chip with a "+N" overflow tail. */
export function PillStack({
  items,
  max = 3,
  tone = "neutral",
  empty = "—",
}: {
  items: string[];
  max?: number;
  tone?: PillTone;
  empty?: string;
}) {
  if (items.length === 0) return <span className="text-fg-faint">{empty}</span>;
  const visible = items.slice(0, max);
  const overflow = items.length - visible.length;
  return (
    <span className="flex flex-wrap items-center gap-1.5">
      {visible.map((label) => (
        <Pill key={label} tone={tone}>
          {label}
        </Pill>
      ))}
      {overflow > 0 && (
        <Pill tone="neutral" className="text-fg-faint" >
          +{overflow}
        </Pill>
      )}
    </span>
  );
}
