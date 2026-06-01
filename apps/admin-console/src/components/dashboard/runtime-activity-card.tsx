import { useMemo } from "react";
import { useTranslation } from "react-i18next";
import { Link } from "react-router";
import { Eyebrow } from "@/components/ui/eyebrow";
import { FeatureDisabledNotice } from "@/components/ui/feature-disabled-notice";
import { StatCard } from "@/components/ui/stat-card";
import { adminRoutes } from "@/lib/routes";
import type { AgentRuntimeSnapshot } from "@/lib/config-api";

/** Color-blind-safe threshold for "noticeable error rate". 5% is the
 *  point where a stat tile turns warn and a per-tool row gains a warn
 *  tone. Centralised so the rule is auditable. */
const ERROR_RATE_WARN = 0.05;

/** State shape mirrors `WorkloadState` so both top cards distinguish
 *  loading / route_absent / registry_unavailable / error / ready the
 *  same way. The dashboard derives this from the runtime-stats slot. */
export type RuntimeActivityState =
  | { kind: "loading" }
  | { kind: "route_absent" }
  | { kind: "registry_unavailable" }
  | { kind: "error"; message: string }
  | { kind: "ready"; snapshots: AgentRuntimeSnapshot[] };

/** Aggregated, dashboard-wide view of the runtime stats registry. The
 *  card is intentionally point-in-time (matching the underlying
 *  registry's bucket window) — for time-series, drill into the
 *  per-agent dashboard. */
export function RuntimeActivityCard({ state }: { state: RuntimeActivityState }) {
  const { t } = useTranslation();
  const snapshots = state.kind === "ready" ? state.snapshots : null;
  const aggregated = useMemo(
    () => (snapshots ? aggregateRuntimeStats(snapshots) : null),
    [snapshots],
  );

  // All variants share `aria-live="polite"` so transitions between
  // loading → ready, ready → error, etc. are announced once. The
  // skeleton variant additionally sets `aria-busy="true"`.
  if (state.kind === "loading") {
    return (
      <div
        className="rounded-sm border border-line bg-surface p-5 shadow-card"
        aria-live="polite"
        aria-busy="true"
        aria-label={t("dashboard.agentActivity.loading")}
      >
        <h2 className="text-lg font-semibold text-fg-strong">
          {t("dashboard.agentActivity.title")}
        </h2>
        <div className="mt-4 grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
          {[0, 1, 2, 3].map((i) => (
            <div key={i} className="h-20 animate-pulse rounded-sm bg-soft" />
          ))}
        </div>
      </div>
    );
  }

  if (state.kind === "route_absent" || state.kind === "registry_unavailable") {
    const title =
      state.kind === "route_absent"
        ? t("dashboard.agentActivity.routeAbsentTitle")
        : t("dashboard.agentActivity.disabledTitle");
    const hint =
      state.kind === "route_absent"
        ? t("dashboard.agentActivity.routeAbsentHint")
        : t("dashboard.agentActivity.disabledHint");
    return (
      <div
        className="rounded-sm border border-line bg-surface p-5 shadow-card"
        aria-live="polite"
      >
        <h2 className="text-lg font-semibold text-fg-strong">{t("dashboard.agentActivity.title")}</h2>
        <FeatureDisabledNotice
          title={title}
          configHint={hint}
        />
      </div>
    );
  }

  if (state.kind === "error") {
    return (
      <div
        className="rounded-sm border border-tone-error/30 bg-tone-error/[0.06] p-5 shadow-card"
        aria-live="polite"
      >
        <h2 className="text-lg font-semibold text-fg-strong">{t("dashboard.agentActivity.title")}</h2>
        <div className="mt-3 text-sm text-tone-error">
          <span className="font-medium">{t("dashboard.agentActivity.errorTitle")}: </span>
          {state.message}
        </div>
      </div>
    );
  }

  if (!aggregated || aggregated.totalInferences === 0) {
    return (
      <div
        className="rounded-sm border border-line bg-surface p-5 shadow-card"
        aria-live="polite"
        aria-atomic="false"
      >
        <h2 className="text-lg font-semibold text-fg-strong">{t("dashboard.agentActivity.title")}</h2>
        <p className="mt-3 text-sm text-fg-soft">{t("dashboard.agentActivity.empty")}</p>
      </div>
    );
  }

  // Mixed `window_seconds` across snapshots: summing them is silently
  // misleading (an "errors" total mixing 1h + 24h windows reads like a
  // single comparable rate). Refuse to aggregate; surface a degraded
  // notice with the same card chrome instead.
  if (aggregated.mixedWindows) {
    return (
      <div
        className="rounded-sm border border-line bg-surface p-5 shadow-card"
        aria-live="polite"
        aria-atomic="false"
      >
        <h2 className="text-lg font-semibold text-fg-strong">{t("dashboard.agentActivity.title")}</h2>
        <p className="mt-3 text-sm text-tone-warn">
          {t("dashboard.agentActivity.mixedWindowsHint")}
        </p>
        <p className="mt-2 text-xs text-fg-soft">
          {t("dashboard.agentActivity.mixedWindowsDegraded")}
        </p>
      </div>
    );
  }

  const errorRate =
    aggregated.totalInferences > 0
      ? aggregated.totalErrors / aggregated.totalInferences
      : 0;
  const errorTone =
    errorRate >= ERROR_RATE_WARN ? "warn" : aggregated.totalErrors > 0 ? "info" : "neutral";

  return (
    // aria-live="polite" + aria-atomic="false" so the per-card poll
    // refresh announces the new aggregate without re-reading the
    // header text every 30 seconds.
    <div
      className="rounded-sm border border-line bg-surface p-5 shadow-card"
      aria-live="polite"
      aria-atomic="false"
    >
      <div className="flex items-baseline justify-between gap-4">
        <h2 className="text-lg font-semibold text-fg-strong">{t("dashboard.agentActivity.title")}</h2>
        {/* Mixed-window snapshots are surfaced as a degraded notice
            higher up; by the time we render the comparable-window
            view, every aggregated row shares one `window_seconds`. */}
        <span className="text-xs text-fg-soft">
          {t("dashboard.agentActivity.window", {
            window: formatWindowSecondsI18n(t, aggregated.windowSeconds),
          })}
        </span>
      </div>

      <div className="mt-4 grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
        <StatCard
          layout="compact"
          label={t("dashboard.agentActivity.inferences")}
          value={aggregated.totalInferences.toLocaleString()}
        />
        <StatCard
          layout="compact"
          label={t("dashboard.agentActivity.errors")}
          value={aggregated.totalErrors.toLocaleString()}
          sub={t("dashboard.agentActivity.errorRate", { pct: (errorRate * 100).toFixed(1) })}
          tone={errorTone}
        />
        <StatCard
          layout="compact"
          label={t("dashboard.agentActivity.tokens")}
          value={(aggregated.totalInputTokens + aggregated.totalOutputTokens).toLocaleString()}
          sub={t("dashboard.agentActivity.tokensInOut", {
            input: aggregated.totalInputTokens.toLocaleString(),
            output: aggregated.totalOutputTokens.toLocaleString(),
          })}
        />
        <StatCard
          layout="compact"
          label={t("dashboard.agentActivity.coordination")}
          value={(
            aggregated.totalSuspensions +
            aggregated.totalHandoffs +
            aggregated.totalDelegations
          ).toLocaleString()}
          sub={t("dashboard.agentActivity.coordinationBreakdown", {
            suspensions: aggregated.totalSuspensions.toLocaleString(),
            handoffs: aggregated.totalHandoffs.toLocaleString(),
            delegations: aggregated.totalDelegations.toLocaleString(),
          })}
        />
      </div>

      <div className="mt-5 grid gap-4 lg:grid-cols-2">
        <ActivityRankList
          title={t("dashboard.agentActivity.topAgents")}
          rows={aggregated.topAgents.map((row) => ({
            id: row.agentId,
            primary: row.agentId,
            href: adminRoutes.agentDashboard(row.agentId),
            count: row.inferences,
            metric: t("dashboard.agentActivity.inferences"),
          }))}
        />
        <ActivityRankList
          title={t("dashboard.agentActivity.topTools")}
          rows={aggregated.topTools.map((row) => ({
            id: row.tool,
            primary: row.tool,
            // Tool ids with "/" are MCP-provided (`mcp/server/tool`);
            // they have no admin-console detail page, so render as
            // plain text rather than a 404 link. Native tools link.
            href: row.tool.includes("/") ? undefined : adminRoutes.tool(row.tool),
            count: row.calls,
            metric: t("dashboard.agentActivity.toolCalls"),
            tone: row.failureRate >= ERROR_RATE_WARN ? "warn" : undefined,
            metricExtra:
              row.failures > 0
                ? t("dashboard.agentActivity.toolFailures", {
                    failures: row.failures.toLocaleString(),
                    pct: (row.failureRate * 100).toFixed(1),
                  })
                : undefined,
          }))}
        />
      </div>
    </div>
  );
}

interface RankRow {
  id: string;
  primary: string;
  href?: string;
  count: number;
  metric: string;
  metricExtra?: string;
  tone?: "warn";
}

function ActivityRankList({ title, rows }: { title: string; rows: RankRow[] }) {
  return (
    <div>
      <Eyebrow>{title}</Eyebrow>
      {rows.length === 0 ? (
        <p className="mt-2 text-sm text-fg-soft">—</p>
      ) : (
        <ul className="mt-2 space-y-1.5">
          {rows.map((row) => {
            const countCls = row.tone === "warn" ? "text-tone-warn" : "text-fg-strong";
            return (
              <li
                key={row.id}
                className="flex items-baseline justify-between gap-3 rounded-sm border border-line bg-soft px-3 py-1.5"
              >
                <div className="min-w-0 truncate font-mono text-sm text-fg-strong">
                  {row.href ? (
                    <Link to={row.href} className="transition-colors hover:text-link-hover">
                      {row.primary}
                    </Link>
                  ) : (
                    row.primary
                  )}
                </div>
                <div className="shrink-0 text-right text-xs text-fg-soft">
                  <span className={`font-mono font-semibold tabular-nums ${countCls}`}>
                    {row.count.toLocaleString()}
                  </span>{" "}
                  {row.metric}
                  {row.metricExtra && (
                    <span className="ml-2 text-tone-warn">{row.metricExtra}</span>
                  )}
                </div>
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}

interface AggregatedRuntimeStats {
  totalInferences: number;
  totalErrors: number;
  totalInputTokens: number;
  totalOutputTokens: number;
  totalSuspensions: number;
  totalHandoffs: number;
  totalDelegations: number;
  /** Common window across all snapshots, or `0` when snapshots disagree
   *  (`mixedWindows` is then `true` and the card surfaces a warning
   *  instead of a misleading "last 24h" label). */
  windowSeconds: number;
  mixedWindows: boolean;
  topAgents: Array<{ agentId: string; inferences: number; errors: number }>;
  topTools: Array<{ tool: string; calls: number; failures: number; failureRate: number }>;
}

function aggregateRuntimeStats(snapshots: AgentRuntimeSnapshot[]): AggregatedRuntimeStats {
  let totalInferences = 0;
  let totalErrors = 0;
  let totalInputTokens = 0;
  let totalOutputTokens = 0;
  let totalSuspensions = 0;
  let totalHandoffs = 0;
  let totalDelegations = 0;
  let firstWindow: number | null = null;
  let mixedWindows = false;
  const toolAggregate = new Map<string, { calls: number; failures: number }>();
  const agentAggregate: Array<{ agentId: string; inferences: number; errors: number }> = [];

  for (const snap of snapshots) {
    totalInferences += snap.inference_count;
    totalErrors += snap.error_count;
    totalInputTokens += snap.input_tokens;
    totalOutputTokens += snap.output_tokens;
    totalSuspensions += snap.suspensions;
    totalHandoffs += snap.handoffs;
    totalDelegations += snap.delegations;
    if (firstWindow === null) {
      firstWindow = snap.window_seconds;
    } else if (snap.window_seconds !== firstWindow) {
      mixedWindows = true;
    }
    agentAggregate.push({
      agentId: snap.agent_id,
      inferences: snap.inference_count,
      errors: snap.error_count,
    });
    for (const tool of snap.tool_calls_by_tool) {
      const entry = toolAggregate.get(tool.tool) ?? { calls: 0, failures: 0 };
      entry.calls += tool.call_count;
      entry.failures += tool.failure_count;
      toolAggregate.set(tool.tool, entry);
    }
  }

  const topAgents = agentAggregate
    .filter((a) => a.inferences > 0)
    .sort((a, b) => b.inferences - a.inferences)
    .slice(0, 5);

  const topTools = Array.from(toolAggregate.entries())
    .map(([tool, agg]) => ({
      tool,
      calls: agg.calls,
      failures: agg.failures,
      failureRate: agg.calls > 0 ? agg.failures / agg.calls : 0,
    }))
    .filter((row) => row.calls > 0)
    .sort((a, b) => b.calls - a.calls)
    .slice(0, 5);

  return {
    totalInferences,
    totalErrors,
    totalInputTokens,
    totalOutputTokens,
    totalSuspensions,
    totalHandoffs,
    totalDelegations,
    windowSeconds: mixedWindows ? 0 : (firstWindow ?? 0),
    mixedWindows,
    topAgents,
    topTools,
  };
}

/** Format a window in localised units. Returns `"—"` for unknown
 *  windows (`<= 0`), otherwise picks the largest unit that gives an
 *  integer ≥ 1 and uses the matching i18n unit key so zh-CN can render
 *  "时" / "分" / "天" instead of bare s/m/h/d. */
function formatWindowSecondsI18n(
  t: (key: string, params?: Record<string, unknown>) => string,
  seconds: number,
): string {
  if (seconds <= 0) return "—";
  if (seconds < 60) {
    return t("dashboard.agentActivity.units.s", { value: seconds });
  }
  if (seconds < 3600) {
    return t("dashboard.agentActivity.units.m", { value: Math.round(seconds / 60) });
  }
  if (seconds < 86_400) {
    return t("dashboard.agentActivity.units.h", { value: Math.round(seconds / 3600) });
  }
  return t("dashboard.agentActivity.units.d", { value: Math.round(seconds / 86_400) });
}
