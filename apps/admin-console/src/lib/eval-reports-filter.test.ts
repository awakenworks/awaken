import { describe, expect, it } from "vitest";
import {
  DEFAULT_FIXTURE_FILTER,
  filterFixtures,
  type FixtureFilterState,
} from "./eval-reports-filter";
import type { DiffEntry, ReplayReport } from "./eval-reports";

function makeReport(overrides: Partial<ReplayReport> = {}): ReplayReport {
  return {
    fixture_id: "fixture-a",
    passed: true,
    failures: [],
    final_text: "",
    inference_count: 0,
    total_input_tokens: 0,
    total_output_tokens: 0,
    session_duration_ms: 0,
    tool_calls: [],
    tool_calls_by_agent: [],
    ...overrides,
  } as ReplayReport;
}

const REPORTS: ReplayReport[] = [
  makeReport({ fixture_id: "checkout", passed: true }),
  makeReport({ fixture_id: "search", passed: false }),
  makeReport({ fixture_id: "auth-login", passed: true }),
];

const DIFFS: Map<string, DiffEntry> = new Map([
  ["search", { kind: "regression", fixture_id: "search" } as DiffEntry],
  ["auth-login", { kind: "fixed", fixture_id: "auth-login" } as DiffEntry],
]);

function withFilter(overrides: Partial<FixtureFilterState>): FixtureFilterState {
  return { ...DEFAULT_FIXTURE_FILTER, ...overrides };
}

describe("filterFixtures", () => {
  it("returns every report with the default filter", () => {
    expect(filterFixtures(REPORTS, DEFAULT_FIXTURE_FILTER, DIFFS)).toEqual(REPORTS);
  });

  it("filters to passing fixtures", () => {
    expect(
      filterFixtures(REPORTS, withFilter({ status: "passed" })).map(
        (r) => r.fixture_id,
      ),
    ).toEqual(["checkout", "auth-login"]);
  });

  it("filters to failing fixtures", () => {
    expect(
      filterFixtures(REPORTS, withFilter({ status: "failed" })).map(
        (r) => r.fixture_id,
      ),
    ).toEqual(["search"]);
  });

  it("filters to regressions when a baseline diff is provided", () => {
    expect(
      filterFixtures(
        REPORTS,
        withFilter({ status: "regressions" }),
        DIFFS,
      ).map((r) => r.fixture_id),
    ).toEqual(["search"]);
  });

  it("filters to newly fixed fixtures", () => {
    expect(
      filterFixtures(REPORTS, withFilter({ status: "fixed" }), DIFFS).map(
        (r) => r.fixture_id,
      ),
    ).toEqual(["auth-login"]);
  });

  it("returns nothing when regressions are requested but no diff is loaded", () => {
    expect(filterFixtures(REPORTS, withFilter({ status: "regressions" }))).toEqual([]);
  });

  it("matches search tokens case-insensitively against the fixture id", () => {
    expect(
      filterFixtures(REPORTS, withFilter({ search: "AUTH" })).map(
        (r) => r.fixture_id,
      ),
    ).toEqual(["auth-login"]);
  });

  it("requires every search token to match", () => {
    expect(
      filterFixtures(REPORTS, withFilter({ search: "auth login" })).map(
        (r) => r.fixture_id,
      ),
    ).toEqual(["auth-login"]);
    expect(
      filterFixtures(REPORTS, withFilter({ search: "auth zzz" })),
    ).toEqual([]);
  });
});
