// Small stateless display primitives shared across surfaces: source badges,
// used-by lists, sparklines, stat cards, a JSON inspector, empty states, skeletons.

import { useState, type ReactNode } from "react";
import { useApp } from "../../lib/app-state";
import type { SessionUsage } from "../../lib/api/types";

// ---- usage badges (tokens / cache / client-measured latency) ----
/** Sum of billable tokens in a usage record (input + output + both cache legs). */
export function usageTotal(u: SessionUsage | undefined): number {
  if (!u) return 0;
  return (
    (u.input_tokens ?? 0) +
    (u.output_tokens ?? 0) +
    (u.cache_read_input_tokens ?? 0) +
    (u.cache_creation_input_tokens ?? 0)
  );
}

export function UsageBadges({
  usage,
  latencyMs,
}: {
  usage?: SessionUsage;
  latencyMs?: number;
}) {
  const hasTokens = usageTotal(usage) > 0;
  const cache = (usage?.cache_read_input_tokens ?? 0) + (usage?.cache_creation_input_tokens ?? 0);
  if (!hasTokens && latencyMs == null) return null;
  return (
    <span className="row" style={{ gap: 6, flexWrap: "wrap" }}>
      {hasTokens && (
        <span className="pill neutral" title="input → output tokens">
          {usage!.input_tokens}↑ {usage!.output_tokens}↓
        </span>
      )}
      {cache > 0 && (
        <span className="pill neutral" title="cache read + creation tokens">
          ⚡ {cache} cache
        </span>
      )}
      {latencyMs != null && (
        <span className="pill neutral" title="round-trip latency">
          {latencyMs < 1000 ? `${latencyMs}ms` : `${(latencyMs / 1000).toFixed(1)}s`}
        </span>
      )}
    </span>
  );
}

// ---- source-state badge (builtin / customized / user-defined) ----
export type SourceState = "builtin" | "customized" | "user-defined";
export function SourceBadge({ source }: { source: SourceState }) {
  const app = useApp();
  const label: Record<SourceState, [string, string]> = {
    builtin: ["built-in", "内置"],
    customized: ["customized", "已定制"],
    "user-defined": ["user-defined", "自定义"],
  };
  const cls = source === "builtin" ? "neutral" : source === "customized" ? "warn" : "agent";
  return <span className={`pill ${cls}`}>{app.t(...label[source])}</span>;
}

// ---- used-by list (who references this resource) ----
export function UsedByList({
  items,
  onOpen,
}: {
  items: { id: string; label?: string }[];
  onOpen?: (id: string) => void;
}) {
  const app = useApp();
  if (items.length === 0) return <span className="mut">{app.t("Used by nothing.", "无引用。")}</span>;
  return (
    <div className="chain">
      <span className="mut">{app.t("Used by", "被引用")}</span>
      {items.map((it) => (
        <button
          key={it.id}
          className="chip"
          style={{ cursor: onOpen ? "pointer" : "default" }}
          onClick={() => onOpen?.(it.id)}
        >
          <span className="mono">{it.label ?? it.id}</span>
        </button>
      ))}
    </div>
  );
}

// ---- sparkline (tiny inline trend) ----
export function Sparkline({ values, width = 96, height = 22 }: { values: number[]; width?: number; height?: number }) {
  if (values.length < 2) return <span className="mut">—</span>;
  const max = Math.max(...values, 1);
  const min = Math.min(...values, 0);
  const span = max - min || 1;
  const step = width / (values.length - 1);
  const pts = values
    .map((v, i) => `${(i * step).toFixed(1)},${(height - ((v - min) / span) * height).toFixed(1)}`)
    .join(" ");
  return (
    <svg width={width} height={height} style={{ display: "block" }} aria-hidden>
      <polyline points={pts} fill="none" stroke="var(--accent)" strokeWidth={1.5} strokeLinejoin="round" />
    </svg>
  );
}

// ---- stat card ----
export function StatCard({
  label,
  value,
  hint,
  onClick,
}: {
  label: string;
  value: ReactNode;
  hint?: ReactNode;
  onClick?: () => void;
}) {
  return (
    <button className="kpi" onClick={onClick} style={{ cursor: onClick ? "pointer" : "default" }}>
      <span className="val">{value}</span>
      <span className="label">{label}</span>
      {hint != null && <span className="mut" style={{ fontSize: 11 }}>{hint}</span>}
    </button>
  );
}

// ---- JSON inspector (pretty, collapsible, copyable) ----
export function JsonInspector({ value, collapsed = false }: { value: unknown; collapsed?: boolean }) {
  const app = useApp();
  const [open, setOpen] = useState(!collapsed);
  const text = JSON.stringify(value, null, 2);
  return (
    <div className="json-inspector">
      <div className="row" style={{ justifyContent: "space-between" }}>
        <button className="btn ghost" style={{ height: 22 }} onClick={() => setOpen((v) => !v)}>
          {open ? "▾" : "▸"} JSON
        </button>
        <button className="btn ghost" style={{ height: 22 }} onClick={() => void navigator.clipboard.writeText(text)}>
          {app.t("Copy", "复制")}
        </button>
      </div>
      {open && <pre className="json-body mono">{text}</pre>}
    </div>
  );
}

// ---- empty state ----
export function EmptyState({ title, hint, action }: { title: string; hint?: string; action?: ReactNode }) {
  return (
    <div className="empty-state">
      <div className="empty-title">{title}</div>
      {hint && <div className="mut">{hint}</div>}
      {action && <div style={{ marginTop: 8 }}>{action}</div>}
    </div>
  );
}

// ---- skeletons ----
export function Skeleton({ width = "100%", height = 12 }: { width?: number | string; height?: number }) {
  return <span className="skeleton" style={{ width, height, display: "inline-block" }} />;
}
export function SkeletonRows({ rows = 4, cols = 4 }: { rows?: number; cols?: number }) {
  return (
    <tbody>
      {Array.from({ length: rows }).map((_, r) => (
        <tr key={r}>
          {Array.from({ length: cols }).map((_, c) => (
            <td key={c}>
              <Skeleton width={c === 0 ? "60%" : "80%"} />
            </td>
          ))}
        </tr>
      ))}
    </tbody>
  );
}
