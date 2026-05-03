// Pure helpers for the Eval Reports admin page.
//
// The shapes in this file mirror `crates/awaken-eval/src/outcome.rs` and
// `report.rs` so an NDJSON file produced by `awaken-eval replay` parses
// directly into typed records.  Drift here is a CI failure waiting to
// happen — keep field names and tag values in lock-step with the Rust
// crate.

/// A failure tagged union mirroring `awaken_eval::Failure`.
export type Failure =
  | { kind: "answer_missing_phrase"; phrase: string }
  | { kind: "answer_contains_excluded_phrase"; phrase: string }
  | { kind: "tool_sequence_mismatch"; expected: string[]; actual: string[] }
  | { kind: "forbidden_tool_used"; tool: string }
  | { kind: "token_budget_exceeded"; budget: number; actual: number }
  | { kind: "duration_exceeded"; budget_ms: number; actual_ms: number }
  | { kind: "judge_below_threshold"; threshold: number; actual: number };

export const FAILURE_KINDS = [
  "answer_missing_phrase",
  "answer_contains_excluded_phrase",
  "tool_sequence_mismatch",
  "forbidden_tool_used",
  "token_budget_exceeded",
  "duration_exceeded",
  "judge_below_threshold",
] as const;

export type FailureKind = (typeof FAILURE_KINDS)[number];

/// Per-(agent, tool) breakdown attached to each report (M9.2+).
/// Mirrors `awaken_ext_observability::AgentToolStats`.
export type AgentToolStats = {
  agent_id: string;
  tool: string;
  call_count: number;
  failure_count: number;
  total_duration_ms: number;
};

/// One line of an NDJSON eval report, mirroring `awaken_eval::ReplayReport`.
///
/// `tool_calls_by_agent` was introduced in 0.4.1; older NDJSON omits the
/// field, so the parser treats it as optional and defaults to `[]`.
export type ReplayReport = {
  fixture_id: string;
  passed: boolean;
  failures: Failure[];
  final_text: string;
  inference_count: number;
  tool_count: number;
  tool_failures: number;
  total_input_tokens: number;
  total_output_tokens: number;
  session_duration_ms: number;
  elapsed_ms: number;
  tool_calls_by_agent: AgentToolStats[];
};

/// One per malformed NDJSON line.  The page surfaces these without
/// rejecting the whole file so users can edit reports by hand.
export type ParseIssue = {
  line: number;
  message: string;
  raw: string;
};

export type ParseResult = {
  reports: ReplayReport[];
  issues: ParseIssue[];
};

/// Parse NDJSON text into typed reports. Tolerates blank lines and
/// records per-line failures rather than throwing.
export function parseReportsNdjson(text: string): ParseResult {
  const reports: ReplayReport[] = [];
  const issues: ParseIssue[] = [];
  const lines = text.split(/\r?\n/);
  for (let i = 0; i < lines.length; i++) {
    const raw = lines[i] ?? "";
    if (raw.trim() === "") continue;
    let parsed: unknown;
    try {
      parsed = JSON.parse(raw);
    } catch (err) {
      issues.push({
        line: i + 1,
        message: err instanceof Error ? err.message : String(err),
        raw,
      });
      continue;
    }
    if (!isReplayReport(parsed)) {
      issues.push({
        line: i + 1,
        message: "JSON does not match ReplayReport shape",
        raw,
      });
      continue;
    }
    reports.push(parsed);
  }
  return { reports, issues };
}

function isFailure(value: unknown): value is Failure {
  if (typeof value !== "object" || value === null) return false;
  const v = value as Record<string, unknown>;
  return typeof v.kind === "string" && FAILURE_KINDS.includes(v.kind as FailureKind);
}

function isAgentToolStats(value: unknown): value is AgentToolStats {
  if (typeof value !== "object" || value === null) return false;
  const v = value as Record<string, unknown>;
  return (
    typeof v.agent_id === "string" &&
    typeof v.tool === "string" &&
    typeof v.call_count === "number" &&
    typeof v.failure_count === "number" &&
    typeof v.total_duration_ms === "number"
  );
}

function isReplayReport(value: unknown): value is ReplayReport {
  if (typeof value !== "object" || value === null) return false;
  const v = value as Record<string, unknown>;
  const requiredOk =
    typeof v.fixture_id === "string" &&
    typeof v.passed === "boolean" &&
    Array.isArray(v.failures) &&
    v.failures.every(isFailure) &&
    typeof v.final_text === "string" &&
    typeof v.inference_count === "number" &&
    typeof v.tool_count === "number" &&
    typeof v.tool_failures === "number" &&
    typeof v.total_input_tokens === "number" &&
    typeof v.total_output_tokens === "number" &&
    typeof v.session_duration_ms === "number" &&
    typeof v.elapsed_ms === "number";
  if (!requiredOk) return false;
  // `tool_calls_by_agent` is optional (older reports omit it). When
  // present it must be a homogeneous array. Mutate-in-place via the
  // (value as Record) handle so the parsed object always exposes
  // the field as an array, even when the NDJSON omitted it.
  if (v.tool_calls_by_agent === undefined) {
    (v as { tool_calls_by_agent?: AgentToolStats[] }).tool_calls_by_agent = [];
    return true;
  }
  if (!Array.isArray(v.tool_calls_by_agent)) return false;
  return v.tool_calls_by_agent.every(isAgentToolStats);
}

// ---------------------------------------------------------------------------
// Summary
// ---------------------------------------------------------------------------

export type ReportsSummary = {
  total: number;
  passed: number;
  failed: number;
  totalInputTokens: number;
  totalOutputTokens: number;
  totalSessionMs: number;
  failureKindCounts: Record<FailureKind, number>;
};

const ZERO_FAILURE_COUNTS: Record<FailureKind, number> = {
  answer_missing_phrase: 0,
  answer_contains_excluded_phrase: 0,
  tool_sequence_mismatch: 0,
  forbidden_tool_used: 0,
  token_budget_exceeded: 0,
  duration_exceeded: 0,
  judge_below_threshold: 0,
};

export function summariseReports(reports: ReplayReport[]): ReportsSummary {
  const summary: ReportsSummary = {
    total: reports.length,
    passed: 0,
    failed: 0,
    totalInputTokens: 0,
    totalOutputTokens: 0,
    totalSessionMs: 0,
    failureKindCounts: { ...ZERO_FAILURE_COUNTS },
  };
  for (const r of reports) {
    if (r.passed) summary.passed += 1;
    else summary.failed += 1;
    summary.totalInputTokens += r.total_input_tokens;
    summary.totalOutputTokens += r.total_output_tokens;
    summary.totalSessionMs += r.session_duration_ms;
    for (const failure of r.failures) {
      summary.failureKindCounts[failure.kind] += 1;
    }
  }
  return summary;
}

// ---------------------------------------------------------------------------
// Baseline diff (mirrors awaken_eval::report::diff_against_baseline)
// ---------------------------------------------------------------------------

export type DiffEntry =
  | { kind: "unchanged"; fixture_id: string }
  | { kind: "regression"; fixture_id: string; new_failures: FailureKind[] }
  | { kind: "fixed"; fixture_id: string }
  | {
      kind: "still_failing";
      fixture_id: string;
      new_failures: FailureKind[];
    }
  | { kind: "missing_from_new"; fixture_id: string }
  | { kind: "newly_added"; fixture_id: string; passed: boolean };

export type DiffSummary = {
  entries: DiffEntry[];
  regressions: number;
  missing: number;
  added: number;
  fixed: number;
  unchanged: number;
  stillFailing: number;
  isClean: boolean;
};

/// Compare N runs against the first one (treated as baseline). Returns
/// per-fixture rows of (id, status[]) where status[i] tells how run i
/// fared relative to the baseline. Used by the Eval Reports A/B/C view.
export type MultiBaselineStatus =
  | "missing"
  | "passed"
  | "failed"
  | "regression"
  | "fixed";

export interface MultiBaselineRow {
  fixture_id: string;
  /** Length matches the runs argument; index 0 is always baseline status. */
  statuses: MultiBaselineStatus[];
}

export function diffReportsMulti(
  runs: { label: string; reports: ReplayReport[] }[],
): { rows: MultiBaselineRow[]; runs: { label: string }[] } {
  if (runs.length === 0) return { rows: [], runs: [] };
  const baseline = runs[0].reports;
  const baselineMap = new Map(baseline.map((r) => [r.fixture_id, r] as const));
  const allIds = new Set<string>();
  for (const run of runs) for (const r of run.reports) allIds.add(r.fixture_id);
  for (const r of baseline) allIds.add(r.fixture_id);

  const sortedIds = [...allIds].sort((a, b) => a.localeCompare(b));
  const rows: MultiBaselineRow[] = sortedIds.map((id) => {
    const baselineReport = baselineMap.get(id);
    const statuses = runs.map((run, idx) => {
      const r = run.reports.find((rr) => rr.fixture_id === id);
      if (!r) return "missing" as MultiBaselineStatus;
      if (idx === 0) return r.passed ? "passed" : "failed";
      if (!baselineReport) return r.passed ? "passed" : "failed";
      if (baselineReport.passed && !r.passed) return "regression";
      if (!baselineReport.passed && r.passed) return "fixed";
      return r.passed ? "passed" : "failed";
    });
    return { fixture_id: id, statuses };
  });
  return { rows, runs: runs.map((r) => ({ label: r.label })) };
}

export function diffReports(
  baseline: ReplayReport[],
  next: ReplayReport[],
): DiffSummary {
  const baselineMap = new Map(baseline.map((r) => [r.fixture_id, r] as const));
  const nextMap = new Map(next.map((r) => [r.fixture_id, r] as const));
  const ids = new Set<string>([...baselineMap.keys(), ...nextMap.keys()]);

  const entries: DiffEntry[] = [];
  for (const id of [...ids].sort((a, b) => a.localeCompare(b))) {
    const b = baselineMap.get(id);
    const n = nextMap.get(id);
    if (b && n) {
      if (b.passed && n.passed) {
        entries.push({ kind: "unchanged", fixture_id: id });
      } else if (b.passed && !n.passed) {
        entries.push({
          kind: "regression",
          fixture_id: id,
          new_failures: n.failures.map((f) => f.kind),
        });
      } else if (!b.passed && n.passed) {
        entries.push({ kind: "fixed", fixture_id: id });
      } else {
        entries.push({
          kind: "still_failing",
          fixture_id: id,
          new_failures: n.failures.map((f) => f.kind),
        });
      }
    } else if (b && !n) {
      entries.push({ kind: "missing_from_new", fixture_id: id });
    } else if (!b && n) {
      entries.push({ kind: "newly_added", fixture_id: id, passed: n.passed });
    }
  }

  const regressions = entries.filter((e) => e.kind === "regression").length;
  const missing = entries.filter((e) => e.kind === "missing_from_new").length;
  const added = entries.filter((e) => e.kind === "newly_added").length;
  const fixed = entries.filter((e) => e.kind === "fixed").length;
  const unchanged = entries.filter((e) => e.kind === "unchanged").length;
  const stillFailing = entries.filter((e) => e.kind === "still_failing").length;

  return {
    entries,
    regressions,
    missing,
    added,
    fixed,
    unchanged,
    stillFailing,
    isClean: regressions === 0 && missing === 0,
  };
}

// ---------------------------------------------------------------------------
// Display helpers
// ---------------------------------------------------------------------------

/// Stable, human-readable label for a failure variant.
export function describeFailure(failure: Failure): string {
  switch (failure.kind) {
    case "answer_missing_phrase":
      return `Missing phrase: ${JSON.stringify(failure.phrase)}`;
    case "answer_contains_excluded_phrase":
      return `Excluded phrase present: ${JSON.stringify(failure.phrase)}`;
    case "tool_sequence_mismatch":
      return `Tool sequence mismatch (expected ${JSON.stringify(failure.expected)}, got ${JSON.stringify(failure.actual)})`;
    case "forbidden_tool_used":
      return `Forbidden tool used: ${failure.tool}`;
    case "token_budget_exceeded":
      return `Token budget exceeded: ${failure.actual} / ${failure.budget}`;
    case "duration_exceeded":
      return `Duration exceeded: ${failure.actual_ms} ms / ${failure.budget_ms} ms`;
    case "judge_below_threshold":
      return `Judge score ${failure.actual.toFixed(2)} below threshold ${failure.threshold.toFixed(2)}`;
  }
}

/// Stable label for a diff entry.
export function describeDiffEntry(entry: DiffEntry): string {
  switch (entry.kind) {
    case "unchanged":
      return "Unchanged";
    case "regression":
      return `Regression: ${entry.new_failures.join(", ")}`;
    case "fixed":
      return "Fixed";
    case "still_failing":
      return `Still failing: ${entry.new_failures.join(", ")}`;
    case "missing_from_new":
      return "Missing from new run";
    case "newly_added":
      return entry.passed ? "Newly added (passing)" : "Newly added (failing)";
  }
}

export function isBlockingDiff(entry: DiffEntry): boolean {
  return entry.kind === "regression" || entry.kind === "missing_from_new";
}

// ---------------------------------------------------------------------------
// Scorer categorization (per industry consensus: Braintrust/LangSmith/Phoenix)
// ---------------------------------------------------------------------------

/// Scorer family. Industry consensus splits LLM evals into 4 buckets.
export type ScorerCategory = "heuristic" | "judge" | "code" | "human";

/// Categorize a failure by the kind of scorer that produced it.
export function categorizeFailure(failure: Failure): ScorerCategory {
  if (failure.kind === "judge_below_threshold") return "judge";
  // Everything else in the current Failure union is a heuristic check
  // (text presence, tool sequence, budgets). Code/human scorers will
  // appear when the eval crate exposes them.
  return "heuristic";
}

/// Pretty label for scorer category badges.
export function scorerCategoryLabel(c: ScorerCategory): string {
  switch (c) {
    case "judge": return "LLM judge";
    case "heuristic": return "heuristic";
    case "code": return "code";
    case "human": return "human";
  }
}

/// Trajectory match — derived from tool_sequence_mismatch failures.
/// Returns null when the report doesn't carry the relevant signal.
export function trajectoryMatch(report: ReplayReport): { matched: boolean; expected?: string[]; actual?: string[] } | null {
  const mismatch = report.failures.find((f) => f.kind === "tool_sequence_mismatch");
  if (mismatch && mismatch.kind === "tool_sequence_mismatch") {
    return { matched: false, expected: mismatch.expected, actual: mismatch.actual };
  }
  // No mismatch failure but the run had tool calls → trajectory is correct.
  if (report.tool_count > 0) return { matched: true };
  return null;
}

/// Heuristic cost model (USD). Defaults match Anthropic Sonnet-class
/// pricing as of 2025-01: $3 / Mtok in, $15 / Mtok out. Override via
/// caller when a more accurate per-model rate is known.
const DEFAULT_RATE_IN_PER_MTOK = 3;
const DEFAULT_RATE_OUT_PER_MTOK = 15;
export function estimateCost(report: ReplayReport, rateIn = DEFAULT_RATE_IN_PER_MTOK, rateOut = DEFAULT_RATE_OUT_PER_MTOK): number {
  return (
    (report.total_input_tokens / 1_000_000) * rateIn +
    (report.total_output_tokens / 1_000_000) * rateOut
  );
}

// ---------------------------------------------------------------------------
// Per-agent tool-call aggregation across the whole report
// ---------------------------------------------------------------------------

/// One row of the aggregate-across-fixtures view: which agent invoked
/// which tool how many times, summed across every fixture in the report.
export type AgentToolAggregate = {
  agent_id: string;
  tool: string;
  call_count: number;
  failure_count: number;
  total_duration_ms: number;
  /// Number of fixtures this (agent, tool) pair appeared in. Useful for
  /// distinguishing 100 calls in 1 fixture from 100 calls spread across
  /// 50 fixtures.
  fixture_count: number;
};

/// Sum the `tool_calls_by_agent` fields across `reports` and return a
/// stably-sorted (`agent_id`, then `tool`) view. Empty when no fixture
/// recorded any tool calls.
export function aggregateToolCallsByAgent(
  reports: ReplayReport[],
): AgentToolAggregate[] {
  const map = new Map<string, AgentToolAggregate>();
  for (const r of reports) {
    const seenInThisReport = new Set<string>();
    for (const stats of r.tool_calls_by_agent ?? []) {
      const key = `${stats.agent_id} ${stats.tool}`;
      const existing = map.get(key);
      if (existing) {
        existing.call_count += stats.call_count;
        existing.failure_count += stats.failure_count;
        existing.total_duration_ms += stats.total_duration_ms;
        if (!seenInThisReport.has(key)) {
          existing.fixture_count += 1;
          seenInThisReport.add(key);
        }
      } else {
        map.set(key, {
          agent_id: stats.agent_id,
          tool: stats.tool,
          call_count: stats.call_count,
          failure_count: stats.failure_count,
          total_duration_ms: stats.total_duration_ms,
          fixture_count: 1,
        });
        seenInThisReport.add(key);
      }
    }
  }
  return [...map.values()].sort((a, b) => {
    if (a.agent_id !== b.agent_id) {
      return a.agent_id.localeCompare(b.agent_id);
    }
    return a.tool.localeCompare(b.tool);
  });
}

/// Returns `true` when at least one report carries non-empty
/// `tool_calls_by_agent`. The Eval Reports page hides the per-agent panel
/// entirely when no report has populated the field, which keeps older
/// fixtures (no tool calls) visually clean.
export function hasAnyAgentToolStats(reports: ReplayReport[]): boolean {
  return reports.some((r) => (r.tool_calls_by_agent ?? []).length > 0);
}
