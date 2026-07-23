// Small stateless display primitives used across Awaken surfaces.

import { useState, type ReactNode } from "react";
import {
  EmptyState as SharedEmptyState,
  Skeleton as SharedSkeleton,
} from "@awaken/ui";
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
    <SharedEmptyState
      className="empty-state"
      title={<span className="empty-title">{title}</span>}
      body={hint ? <span className="mut">{hint}</span> : undefined}
      actions={action}
    />
  );
}

// ---- skeletons ----
export function Skeleton({ width = "100%", height = 12 }: { width?: number | string; height?: number }) {
  return <SharedSkeleton className="skeleton" width={width} height={height} />;
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
