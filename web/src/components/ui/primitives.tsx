// Small stateless display primitives used across Awaken surfaces.

import { type ReactNode } from "react";
import {
  EmptyState as SharedEmptyState,
  JsonInspector as SharedJsonInspector,
  Skeleton as SharedSkeleton,
} from "@awaken/ui";
import { useApp } from "../../lib/app-state";
import type { SessionUsage } from "../../lib/api/types";

// ---- usage badges (tokens / cache / client-measured latency) ----
function cacheCreationTotal(usage: SessionUsage | undefined): number {
  return (usage?.cache_creation?.ephemeral_1h_input_tokens ?? 0)
    + (usage?.cache_creation?.ephemeral_5m_input_tokens ?? 0);
}

/** Sum of billable tokens in a usage record (input + output + both cache legs). */
export function usageTotal(u: SessionUsage | undefined): number {
  if (!u) return 0;
  return (
    (u.input_tokens ?? 0) +
    (u.output_tokens ?? 0) +
    (u.cache_read_input_tokens ?? 0) +
    cacheCreationTotal(u)
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
  const cache = (usage?.cache_read_input_tokens ?? 0) + cacheCreationTotal(usage);
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
  return (
    <SharedJsonInspector
      collapsed={collapsed}
      classes={{
        root: "json-inspector",
        header: "row",
        toggle: "btn ghost",
        copy: "btn ghost",
        body: "json-body mono",
      }}
      labels={{
        copy: app.t("Copy", "复制"),
        copied: app.t("Copied", "已复制"),
      }}
      value={value}
    />
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
