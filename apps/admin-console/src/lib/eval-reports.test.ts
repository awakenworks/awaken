import { describe, expect, it } from "vitest";
import {
  describeDiffEntry,
  describeFailure,
  diffReports,
  isBlockingDiff,
  parseReportsNdjson,
  summariseReports,
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
