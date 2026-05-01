import type { ReplayReport, DiffEntry } from "./eval-reports";
import { isBlockingDiff } from "./eval-reports";

export type FixtureStatusFilter =
  | "all"
  | "passed"
  | "failed"
  | "regressions"
  | "fixed";

export interface FixtureFilterState {
  status: FixtureStatusFilter;
  search: string;
}

export const DEFAULT_FIXTURE_FILTER: FixtureFilterState = {
  status: "all",
  search: "",
};

/// Filter the per-fixture rows for the report table. The diff lookup is
/// optional — when no baseline is loaded, "regressions" / "fixed" still
/// evaluate but match nothing.
export function filterFixtures(
  reports: ReplayReport[],
  filter: FixtureFilterState,
  diffs: Map<string, DiffEntry> = new Map(),
): ReplayReport[] {
  const tokens = filter.search
    .trim()
    .toLowerCase()
    .split(/\s+/)
    .filter((token) => token.length > 0);

  return reports.filter((report) => {
    if (!matchesStatus(report, filter.status, diffs)) return false;
    if (tokens.length === 0) return true;
    const haystack = report.fixture_id.toLowerCase();
    return tokens.every((token) => haystack.includes(token));
  });
}

function matchesStatus(
  report: ReplayReport,
  status: FixtureStatusFilter,
  diffs: Map<string, DiffEntry>,
): boolean {
  switch (status) {
    case "all":
      return true;
    case "passed":
      return report.passed;
    case "failed":
      return !report.passed;
    case "regressions": {
      const diff = diffs.get(report.fixture_id);
      return diff ? isBlockingDiff(diff) : false;
    }
    case "fixed": {
      const diff = diffs.get(report.fixture_id);
      return diff?.kind === "fixed";
    }
  }
}
