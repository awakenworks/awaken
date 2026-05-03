import { describe, expect, it } from "vitest";
import {
  aggregateToolCallsByAgent,
  describeDiffEntry,
  describeFailure,
  diffReports,
  diffReportsMulti,
  hasAnyAgentToolStats,
  isBlockingDiff,
  parseReportsNdjson,
  summariseReports,
  type AgentToolStats,
  type DiffEntry,
  type Failure,
  type ReplayReport,
} from "./eval-reports";

// ── factories ──────────────────────────────────────────────────────

function makeReport(overrides: Partial<ReplayReport> = {}): ReplayReport {
  return {
    fixture_id: "test",
    passed: true,
    failures: [],
    final_text: "answer",
    inference_count: 1,
    tool_count: 0,
    tool_failures: 0,
    total_input_tokens: 10,
    total_output_tokens: 5,
    session_duration_ms: 100,
    elapsed_ms: 100,
    tool_calls_by_agent: [],
    ...overrides,
  };
}

function toolStats(
  agent_id: string,
  tool: string,
  overrides: Partial<AgentToolStats> = {},
): AgentToolStats {
  return {
    agent_id,
    tool,
    call_count: 1,
    failure_count: 0,
    total_duration_ms: 10,
    ...overrides,
  };
}

function tokenFailure(): Failure {
  return { kind: "token_budget_exceeded", budget: 100, actual: 200 };
}

function lineify(reports: ReplayReport[]): string {
  return reports.map((r) => JSON.stringify(r)).join("\n");
}

// ── parseReportsNdjson ─────────────────────────────────────────────

describe("parseReportsNdjson", () => {
  it("returns empty result for empty input", () => {
    const result = parseReportsNdjson("");
    expect(result.reports).toHaveLength(0);
    expect(result.issues).toHaveLength(0);
  });

  it("returns empty result for whitespace-only input", () => {
    const result = parseReportsNdjson("\n\n  \n");
    expect(result.reports).toHaveLength(0);
    expect(result.issues).toHaveLength(0);
  });

  it("parses a single valid line", () => {
    const text = JSON.stringify(makeReport({ fixture_id: "alpha" }));
    const result = parseReportsNdjson(text);
    expect(result.reports).toHaveLength(1);
    expect(result.reports[0]?.fixture_id).toBe("alpha");
    expect(result.issues).toHaveLength(0);
  });

  it("parses multiple lines and preserves order", () => {
    const reports = ["a", "b", "c"].map((id) =>
      makeReport({ fixture_id: id }),
    );
    const result = parseReportsNdjson(lineify(reports));
    expect(result.reports.map((r) => r.fixture_id)).toEqual(["a", "b", "c"]);
    expect(result.issues).toHaveLength(0);
  });

  it("tolerates blank lines between records", () => {
    const text =
      JSON.stringify(makeReport({ fixture_id: "a" })) +
      "\n\n" +
      JSON.stringify(makeReport({ fixture_id: "b" }));
    const result = parseReportsNdjson(text);
    expect(result.reports).toHaveLength(2);
    expect(result.issues).toHaveLength(0);
  });

  it("handles \\r\\n line separators", () => {
    const text =
      JSON.stringify(makeReport({ fixture_id: "a" })) +
      "\r\n" +
      JSON.stringify(makeReport({ fixture_id: "b" }));
    const result = parseReportsNdjson(text);
    expect(result.reports).toHaveLength(2);
  });

  it("records a parse issue for invalid JSON without throwing", () => {
    const text =
      JSON.stringify(makeReport({ fixture_id: "valid" })) + "\nnot-json\n";
    const result = parseReportsNdjson(text);
    expect(result.reports).toHaveLength(1);
    expect(result.issues).toHaveLength(1);
    expect(result.issues[0]?.line).toBe(2);
    expect(result.issues[0]?.raw).toBe("not-json");
  });

  it("records an issue for JSON that doesn't match the ReplayReport shape", () => {
    const text = JSON.stringify({ unrelated: true });
    const result = parseReportsNdjson(text);
    expect(result.reports).toHaveLength(0);
    expect(result.issues).toHaveLength(1);
    expect(result.issues[0]?.message).toContain("ReplayReport");
  });

  it("rejects records with malformed failures array", () => {
    const text = JSON.stringify({
      ...makeReport(),
      failures: [{ kind: "totally_unknown" }],
    });
    const result = parseReportsNdjson(text);
    expect(result.reports).toHaveLength(0);
    expect(result.issues).toHaveLength(1);
  });

  it("accepts every known failure kind", () => {
    const failures: Failure[] = [
      { kind: "answer_missing_phrase", phrase: "x" },
      { kind: "answer_contains_excluded_phrase", phrase: "y" },
      {
        kind: "tool_sequence_mismatch",
        expected: ["a"],
        actual: ["b"],
      },
      { kind: "forbidden_tool_used", tool: "rm" },
      { kind: "token_budget_exceeded", budget: 1, actual: 2 },
      { kind: "duration_exceeded", budget_ms: 1, actual_ms: 2 },
      { kind: "judge_below_threshold", threshold: 0.5, actual: 0.2 },
    ];
    const text = JSON.stringify(makeReport({ failures, passed: false }));
    const result = parseReportsNdjson(text);
    expect(result.reports).toHaveLength(1);
    expect(result.reports[0]?.failures).toHaveLength(failures.length);
  });

  it("issues carry the offending line number", () => {
    const text =
      JSON.stringify(makeReport({ fixture_id: "ok" })) +
      "\n" +
      "garbage" +
      "\n" +
      JSON.stringify(makeReport({ fixture_id: "ok2" }));
    const result = parseReportsNdjson(text);
    expect(result.reports).toHaveLength(2);
    expect(result.issues).toHaveLength(1);
    expect(result.issues[0]?.line).toBe(2);
  });
});

// ── summariseReports ───────────────────────────────────────────────

describe("summariseReports", () => {
  it("returns zeros for empty input", () => {
    const summary = summariseReports([]);
    expect(summary.total).toBe(0);
    expect(summary.passed).toBe(0);
    expect(summary.failed).toBe(0);
    expect(summary.totalInputTokens).toBe(0);
    expect(summary.totalOutputTokens).toBe(0);
    expect(summary.totalSessionMs).toBe(0);
    for (const count of Object.values(summary.failureKindCounts)) {
      expect(count).toBe(0);
    }
  });

  it("counts passed and failed reports", () => {
    const summary = summariseReports([
      makeReport({ passed: true }),
      makeReport({ passed: true }),
      makeReport({ passed: false, failures: [tokenFailure()] }),
    ]);
    expect(summary.total).toBe(3);
    expect(summary.passed).toBe(2);
    expect(summary.failed).toBe(1);
  });

  it("aggregates token and duration totals", () => {
    const summary = summariseReports([
      makeReport({
        total_input_tokens: 10,
        total_output_tokens: 5,
        session_duration_ms: 100,
      }),
      makeReport({
        total_input_tokens: 20,
        total_output_tokens: 7,
        session_duration_ms: 200,
      }),
    ]);
    expect(summary.totalInputTokens).toBe(30);
    expect(summary.totalOutputTokens).toBe(12);
    expect(summary.totalSessionMs).toBe(300);
  });

  it("counts failure kinds", () => {
    const summary = summariseReports([
      makeReport({
        passed: false,
        failures: [
          tokenFailure(),
          { kind: "answer_missing_phrase", phrase: "x" },
        ],
      }),
      makeReport({
        passed: false,
        failures: [tokenFailure()],
      }),
    ]);
    expect(summary.failureKindCounts.token_budget_exceeded).toBe(2);
    expect(summary.failureKindCounts.answer_missing_phrase).toBe(1);
    expect(summary.failureKindCounts.forbidden_tool_used).toBe(0);
  });

  it("returns a fresh object each call (no shared mutable state)", () => {
    const a = summariseReports([
      makeReport({ passed: false, failures: [tokenFailure()] }),
    ]);
    const b = summariseReports([]);
    expect(a.failureKindCounts.token_budget_exceeded).toBe(1);
    expect(b.failureKindCounts.token_budget_exceeded).toBe(0);
  });
});

// ── diffReports ────────────────────────────────────────────────────

describe("diffReports", () => {
  it("returns empty entries when both inputs are empty", () => {
    const summary = diffReports([], []);
    expect(summary.entries).toHaveLength(0);
    expect(summary.isClean).toBe(true);
  });

  it("classifies unchanged when both pass", () => {
    const r = makeReport({ fixture_id: "x", passed: true });
    const summary = diffReports([r], [r]);
    expect(summary.entries).toHaveLength(1);
    expect(summary.entries[0]?.kind).toBe("unchanged");
    expect(summary.isClean).toBe(true);
  });

  it("classifies regression when baseline passed and new failed", () => {
    const summary = diffReports(
      [makeReport({ fixture_id: "x", passed: true })],
      [
        makeReport({
          fixture_id: "x",
          passed: false,
          failures: [tokenFailure()],
        }),
      ],
    );
    expect(summary.regressions).toBe(1);
    expect(summary.isClean).toBe(false);
    const entry = summary.entries[0];
    expect(entry?.kind).toBe("regression");
    if (entry?.kind === "regression") {
      expect(entry.new_failures).toEqual(["token_budget_exceeded"]);
    }
  });

  it("classifies fixed when baseline failed and new passed", () => {
    const summary = diffReports(
      [
        makeReport({
          fixture_id: "x",
          passed: false,
          failures: [tokenFailure()],
        }),
      ],
      [makeReport({ fixture_id: "x", passed: true })],
    );
    expect(summary.fixed).toBe(1);
    expect(summary.isClean).toBe(true);
    expect(summary.entries[0]?.kind).toBe("fixed");
  });

  it("classifies still failing when both failed", () => {
    const summary = diffReports(
      [
        makeReport({
          fixture_id: "x",
          passed: false,
          failures: [tokenFailure()],
        }),
      ],
      [
        makeReport({
          fixture_id: "x",
          passed: false,
          failures: [
            { kind: "duration_exceeded", budget_ms: 1, actual_ms: 2 },
          ],
        }),
      ],
    );
    expect(summary.stillFailing).toBe(1);
    // still_failing does NOT block CI — baseline already failed.
    expect(summary.isClean).toBe(true);
  });

  it("classifies missing_from_new and blocks CI", () => {
    const summary = diffReports(
      [makeReport({ fixture_id: "gone", passed: true })],
      [],
    );
    expect(summary.missing).toBe(1);
    expect(summary.isClean).toBe(false);
    expect(summary.entries[0]?.kind).toBe("missing_from_new");
  });

  it("classifies newly_added without blocking CI", () => {
    const summary = diffReports(
      [],
      [makeReport({ fixture_id: "new", passed: true })],
    );
    expect(summary.added).toBe(1);
    expect(summary.isClean).toBe(true);
  });

  it("classifies newly_added even when failing", () => {
    const summary = diffReports(
      [],
      [
        makeReport({
          fixture_id: "new",
          passed: false,
          failures: [tokenFailure()],
        }),
      ],
    );
    expect(summary.added).toBe(1);
    expect(summary.isClean).toBe(true);
    const entry = summary.entries[0];
    expect(entry?.kind).toBe("newly_added");
    if (entry?.kind === "newly_added") {
      expect(entry.passed).toBe(false);
    }
  });

  it("returns entries sorted by fixture id", () => {
    const summary = diffReports(
      [
        makeReport({ fixture_id: "zeta", passed: true }),
        makeReport({ fixture_id: "alpha", passed: true }),
      ],
      [
        makeReport({ fixture_id: "beta", passed: true }),
        makeReport({ fixture_id: "alpha", passed: true }),
        makeReport({ fixture_id: "zeta", passed: true }),
      ],
    );
    expect(summary.entries.map((e) => e.fixture_id)).toEqual([
      "alpha",
      "beta",
      "zeta",
    ]);
  });

  it("counts each variant correctly in mixed input", () => {
    const summary = diffReports(
      [
        makeReport({ fixture_id: "unchanged", passed: true }),
        makeReport({ fixture_id: "regression", passed: true }),
        makeReport({
          fixture_id: "fixed",
          passed: false,
          failures: [tokenFailure()],
        }),
        makeReport({ fixture_id: "missing", passed: true }),
      ],
      [
        makeReport({ fixture_id: "unchanged", passed: true }),
        makeReport({
          fixture_id: "regression",
          passed: false,
          failures: [tokenFailure()],
        }),
        makeReport({ fixture_id: "fixed", passed: true }),
        makeReport({ fixture_id: "newly_added", passed: true }),
      ],
    );
    expect(summary.unchanged).toBe(1);
    expect(summary.regressions).toBe(1);
    expect(summary.fixed).toBe(1);
    expect(summary.missing).toBe(1);
    expect(summary.added).toBe(1);
    expect(summary.isClean).toBe(false);
  });
});

// ── describeFailure / describeDiffEntry / isBlockingDiff ───────────

describe("describeFailure", () => {
  it("formats every failure kind", () => {
    const cases: Failure[] = [
      { kind: "answer_missing_phrase", phrase: "x" },
      { kind: "answer_contains_excluded_phrase", phrase: "y" },
      { kind: "tool_sequence_mismatch", expected: ["a"], actual: ["b"] },
      { kind: "forbidden_tool_used", tool: "rm" },
      { kind: "token_budget_exceeded", budget: 100, actual: 200 },
      { kind: "duration_exceeded", budget_ms: 100, actual_ms: 200 },
      { kind: "judge_below_threshold", threshold: 0.7, actual: 0.4 },
    ];
    for (const f of cases) {
      const text = describeFailure(f);
      expect(text).toBeTruthy();
      expect(typeof text).toBe("string");
    }
  });

  it("includes the missing phrase verbatim", () => {
    const text = describeFailure({
      kind: "answer_missing_phrase",
      phrase: "banana",
    });
    expect(text).toContain("banana");
  });
});

describe("describeDiffEntry", () => {
  it("formats every diff kind", () => {
    const cases: DiffEntry[] = [
      { kind: "unchanged", fixture_id: "x" },
      { kind: "regression", fixture_id: "x", new_failures: ["token_budget_exceeded"] },
      { kind: "fixed", fixture_id: "x" },
      { kind: "still_failing", fixture_id: "x", new_failures: ["duration_exceeded"] },
      { kind: "missing_from_new", fixture_id: "x" },
      { kind: "newly_added", fixture_id: "x", passed: true },
      { kind: "newly_added", fixture_id: "x", passed: false },
    ];
    for (const e of cases) {
      const text = describeDiffEntry(e);
      expect(text).toBeTruthy();
    }
  });
});

// ── parseReportsNdjson + tool_calls_by_agent ───────────────────────

describe("parseReportsNdjson with tool_calls_by_agent", () => {
  it("parses a record carrying populated tool_calls_by_agent", () => {
    const text = JSON.stringify(
      makeReport({
        fixture_id: "with-agents",
        tool_calls_by_agent: [
          toolStats("planner", "search", { call_count: 3 }),
          toolStats("worker", "write", { call_count: 1, failure_count: 1 }),
        ],
      }),
    );
    const result = parseReportsNdjson(text);
    expect(result.issues).toHaveLength(0);
    expect(result.reports).toHaveLength(1);
    expect(result.reports[0]?.tool_calls_by_agent).toHaveLength(2);
    expect(result.reports[0]?.tool_calls_by_agent[0]?.call_count).toBe(3);
  });

  it("defaults tool_calls_by_agent to [] when the field is omitted", () => {
    // Build a legacy-shaped JSON line without the new field.
    const legacy = {
      fixture_id: "legacy",
      passed: true,
      failures: [],
      final_text: "ok",
      inference_count: 1,
      tool_count: 0,
      tool_failures: 0,
      total_input_tokens: 0,
      total_output_tokens: 0,
      session_duration_ms: 0,
      elapsed_ms: 0,
    };
    const result = parseReportsNdjson(JSON.stringify(legacy));
    expect(result.issues).toHaveLength(0);
    expect(result.reports).toHaveLength(1);
    expect(result.reports[0]?.tool_calls_by_agent).toEqual([]);
  });

  it("rejects records whose tool_calls_by_agent is not an array", () => {
    const text = JSON.stringify({
      ...makeReport(),
      tool_calls_by_agent: "not-array",
    });
    const result = parseReportsNdjson(text);
    expect(result.reports).toHaveLength(0);
    expect(result.issues).toHaveLength(1);
  });

  it("rejects records whose tool_calls_by_agent entry is malformed", () => {
    const text = JSON.stringify({
      ...makeReport(),
      tool_calls_by_agent: [{ agent_id: "x" }], // missing tool/count fields
    });
    const result = parseReportsNdjson(text);
    expect(result.reports).toHaveLength(0);
    expect(result.issues).toHaveLength(1);
  });

  it("accepts an empty tool_calls_by_agent array", () => {
    const text = JSON.stringify(
      makeReport({ tool_calls_by_agent: [] }),
    );
    const result = parseReportsNdjson(text);
    expect(result.reports).toHaveLength(1);
    expect(result.reports[0]?.tool_calls_by_agent).toEqual([]);
  });
});

// ── aggregateToolCallsByAgent ──────────────────────────────────────

describe("aggregateToolCallsByAgent", () => {
  it("returns empty for empty input", () => {
    expect(aggregateToolCallsByAgent([])).toEqual([]);
  });

  it("returns empty when no report has tool calls", () => {
    expect(
      aggregateToolCallsByAgent([makeReport(), makeReport()]),
    ).toEqual([]);
  });

  it("aggregates calls across multiple fixtures for the same (agent,tool)", () => {
    const r1 = makeReport({
      fixture_id: "a",
      tool_calls_by_agent: [
        toolStats("planner", "search", {
          call_count: 2,
          failure_count: 0,
          total_duration_ms: 50,
        }),
      ],
    });
    const r2 = makeReport({
      fixture_id: "b",
      tool_calls_by_agent: [
        toolStats("planner", "search", {
          call_count: 3,
          failure_count: 1,
          total_duration_ms: 70,
        }),
      ],
    });
    const agg = aggregateToolCallsByAgent([r1, r2]);
    expect(agg).toHaveLength(1);
    expect(agg[0]).toEqual({
      agent_id: "planner",
      tool: "search",
      call_count: 5,
      failure_count: 1,
      total_duration_ms: 120,
      fixture_count: 2,
    });
  });

  it("does not double-count fixture_count when an agent/tool appears twice in one fixture", () => {
    // Pre-aggregated within a single report: only one entry per pair, but
    // make sure repeated identical pairs in the same report still bump
    // fixture_count exactly once. (Defensive — Rust never emits duplicate
    // pairs for one report, but parser tolerance matters for hand-edited
    // NDJSON.)
    const r = makeReport({
      tool_calls_by_agent: [
        toolStats("worker", "write", { call_count: 1 }),
        toolStats("worker", "write", { call_count: 4 }),
      ],
    });
    const agg = aggregateToolCallsByAgent([r]);
    expect(agg).toHaveLength(1);
    expect(agg[0]?.call_count).toBe(5);
    expect(agg[0]?.fixture_count).toBe(1);
  });

  it("keeps separate buckets per (agent, tool) pair", () => {
    const r = makeReport({
      tool_calls_by_agent: [
        toolStats("planner", "search"),
        toolStats("planner", "write"),
        toolStats("worker", "search"),
      ],
    });
    const agg = aggregateToolCallsByAgent([r]);
    expect(agg).toHaveLength(3);
  });

  it("returns rows sorted lexicographically by (agent_id, tool)", () => {
    const r = makeReport({
      tool_calls_by_agent: [
        toolStats("zeta", "alpha"),
        toolStats("alpha", "zeta"),
        toolStats("alpha", "alpha"),
      ],
    });
    const agg = aggregateToolCallsByAgent([r]);
    expect(agg.map((a) => `${a.agent_id}/${a.tool}`)).toEqual([
      "alpha/alpha",
      "alpha/zeta",
      "zeta/alpha",
    ]);
  });

  it("handles legacy reports without the field gracefully", () => {
    // hasAnyAgentToolStats happy-path covered separately; here we want
    // aggregation to behave like the field is empty.
    const legacy = makeReport();
    // Force-cast away the field to simulate hand-edited legacy NDJSON.
    delete (legacy as Partial<ReplayReport>).tool_calls_by_agent;
    expect(
      aggregateToolCallsByAgent([legacy as ReplayReport]),
    ).toEqual([]);
  });
});

// ── hasAnyAgentToolStats ───────────────────────────────────────────

describe("hasAnyAgentToolStats", () => {
  it("returns false for empty input", () => {
    expect(hasAnyAgentToolStats([])).toBe(false);
  });

  it("returns false when every report has empty tool_calls_by_agent", () => {
    expect(
      hasAnyAgentToolStats([makeReport(), makeReport()]),
    ).toBe(false);
  });

  it("returns true as soon as one report has a populated bucket", () => {
    expect(
      hasAnyAgentToolStats([
        makeReport(),
        makeReport({
          tool_calls_by_agent: [toolStats("a", "b")],
        }),
      ]),
    ).toBe(true);
  });
});

describe("isBlockingDiff", () => {
  it("returns true only for regression and missing_from_new", () => {
    expect(
      isBlockingDiff({ kind: "regression", fixture_id: "x", new_failures: [] }),
    ).toBe(true);
    expect(isBlockingDiff({ kind: "missing_from_new", fixture_id: "x" })).toBe(
      true,
    );
    expect(isBlockingDiff({ kind: "unchanged", fixture_id: "x" })).toBe(false);
    expect(isBlockingDiff({ kind: "fixed", fixture_id: "x" })).toBe(false);
    expect(
      isBlockingDiff({
        kind: "still_failing",
        fixture_id: "x",
        new_failures: [],
      }),
    ).toBe(false);
    expect(
      isBlockingDiff({ kind: "newly_added", fixture_id: "x", passed: true }),
    ).toBe(false);
  });
});

// ── End-to-end NDJSON written by the Rust CLI ─────────────────────

describe("Rust CLI compatibility", () => {
  it("parses a sample bundled baseline-shaped NDJSON", () => {
    // Same shape as crates/awaken-eval/baseline.ndjson lines.
    const text = [
      '{"fixture_id":"01_simple_qa","passed":true,"failures":[],"final_text":"4","inference_count":1,"tool_count":0,"tool_failures":0,"total_input_tokens":10,"total_output_tokens":1,"session_duration_ms":0,"elapsed_ms":0}',
      '{"fixture_id":"02_phrase_recall","passed":true,"failures":[],"final_text":"green pelican","inference_count":1,"tool_count":0,"tool_failures":0,"total_input_tokens":19,"total_output_tokens":4,"session_duration_ms":0,"elapsed_ms":0}',
    ].join("\n");
    const result = parseReportsNdjson(text);
    expect(result.issues).toHaveLength(0);
    expect(result.reports).toHaveLength(2);
    expect(result.reports[0]?.fixture_id).toBe("01_simple_qa");
    expect(result.reports[1]?.final_text).toBe("green pelican");
  });

  it("parses an NDJSON line carrying tool_sequence_mismatch failure", () => {
    const text = JSON.stringify({
      fixture_id: "x",
      passed: false,
      failures: [
        {
          kind: "tool_sequence_mismatch",
          expected: ["search", "write"],
          actual: ["write"],
        },
      ],
      final_text: "",
      inference_count: 1,
      tool_count: 1,
      tool_failures: 0,
      total_input_tokens: 5,
      total_output_tokens: 5,
      session_duration_ms: 0,
      elapsed_ms: 0,
    });
    const result = parseReportsNdjson(text);
    expect(result.reports).toHaveLength(1);
    const failure = result.reports[0]?.failures[0];
    expect(failure?.kind).toBe("tool_sequence_mismatch");
  });
});

describe("diffReportsMulti (N-way comparison)", () => {
  function rep(id: string, passed: boolean): ReplayReport {
    return {
      fixture_id: id,
      passed,
      failures: passed ? [] : [{ kind: "answer_missing_phrase", phrase: "x" }],
      final_text: "",
      inference_count: 1,
      tool_count: 0,
      tool_failures: 0,
      total_input_tokens: 0,
      total_output_tokens: 0,
      session_duration_ms: 0,
      elapsed_ms: 0,
      tool_calls_by_agent: [],
    };
  }

  it("returns empty for an empty runs list", () => {
    const r = diffReportsMulti([]);
    expect(r.rows).toEqual([]);
    expect(r.runs).toEqual([]);
  });

  it("classifies passed/failed in baseline-only mode", () => {
    const r = diffReportsMulti([
      { label: "A", reports: [rep("a", true), rep("b", false)] },
    ]);
    expect(r.rows.map((x) => x.statuses[0])).toEqual(["passed", "failed"]);
  });

  it("flags regression when baseline passes but later run fails", () => {
    const r = diffReportsMulti([
      { label: "A", reports: [rep("a", true)] },
      { label: "B", reports: [rep("a", false)] },
    ]);
    expect(r.rows[0].statuses).toEqual(["passed", "regression"]);
  });

  it("flags fixed when baseline fails but later run passes", () => {
    const r = diffReportsMulti([
      { label: "A", reports: [rep("a", false)] },
      { label: "B", reports: [rep("a", true)] },
    ]);
    expect(r.rows[0].statuses).toEqual(["failed", "fixed"]);
  });

  it("marks a fixture missing when not present in a run", () => {
    const r = diffReportsMulti([
      { label: "A", reports: [rep("a", true), rep("b", true)] },
      { label: "B", reports: [rep("a", true)] },
    ]);
    const b = r.rows.find((x) => x.fixture_id === "b")!;
    expect(b.statuses).toEqual(["passed", "missing"]);
  });

  it("supports 3+ runs in one matrix; later runs always compared to baseline", () => {
    // baseline (v1) passes → v2 fails → v3 passes again. v3's status is
    // "passed" (it matches the baseline), not "fixed" (which would require
    // baseline to have failed).
    const r = diffReportsMulti([
      { label: "v1", reports: [rep("a", true)] },
      { label: "v2", reports: [rep("a", false)] },
      { label: "v3", reports: [rep("a", true)] },
    ]);
    expect(r.rows[0].statuses).toEqual(["passed", "regression", "passed"]);
    expect(r.runs.map((x) => x.label)).toEqual(["v1", "v2", "v3"]);
  });

  it("flags 'fixed' on a later run only when baseline failed", () => {
    const r = diffReportsMulti([
      { label: "v1", reports: [rep("a", false)] },
      { label: "v2", reports: [rep("a", false)] },
      { label: "v3", reports: [rep("a", true)] },
    ]);
    expect(r.rows[0].statuses).toEqual(["failed", "failed", "fixed"]);
  });
});
