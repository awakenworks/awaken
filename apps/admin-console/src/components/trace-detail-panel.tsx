import { useEffect } from "react";
import { useTranslation } from "react-i18next";
import {
  categorizeFailure,
  describeFailure,
  estimateCost,
  scorerCategoryLabel,
  trajectoryMatch,
  type Failure,
  type ReplayReport,
  type ScorerCategory,
} from "@/lib/eval-reports";
import { Pill } from "@/components/ui/pill";

/**
 * Trace Detail — the design's per-case drill-in. Surfaces what we have
 * today (`ReplayReport` fields) in the structure the design specifies:
 * header strip → scorer breakdown → input/output → span timeline.
 *
 * Per-trace data (span tree, structured i/o) requires backend trace
 * collection that doesn't exist yet; the empty zones explicitly say so
 * rather than fabricate.
 */
export function TraceDetailPanel({
  report,
  onClose,
}: {
  report: ReplayReport;
  onClose: () => void;
}) {
  const { t } = useTranslation();

  useEffect(() => {
    function onEsc(e: KeyboardEvent) {
      if (e.key === "Escape") onClose();
    }
    document.addEventListener("keydown", onEsc);
    return () => document.removeEventListener("keydown", onEsc);
  }, [onClose]);

  const traj = trajectoryMatch(report);
  const cost = estimateCost(report);
  const failuresByCategory = groupFailures(report.failures);

  return (
    <>
      <button
        type="button"
        aria-label={t("common.close")}
        onClick={onClose}
        className="fixed inset-0 z-40 bg-overlay backdrop-blur-sm"
      />
      <aside
        role="dialog"
        aria-label={t("trace.title")}
        className="fixed inset-y-0 right-0 z-50 flex w-full max-w-3xl flex-col overflow-hidden border-l border-line bg-surface shadow-overlay"
      >
        <header className="flex items-baseline justify-between gap-3 border-b border-line px-6 py-4">
          <div>
            <p className="text-[10px] font-medium uppercase tracking-eyebrow text-fg-soft">
              {t("trace.eyebrow")}
            </p>
            <h2 className="mt-1 text-lg font-semibold text-fg-strong">
              <span className="font-mono">{report.fixture_id}</span>
            </h2>
          </div>
          <button
            type="button"
            onClick={onClose}
            className="text-xs font-medium text-fg-soft transition hover:text-fg-strong"
          >
            {t("common.close")}
          </button>
        </header>

        <div className="flex-1 overflow-y-auto">
          {/* Header strip — 4 stat cells */}
          <div className="grid grid-cols-2 gap-px border-b border-line bg-line lg:grid-cols-4">
            <Cell
              label={t("trace.meta.totalLatency")}
              value={`${(report.session_duration_ms / 1000).toFixed(2)}s`}
              sub={`elapsed ${(report.elapsed_ms / 1000).toFixed(2)}s`}
            />
            <Cell
              label={t("trace.meta.tokens")}
              value={`${report.total_input_tokens.toLocaleString()} → ${report.total_output_tokens.toLocaleString()}`}
              sub={`${(report.total_input_tokens + report.total_output_tokens).toLocaleString()} total`}
            />
            <Cell
              label={t("trace.meta.trajectory")}
              value={
                traj === null
                  ? "—"
                  : traj.matched
                    ? `✓ ${report.tool_count} steps`
                    : `${traj.actual?.length ?? 0}/${traj.expected?.length ?? 0} steps`
              }
              sub={traj === null ? "no tool calls" : traj.matched ? "matches expected" : "mismatch vs expected"}
              tone={traj && !traj.matched ? "warn" : undefined}
            />
            <Cell
              label={t("trace.meta.cost")}
              value={`$${cost.toFixed(4)}`}
              sub={`${report.inference_count} inferences · ${report.tool_count} tools`}
            />
          </div>

          {/* Status pill */}
          <div className="flex items-center gap-2 px-6 py-3">
            <Pill tone={report.passed ? "success" : "error"}>
              {report.passed ? t("trace.scorers.pass") : t("trace.scorers.fail")}
            </Pill>
            <span className="font-mono text-xs text-fg-faint">
              {report.failures.length} failure{report.failures.length === 1 ? "" : "s"}
            </span>
          </div>

          {/* Scorers, grouped by category */}
          <section className="border-t border-line px-6 py-4">
            <h3 className="text-sm font-semibold text-fg-strong">{t("trace.scorers.title")}</h3>
            {report.failures.length === 0 ? (
              <p className="mt-2 text-sm text-fg-soft">No scorer failures recorded.</p>
            ) : (
              <ul className="mt-3 space-y-3">
                {(Object.entries(failuresByCategory) as [ScorerCategory, Failure[]][])
                  .filter(([, list]) => list.length > 0)
                  .map(([cat, list]) => (
                    <li key={cat} className="rounded-sm border border-line bg-soft px-3 py-2">
                      <div className="flex items-center gap-2">
                        <Pill tone={cat === "judge" ? "info" : "warn"}>
                          {scorerCategoryLabel(cat)}
                        </Pill>
                        <span className="text-xs text-fg-faint">
                          {list.length} failure{list.length === 1 ? "" : "s"}
                        </span>
                      </div>
                      <ul className="mt-2 space-y-1 text-sm text-fg">
                        {list.map((f, idx) => (
                          <li key={idx} className="font-mono text-xs text-fg-strong">
                            {describeFailure(f)}
                          </li>
                        ))}
                      </ul>
                    </li>
                  ))}
              </ul>
            )}
          </section>

          {/* Output text */}
          <section className="border-t border-line px-6 py-4">
            <h3 className="text-sm font-semibold text-fg-strong">{t("trace.io.output")}</h3>
            {report.final_text ? (
              <pre className="mt-2 max-h-64 overflow-auto rounded-sm bg-code-bg px-3 py-2 font-mono text-[11px] leading-5 text-code-fg">
                {report.final_text}
              </pre>
            ) : (
              <p className="mt-2 text-sm text-fg-soft">No final text emitted.</p>
            )}
          </section>

          {/* Span timeline placeholder */}
          <section className="border-t border-line px-6 py-4">
            <h3 className="text-sm font-semibold text-fg-strong">{t("trace.spans.title")}</h3>
            <p className="mt-1 text-xs text-fg-faint">
              {t("trace.spans.meta", { total: (report.session_duration_ms / 1000).toFixed(2), count: report.inference_count + report.tool_count })}
            </p>
            <div className="mt-3 rounded-sm border border-dashed border-line bg-canvas p-4 text-xs text-fg-soft">
              Per-span trace tree requires backend trace collection (not yet wired).
              The aggregate counts above come from <code className="font-mono">ReplayReport</code>.
            </div>
          </section>

          {/* Per-agent tool usage — if present */}
          {(report.tool_calls_by_agent ?? []).length > 0 && (
            <section className="border-t border-line px-6 py-4">
              <h3 className="text-sm font-semibold text-fg-strong">Per-agent tool calls</h3>
              <table className="mt-3 w-full text-xs">
                <thead className="text-fg-soft">
                  <tr>
                    <th className="py-1 text-left font-medium">Agent</th>
                    <th className="py-1 text-left font-medium">Tool</th>
                    <th className="py-1 text-right font-medium">Calls</th>
                    <th className="py-1 text-right font-medium">Failures</th>
                    <th className="py-1 text-right font-medium">Total ms</th>
                  </tr>
                </thead>
                <tbody className="divide-y divide-line">
                  {(report.tool_calls_by_agent ?? []).map((s, i) => (
                    <tr key={i}>
                      <td className="py-1 font-mono text-fg-strong">{s.agent_id}</td>
                      <td className="py-1 font-mono text-fg">{s.tool}</td>
                      <td className="py-1 text-right font-mono">{s.call_count}</td>
                      <td className={`py-1 text-right font-mono ${s.failure_count > 0 ? "text-tone-error" : "text-fg-faint"}`}>
                        {s.failure_count}
                      </td>
                      <td className="py-1 text-right font-mono text-fg-soft">{s.total_duration_ms}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </section>
          )}
        </div>
      </aside>
    </>
  );
}

function Cell({
  label,
  value,
  sub,
  tone,
}: {
  label: string;
  value: string;
  sub?: string;
  tone?: "warn";
}) {
  return (
    <div className="bg-surface px-4 py-3">
      <div className="text-[10px] font-medium uppercase tracking-eyebrow text-fg-soft">{label}</div>
      <div className={`mt-1 font-mono text-base font-semibold ${tone === "warn" ? "text-tone-warn" : "text-fg-strong"}`}>
        {value}
      </div>
      {sub && <div className="mt-0.5 text-[10px] text-fg-faint">{sub}</div>}
    </div>
  );
}

function groupFailures(failures: Failure[]): Record<ScorerCategory, Failure[]> {
  const out: Record<ScorerCategory, Failure[]> = { heuristic: [], judge: [], code: [], human: [] };
  for (const f of failures) out[categorizeFailure(f)].push(f);
  return out;
}
