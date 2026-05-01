import { useMemo, useState, type ChangeEvent } from "react";
import {
  aggregateToolCallsByAgent,
  describeDiffEntry,
  describeFailure,
  diffReports,
  hasAnyAgentToolStats,
  isBlockingDiff,
  parseReportsNdjson,
  summariseReports,
  type AgentToolAggregate,
  type DiffEntry,
  type ParseIssue,
  type ReplayReport,
} from "@/lib/eval-reports";
import {
  DEFAULT_FIXTURE_FILTER,
  filterFixtures,
  type FixtureFilterState,
  type FixtureStatusFilter,
} from "@/lib/eval-reports-filter";

const STATUS_OPTIONS: Array<{ value: FixtureStatusFilter; label: string }> = [
  { value: "all", label: "All fixtures" },
  { value: "passed", label: "Passing" },
  { value: "failed", label: "Failing" },
  { value: "regressions", label: "Regressions" },
  { value: "fixed", label: "Newly fixed" },
];

type FileSlot = {
  name: string;
  reports: ReplayReport[];
  issues: ParseIssue[];
};

export function EvalReportsPage() {
  const [report, setReport] = useState<FileSlot | null>(null);
  const [baseline, setBaseline] = useState<FileSlot | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [fixtureFilter, setFixtureFilter] = useState<FixtureFilterState>(
    DEFAULT_FIXTURE_FILTER,
  );

  async function readFile(
    event: ChangeEvent<HTMLInputElement>,
    setter: (slot: FileSlot | null) => void,
  ) {
    const file = event.target.files?.[0];
    if (!file) return;
    setError(null);
    try {
      const text = await file.text();
      const { reports, issues } = parseReportsNdjson(text);
      setter({ name: file.name, reports, issues });
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  const summary = useMemo(
    () => (report ? summariseReports(report.reports) : null),
    [report],
  );

  const diff = useMemo(() => {
    if (!report || !baseline) return null;
    return diffReports(baseline.reports, report.reports);
  }, [baseline, report]);

  const diffByFixture = useMemo(() => {
    if (!diff) return new Map<string, DiffEntry>();
    return new Map(diff.entries.map((e) => [e.fixture_id, e]));
  }, [diff]);

  const visibleFixtures = useMemo(() => {
    if (!report) return [] as ReplayReport[];
    return filterFixtures(report.reports, fixtureFilter, diffByFixture);
  }, [report, fixtureFilter, diffByFixture]);

  const perAgentRows = useMemo(
    () => (report ? aggregateToolCallsByAgent(report.reports) : []),
    [report],
  );

  const showPerAgentPanel = useMemo(
    () => (report ? hasAnyAgentToolStats(report.reports) : false),
    [report],
  );

  return (
    <div className="mx-auto max-w-6xl p-6 md:p-8">
      <header className="mb-8">
        <p className="text-sm font-medium uppercase tracking-[0.2em] text-slate-500">
          Replay Forensics
        </p>
        <h2 className="mt-2 text-3xl font-semibold text-slate-950">
          Eval Reports
        </h2>
        <p className="mt-2 max-w-2xl text-sm text-slate-600">
          Drop in an NDJSON report produced by{" "}
          <code className="rounded bg-slate-100 px-1.5 py-0.5 font-mono text-xs">
            awaken-eval replay
          </code>
          . Optionally pair it with a baseline to surface regressions before
          they ship.
        </p>
      </header>

      <section className="grid gap-4 md:grid-cols-2">
        <FileDrop
          label="New report"
          slot={report}
          onChange={(e) => void readFile(e, setReport)}
          onClear={() => setReport(null)}
          required
        />
        <FileDrop
          label="Baseline (optional)"
          slot={baseline}
          onChange={(e) => void readFile(e, setBaseline)}
          onClear={() => setBaseline(null)}
        />
      </section>

      {error && (
        <div className="mt-6 rounded-2xl border border-rose-200 bg-rose-50 p-4 text-sm text-rose-700 shadow-sm">
          {error}
        </div>
      )}

      {report && (
        <>
          {summary && (
            <section className="mt-8 grid gap-4 md:grid-cols-2 xl:grid-cols-3 2xl:grid-cols-6">
              <StatCard label="Total" value={summary.total} />
              <StatCard
                label="Passed"
                value={summary.passed}
                tone="positive"
              />
              <StatCard
                label="Failed"
                value={summary.failed}
                tone={summary.failed > 0 ? "negative" : "neutral"}
              />
              <StatCard
                label="Input tokens"
                value={summary.totalInputTokens}
              />
              <StatCard
                label="Output tokens"
                value={summary.totalOutputTokens}
              />
              <StatCard
                label="Session ms"
                value={summary.totalSessionMs}
              />
            </section>
          )}

          {diff && (
            <section className="mt-6 rounded-2xl border border-slate-200 bg-white p-5 shadow-sm">
              <div className="flex items-center justify-between">
                <h3 className="text-lg font-semibold text-slate-900">
                  Baseline diff
                </h3>
                <span
                  className={[
                    "rounded-full px-3 py-1 text-xs font-semibold uppercase tracking-wide",
                    diff.isClean
                      ? "bg-emerald-100 text-emerald-800"
                      : "bg-rose-100 text-rose-800",
                  ].join(" ")}
                >
                  {diff.isClean ? "Clean" : "Blocking"}
                </span>
              </div>
              <dl className="mt-4 grid grid-cols-2 gap-3 text-sm sm:grid-cols-3 lg:grid-cols-6">
                <DiffStat label="Unchanged" value={diff.unchanged} />
                <DiffStat label="Regressions" value={diff.regressions} />
                <DiffStat label="Fixed" value={diff.fixed} />
                <DiffStat
                  label="Still failing"
                  value={diff.stillFailing}
                />
                <DiffStat label="Missing" value={diff.missing} />
                <DiffStat label="Newly added" value={diff.added} />
              </dl>
            </section>
          )}

          {showPerAgentPanel && (
            <PerAgentToolPanel rows={perAgentRows} />
          )}

          {report.issues.length > 0 && (
            <ParseIssuesPanel issues={report.issues} />
          )}
          {baseline && baseline.issues.length > 0 && (
            <ParseIssuesPanel issues={baseline.issues} forBaseline />
          )}

          <section className="mt-6 flex flex-wrap items-center gap-3 rounded-2xl border border-slate-200 bg-white p-4 shadow-sm">
            <label className="text-xs text-slate-500">
              <span className="mr-2">Show</span>
              <select
                value={fixtureFilter.status}
                onChange={(event) =>
                  setFixtureFilter((current) => ({
                    ...current,
                    status: event.target.value as FixtureStatusFilter,
                  }))
                }
                className="rounded-md border border-slate-300 bg-white px-2 py-1 text-xs text-slate-700 outline-none focus:border-slate-500"
              >
                {STATUS_OPTIONS.map((option) => (
                  <option
                    key={option.value}
                    value={option.value}
                    disabled={
                      (option.value === "regressions" || option.value === "fixed") &&
                      !diff
                    }
                  >
                    {option.label}
                  </option>
                ))}
              </select>
            </label>
            <label className="block w-full max-w-sm">
              <span className="sr-only">Search fixtures</span>
              <input
                type="search"
                value={fixtureFilter.search}
                onChange={(event) =>
                  setFixtureFilter((current) => ({
                    ...current,
                    search: event.target.value,
                  }))
                }
                placeholder="Search by fixture id…"
                className="w-full rounded-xl border border-slate-300 bg-white px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
              />
            </label>
            <span className="ml-auto text-xs text-slate-500">
              {visibleFixtures.length} of {report.reports.length} shown
            </span>
          </section>

          <section className="mt-3 rounded-2xl border border-slate-200 bg-white shadow-sm">
            <table className="min-w-full text-sm">
              <thead className="bg-slate-50 text-left text-xs uppercase tracking-wide text-slate-500">
                <tr>
                  <th className="px-4 py-3">Fixture</th>
                  <th className="px-4 py-3">Status</th>
                  <th className="px-4 py-3">Failures</th>
                  <th className="px-4 py-3">Tokens</th>
                  <th className="px-4 py-3">Duration</th>
                  {diff && <th className="px-4 py-3">vs baseline</th>}
                </tr>
              </thead>
              <tbody className="divide-y divide-slate-200">
                {report.reports.length === 0 ? (
                  <tr>
                    <td
                      colSpan={diff ? 6 : 5}
                      className="px-4 py-6 text-center text-sm text-slate-500"
                    >
                      The report contained no fixtures.
                    </td>
                  </tr>
                ) : visibleFixtures.length === 0 ? (
                  <tr>
                    <td
                      colSpan={diff ? 6 : 5}
                      className="px-4 py-6 text-center text-sm text-slate-500"
                    >
                      No fixtures match the current filter.
                    </td>
                  </tr>
                ) : (
                  visibleFixtures.map((r) => (
                    <FixtureRow
                      key={r.fixture_id}
                      report={r}
                      diff={diffByFixture.get(r.fixture_id) ?? null}
                      hasDiff={Boolean(diff)}
                    />
                  ))
                )}
              </tbody>
            </table>
          </section>
        </>
      )}
    </div>
  );
}

function FileDrop({
  label,
  slot,
  onChange,
  onClear,
  required,
}: {
  label: string;
  slot: FileSlot | null;
  onChange: (event: ChangeEvent<HTMLInputElement>) => void;
  onClear: () => void;
  required?: boolean;
}) {
  return (
    <label className="flex flex-col rounded-2xl border border-dashed border-slate-300 bg-white p-5 shadow-sm transition hover:border-slate-400">
      <div className="flex items-center justify-between">
        <span className="text-sm font-semibold text-slate-700">
          {label}
          {required ? <span className="ml-1 text-rose-600">*</span> : null}
        </span>
        {slot && (
          <button
            type="button"
            onClick={onClear}
            className="rounded-md border border-slate-200 px-2 py-1 text-xs text-slate-500 hover:bg-slate-50"
          >
            Clear
          </button>
        )}
      </div>
      <input
        type="file"
        accept=".ndjson,.json,.txt,application/json,text/plain"
        onChange={onChange}
        className="mt-3 block w-full text-sm text-slate-500 file:mr-3 file:rounded-md file:border-0 file:bg-slate-900 file:px-3 file:py-2 file:text-xs file:font-semibold file:uppercase file:tracking-wide file:text-white hover:file:bg-slate-800"
      />
      {slot ? (
        <div className="mt-3 text-xs text-slate-500">
          <span className="font-mono">{slot.name}</span> · {slot.reports.length}{" "}
          fixture(s){" "}
          {slot.issues.length > 0 && (
            <span className="text-amber-600">
              · {slot.issues.length} parse issue(s)
            </span>
          )}
        </div>
      ) : (
        <p className="mt-3 text-xs text-slate-400">
          Pick an NDJSON file or drop one onto this card.
        </p>
      )}
    </label>
  );
}

function StatCard({
  label,
  value,
  tone = "neutral",
}: {
  label: string;
  value: number;
  tone?: "neutral" | "positive" | "negative";
}) {
  const toneClass =
    tone === "positive"
      ? "text-emerald-700"
      : tone === "negative"
        ? "text-rose-700"
        : "text-slate-950";
  return (
    <div className="rounded-2xl border border-slate-200 bg-white p-5 shadow-sm">
      <div className={`text-3xl font-semibold ${toneClass}`}>{value}</div>
      <div className="mt-2 text-sm text-slate-500">{label}</div>
    </div>
  );
}

function DiffStat({ label, value }: { label: string; value: number }) {
  return (
    <div className="rounded-xl border border-slate-200 bg-slate-50 px-3 py-2">
      <div className="font-mono text-base font-semibold text-slate-900">
        {value}
      </div>
      <div className="text-xs uppercase tracking-wide text-slate-500">
        {label}
      </div>
    </div>
  );
}

function FixtureRow({
  report,
  diff,
  hasDiff,
}: {
  report: ReplayReport;
  diff: DiffEntry | null;
  hasDiff: boolean;
}) {
  return (
    <tr className="hover:bg-slate-50">
      <td className="px-4 py-3 font-mono text-sm text-slate-900">
        {report.fixture_id}
      </td>
      <td className="px-4 py-3">
        <span
          className={[
            "inline-flex items-center rounded-full px-2.5 py-0.5 text-xs font-medium",
            report.passed
              ? "bg-emerald-100 text-emerald-800"
              : "bg-rose-100 text-rose-800",
          ].join(" ")}
        >
          {report.passed ? "passed" : "failed"}
        </span>
      </td>
      <td className="px-4 py-3 text-sm text-slate-700">
        {report.failures.length === 0 ? (
          <span className="text-slate-400">—</span>
        ) : (
          <ul className="space-y-1">
            {report.failures.map((failure, idx) => (
              <li key={idx}>{describeFailure(failure)}</li>
            ))}
          </ul>
        )}
      </td>
      <td className="px-4 py-3 font-mono text-xs text-slate-600">
        {report.total_input_tokens}/{report.total_output_tokens}
      </td>
      <td className="px-4 py-3 font-mono text-xs text-slate-600">
        {report.session_duration_ms} ms
      </td>
      {hasDiff && (
        <td className="px-4 py-3 text-sm text-slate-700">
          {diff ? (
            <span
              className={[
                "inline-flex items-center rounded-full px-2.5 py-0.5 text-xs font-medium",
                isBlockingDiff(diff)
                  ? "bg-rose-100 text-rose-800"
                  : diff.kind === "fixed"
                    ? "bg-emerald-100 text-emerald-800"
                    : "bg-slate-100 text-slate-700",
              ].join(" ")}
            >
              {describeDiffEntry(diff)}
            </span>
          ) : (
            <span className="text-slate-400">—</span>
          )}
        </td>
      )}
    </tr>
  );
}

function PerAgentToolPanel({ rows }: { rows: AgentToolAggregate[] }) {
  return (
    <section className="mt-6 rounded-2xl border border-slate-200 bg-white p-5 shadow-sm">
      <div className="flex items-center justify-between">
        <h3 className="text-lg font-semibold text-slate-900">
          Tool calls by agent
        </h3>
        <span className="text-sm text-slate-500">
          {rows.length} (agent, tool) pair(s)
        </span>
      </div>
      <table className="mt-4 min-w-full text-sm">
        <thead className="text-left text-xs uppercase tracking-wide text-slate-500">
          <tr>
            <th className="px-2 py-2">Agent</th>
            <th className="px-2 py-2">Tool</th>
            <th className="px-2 py-2 text-right">Calls</th>
            <th className="px-2 py-2 text-right">Failures</th>
            <th className="px-2 py-2 text-right">Total ms</th>
            <th className="px-2 py-2 text-right">Fixtures</th>
          </tr>
        </thead>
        <tbody className="divide-y divide-slate-200">
          {rows.map((row) => (
            <tr
              key={`${row.agent_id}::${row.tool}`}
              className="hover:bg-slate-50"
            >
              <td className="px-2 py-2 font-mono text-xs text-slate-900">
                {row.agent_id || (
                  <span className="italic text-slate-400">(unset)</span>
                )}
              </td>
              <td className="px-2 py-2 font-mono text-xs text-slate-900">
                {row.tool}
              </td>
              <td className="px-2 py-2 text-right font-mono text-xs text-slate-700">
                {row.call_count}
              </td>
              <td className="px-2 py-2 text-right font-mono text-xs text-slate-700">
                {row.failure_count > 0 ? (
                  <span className="text-rose-700">{row.failure_count}</span>
                ) : (
                  row.failure_count
                )}
              </td>
              <td className="px-2 py-2 text-right font-mono text-xs text-slate-700">
                {row.total_duration_ms}
              </td>
              <td className="px-2 py-2 text-right font-mono text-xs text-slate-700">
                {row.fixture_count}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </section>
  );
}

function ParseIssuesPanel({
  issues,
  forBaseline,
}: {
  issues: ParseIssue[];
  forBaseline?: boolean;
}) {
  return (
    <section className="mt-6 rounded-2xl border border-amber-200 bg-amber-50 p-5 shadow-sm">
      <h3 className="text-sm font-semibold text-amber-900">
        {forBaseline
          ? "Baseline parse issues"
          : "Report parse issues"}{" "}
        ({issues.length})
      </h3>
      <ul className="mt-3 space-y-2 text-xs text-amber-900">
        {issues.slice(0, 25).map((issue) => (
          <li key={issue.line}>
            <span className="font-mono">line {issue.line}:</span>{" "}
            {issue.message}
          </li>
        ))}
        {issues.length > 25 && (
          <li className="italic">
            …and {issues.length - 25} more (truncated for display)
          </li>
        )}
      </ul>
    </section>
  );
}
