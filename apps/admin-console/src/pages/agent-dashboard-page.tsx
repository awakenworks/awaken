import { useEffect, useState } from "react";
import { Link, useParams, useSearchParams } from "react-router";
import {
  errorRate,
  fetchAgentRuntimeStats,
  formatHistogramLabel,
  formatWindow,
  maxHistogramCount,
  toolFailureRate,
  type AgentRuntimeSnapshot,
  type AgentRuntimeStatsResult,
  type HistogramBucket,
} from "@/lib/agent-stats";
import { adminRoutes } from "@/lib/routes";

const WINDOW_OPTIONS = [
  { label: "Default", value: "" },
  { label: "1h", value: "1h" },
  { label: "6h", value: "6h" },
  { label: "24h", value: "24h" },
  { label: "7d", value: "7d" },
] as const;

export function AgentDashboardPage() {
  const { id } = useParams<{ id: string }>();
  const [searchParams, setSearchParams] = useSearchParams();
  const windowParam = searchParams.get("window") ?? "";
  const [result, setResult] = useState<AgentRuntimeStatsResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [reloadKey, setReloadKey] = useState(0);

  useEffect(() => {
    if (!id) return;
    let cancelled = false;
    setResult(null);
    setError(null);
    const opts = windowParam ? { window: windowParam } : undefined;
    void fetchAgentRuntimeStats(id, opts)
      .then((r) => {
        if (!cancelled) setResult(r);
      })
      .catch((err) => {
        if (!cancelled) {
          setError(err instanceof Error ? err.message : String(err));
        }
      });
    return () => {
      cancelled = true;
    };
  }, [id, windowParam, reloadKey]);

  if (!id) {
    return <Shell title="Agent Dashboard">Missing agent id.</Shell>;
  }

  if (error) {
    return (
      <Shell title={`Dashboard · ${id}`}>
        <ErrorPanel message={error} />
      </Shell>
    );
  }

  if (!result) {
    return (
      <Shell title={`Dashboard · ${id}`}>
        <div className="rounded-2xl border border-line bg-surface p-6 text-sm text-fg-soft shadow-sm">
          Loading runtime stats…
        </div>
      </Shell>
    );
  }

  if (result.kind === "registry_disabled") {
    return (
      <Shell title={`Dashboard · ${id}`}>
        <RegistryDisabledPanel onReload={() => setReloadKey((k) => k + 1)} />
      </Shell>
    );
  }

  if (result.kind === "not_found") {
    return (
      <Shell title={`Dashboard · ${id}`}>
        <NotYetSeenPanel
          agentId={result.agent_id}
          onReload={() => setReloadKey((k) => k + 1)}
        />
      </Shell>
    );
  }

  if (result.kind === "error") {
    return (
      <Shell title={`Dashboard · ${id}`}>
        <ErrorPanel message={`HTTP ${result.status}: ${result.message}`} />
      </Shell>
    );
  }

  const snapshot = result.snapshot;

  return (
    <Shell title={`Dashboard · ${snapshot.agent_id}`}>
      <div className="-mt-4 mb-6 flex flex-wrap items-center gap-3">
        <p className="text-sm text-fg-soft">
          Rolling-window snapshot for the last{" "}
          <span className="font-mono">{formatWindow(snapshot.window_seconds)}</span>{" "}
          ({snapshot.bucket_count} buckets ×{" "}
          <span className="font-mono">
            {formatWindow(snapshot.bucket_window_seconds)}
          </span>
          ).
        </p>
        <div className="flex items-center gap-2">
          <label
            htmlFor="window-picker"
            className="text-xs text-fg-soft"
          >
            Window:
          </label>
          <select
            id="window-picker"
            value={windowParam}
            onChange={(e) => {
              const val = e.target.value;
              setSearchParams(
                (prev) => {
                  const next = new URLSearchParams(prev);
                  if (val) {
                    next.set("window", val);
                  } else {
                    next.delete("window");
                  }
                  return next;
                },
                { replace: true },
              );
            }}
            className="rounded-md border border-line bg-surface px-2 py-1 text-xs text-fg shadow-sm"
          >
            {WINDOW_OPTIONS.map((opt) => (
              <option key={opt.value} value={opt.value}>
                {opt.label}
              </option>
            ))}
          </select>
        </div>
        <button
          type="button"
          onClick={() => setReloadKey((k) => k + 1)}
          className="rounded-md border border-line bg-surface px-2 py-1 text-xs text-fg-soft hover:bg-soft"
        >
          Refresh
        </button>
      </div>

      <Section title="Runtime health">
        <StatGrid>
          <StatCard label="Inferences" value={snapshot.inference_count} />
          <StatCard
            label="Errors"
            value={snapshot.error_count}
            tone={snapshot.error_count > 0 ? "negative" : "neutral"}
          />
          <StatCard
            label="Error rate"
            value={`${(errorRate(snapshot) * 100).toFixed(1)}%`}
            tone={errorRate(snapshot) > 0 ? "negative" : "neutral"}
          />
          <StatCard label="Input tokens" value={snapshot.input_tokens} />
          <StatCard label="Output tokens" value={snapshot.output_tokens} />
          <StatCard
            label="Avg latency (ms)"
            value={Math.round(snapshot.avg_inference_duration_ms)}
          />
          <StatCard
            label="Min latency (ms)"
            value={snapshot.min_inference_duration_ms}
          />
          <StatCard
            label="p50 latency (ms)"
            value={snapshot.p50_inference_duration_ms}
          />
          <StatCard
            label="p95 latency (ms)"
            value={snapshot.p95_inference_duration_ms}
          />
          <StatCard
            label="p99 latency (ms)"
            value={snapshot.p99_inference_duration_ms}
          />
          <StatCard
            label="Max latency (ms)"
            value={snapshot.max_inference_duration_ms}
          />
        </StatGrid>
      </Section>

      {snapshot.inference_duration_histogram.length > 0 && (
        <Section title="Inference latency distribution">
          <HistogramPanel buckets={snapshot.inference_duration_histogram} />
        </Section>
      )}

      <Section title="Lifecycle events">
        <StatGrid>
          <StatCard label="Suspensions" value={snapshot.suspensions} />
          <StatCard label="Handoffs" value={snapshot.handoffs} />
          <StatCard label="Delegations" value={snapshot.delegations} />
          <StatCard
            label="Tool failure rate"
            value={`${(toolFailureRate(snapshot) * 100).toFixed(1)}%`}
            tone={toolFailureRate(snapshot) > 0 ? "negative" : "neutral"}
          />
        </StatGrid>
      </Section>

      <Section title="Tools">
        {snapshot.tool_calls_by_tool.length === 0 ? (
          <div className="rounded-2xl border border-line bg-surface p-5 text-sm text-fg-soft shadow-sm">
            No tool invocations recorded in the current window.
          </div>
        ) : (
          <div className="overflow-auto rounded-2xl border border-line bg-surface shadow-sm">
            <table className="min-w-full text-sm">
              <thead className="bg-soft text-left text-xs uppercase tracking-wide text-fg-soft">
                <tr>
                  <th className="px-3 py-3">Tool</th>
                  <th className="px-3 py-3 text-right">Calls</th>
                  <th className="px-3 py-3 text-right">Failures</th>
                  <th className="px-3 py-3 text-right">Avg ms</th>
                  <th className="px-3 py-3 text-right">Min</th>
                  <th className="px-3 py-3 text-right">p50</th>
                  <th className="px-3 py-3 text-right">p95</th>
                  <th className="px-3 py-3 text-right">p99</th>
                  <th className="px-3 py-3 text-right">Max</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-line">
                {snapshot.tool_calls_by_tool.map((row) => (
                  <tr key={row.tool} className="hover:bg-soft">
                    <td className="px-3 py-3 font-mono text-xs text-fg-strong">
                      {row.tool}
                    </td>
                    <td className="px-3 py-3 text-right font-mono text-xs">
                      {row.call_count}
                    </td>
                    <td className="px-3 py-3 text-right font-mono text-xs">
                      {row.failure_count > 0 ? (
                        <span className="text-tone-error">
                          {row.failure_count}
                        </span>
                      ) : (
                        row.failure_count
                      )}
                    </td>
                    <td className="px-3 py-3 text-right font-mono text-xs">
                      {row.avg_duration_ms.toFixed(1)}
                    </td>
                    <td className="px-3 py-3 text-right font-mono text-xs">
                      {row.min_duration_ms}
                    </td>
                    <td className="px-3 py-3 text-right font-mono text-xs">
                      {row.p50_duration_ms}
                    </td>
                    <td className="px-3 py-3 text-right font-mono text-xs">
                      {row.p95_duration_ms}
                    </td>
                    <td className="px-3 py-3 text-right font-mono text-xs">
                      {row.p99_duration_ms}
                    </td>
                    <td className="px-3 py-3 text-right font-mono text-xs">
                      {row.max_duration_ms}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Section>

      {snapshot.tool_calls_by_tool.some(
        (t) => t.duration_histogram.length > 0,
      ) && (
        <Section title="Tool latency distributions">
          <div className="grid gap-4 lg:grid-cols-2">
            {snapshot.tool_calls_by_tool
              .filter((t) => t.duration_histogram.length > 0)
              .map((t) => (
                <div
                  key={t.tool}
                  className="rounded-2xl border border-line bg-surface p-5 shadow-sm"
                >
                  <h4 className="font-mono text-sm font-semibold text-fg-strong">
                    {t.tool}
                  </h4>
                  <p className="text-xs text-fg-soft">
                    {t.call_count} call(s) · p95 {t.p95_duration_ms} ms
                  </p>
                  <div className="mt-3">
                    <HistogramPanel buckets={t.duration_histogram} compact />
                  </div>
                </div>
              ))}
          </div>
        </Section>
      )}

      <Section title="Quick actions">
        <ul className="flex flex-wrap gap-3">
          <li>
            <Link
              to={adminRoutes.agent(snapshot.agent_id)}
              className="inline-flex items-center rounded-lg border border-line bg-surface px-3 py-2 text-sm text-fg shadow-sm hover:bg-soft"
            >
              Edit configuration
            </Link>
          </li>
          <li>
            <Link
              to={adminRoutes.evalReports}
              className="inline-flex items-center rounded-lg border border-line bg-surface px-3 py-2 text-sm text-fg shadow-sm hover:bg-soft"
            >
              Eval reports
            </Link>
          </li>
        </ul>
      </Section>
    </Shell>
  );
}

// ── Layout primitives ──────────────────────────────────────────────

function Shell({
  title,
  children,
}: {
  title: string;
  children: React.ReactNode;
}) {
  return (
    <div className="mx-auto max-w-6xl p-6 md:p-8">
      <header className="mb-8">
        <p className="text-sm font-medium uppercase tracking-[0.2em] text-fg-soft">
          Agent Dashboard
        </p>
        <h2 className="mt-2 text-3xl font-semibold text-fg-strong">{title}</h2>
      </header>
      {children}
    </div>
  );
}

function Section({
  title,
  children,
}: {
  title: string;
  children: React.ReactNode;
}) {
  return (
    <section className="mt-8">
      <h3 className="mb-3 text-lg font-semibold text-fg-strong">{title}</h3>
      {children}
    </section>
  );
}

function StatGrid({ children }: { children: React.ReactNode }) {
  return (
    <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-4">{children}</div>
  );
}

function StatCard({
  label,
  value,
  tone = "neutral",
}: {
  label: string;
  value: number | string;
  tone?: "neutral" | "positive" | "negative";
}) {
  const toneClass =
    tone === "positive"
      ? "text-tone-success"
      : tone === "negative"
        ? "text-tone-error"
        : "text-fg-strong";
  return (
    <div className="rounded-2xl border border-line bg-surface p-5 shadow-sm">
      <div className={`text-3xl font-semibold ${toneClass}`}>{value}</div>
      <div className="mt-2 text-sm text-fg-soft">{label}</div>
    </div>
  );
}

function RegistryDisabledPanel({ onReload }: { onReload: () => void }) {
  return (
    <div className="rounded-2xl border border-tone-warn/35 bg-tone-warn/10 p-6 text-sm text-tone-warn shadow-sm">
      <h3 className="text-base font-semibold">Runtime stats not configured</h3>
      <p className="mt-2">
        The server is not running with a{" "}
        <code className="rounded bg-tone-warn/15 px-1.5 py-0.5 font-mono text-xs">
          RuntimeStatsRegistry
        </code>{" "}
        attached to its <code className="font-mono">AppState</code>. Embedders
        opt in by calling{" "}
        <code className="rounded bg-tone-warn/15 px-1.5 py-0.5 font-mono text-xs">
          state.with_runtime_stats(registry)
        </code>{" "}
        and wiring the same registry into the agent runtime's observability
        plugin.
      </p>
      <button
        type="button"
        onClick={onReload}
        className="mt-3 rounded-md border border-tone-warn/35 bg-surface px-3 py-1.5 text-xs font-medium hover:bg-tone-warn/15"
      >
        Retry
      </button>
    </div>
  );
}

function NotYetSeenPanel({
  agentId,
  onReload,
}: {
  agentId: string;
  onReload: () => void;
}) {
  return (
    <div className="rounded-2xl border border-line bg-surface p-6 text-sm text-fg shadow-sm">
      <h3 className="text-base font-semibold text-fg-strong">
        No runtime activity yet
      </h3>
      <p className="mt-2">
        The agent <span className="font-mono">{agentId}</span> has not produced
        any inference, tool, or lifecycle events in the current rolling
        window. As soon as it runs, this dashboard will populate.
      </p>
      <button
        type="button"
        onClick={onReload}
        className="mt-3 rounded-md border border-line bg-surface px-3 py-1.5 text-xs font-medium hover:bg-soft"
      >
        Refresh
      </button>
    </div>
  );
}

function HistogramPanel({
  buckets,
  compact,
}: {
  buckets: HistogramBucket[];
  compact?: boolean;
}) {
  const max = maxHistogramCount(buckets);
  const containerClass = compact
    ? "rounded-xl bg-soft p-3"
    : "rounded-2xl border border-line bg-surface p-5 shadow-sm";
  return (
    <div className={containerClass}>
      <ul className="space-y-1.5">
        {buckets.map((b, idx) => {
          const widthPct = max === 0 ? 0 : Math.round((b.count / max) * 100);
          const label = formatHistogramLabel(b);
          return (
            <li
              key={`${idx}-${b.upper_bound_ms ?? "inf"}`}
              className="flex items-center gap-3 text-xs"
            >
              <span className="w-24 shrink-0 text-right font-mono text-fg-soft">
                {label}
              </span>
              <div className="relative flex-1 overflow-hidden rounded bg-muted">
                <div
                  className="h-3 rounded bg-fg transition-[width]"
                  style={{ width: `${widthPct}%` }}
                  aria-hidden
                />
              </div>
              <span className="w-12 shrink-0 text-right font-mono text-fg">
                {b.count}
              </span>
            </li>
          );
        })}
      </ul>
    </div>
  );
}

function ErrorPanel({ message }: { message: string }) {
  return (
    <div className="rounded-2xl border border-tone-error/30 bg-tone-error/10 p-6 text-sm text-tone-error shadow-sm">
      {message}
    </div>
  );
}

// ── Type re-export for tests ───────────────────────────────────────

export type { AgentRuntimeSnapshot };
