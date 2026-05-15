// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { EvalReportsPage } from "./eval-reports-page";
import type { ReplayReport } from "@/lib/eval-reports";
import type { FixtureStatusFilter } from "@/lib/eval-reports-filter";

const filterState = vi.hoisted(() => ({
  status: "all" as FixtureStatusFilter,
  search: "",
  apply: vi.fn(),
}));

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (key: string, params?: Record<string, unknown>) => {
      if (!params || Object.keys(params).length === 0) return key;
      return `${key} ${Object.entries(params)
        .map(([name, value]) => `${name}=${String(value)}`)
        .join(" ")}`;
    },
  }),
}));

vi.mock("@/lib/list-url-state", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/lib/list-url-state")>();
  return {
    ...actual,
    useFixtureFilterUrlState: () => filterState,
  };
});

function report(overrides: Partial<ReplayReport>): ReplayReport {
  return {
    fixture_id: "fixture-pass",
    passed: true,
    failures: [],
    final_text: "final answer",
    inference_count: 1,
    tool_count: 0,
    tool_failures: 0,
    total_input_tokens: 10,
    total_output_tokens: 20,
    session_duration_ms: 100,
    elapsed_ms: 120,
    tool_calls_by_agent: [],
    ...overrides,
  };
}

function ndjsonFile(name: string, rows: Array<ReplayReport | string>): File {
  return new File(
    [
      rows
        .map((row) => (typeof row === "string" ? row : JSON.stringify(row)))
        .join("\n"),
    ],
    name,
    { type: "application/x-ndjson" },
  );
}

function renderReports() {
  return render(<EvalReportsPage />);
}

async function upload(input: HTMLInputElement, file: File) {
  fireEvent.change(input, { target: { files: [file] } });
  await screen.findByText(file.name);
}

beforeEach(() => {
  filterState.status = "all";
  filterState.search = "";
  filterState.apply = vi.fn();
});

afterEach(() => {
  cleanup();
});

describe("EvalReportsPage", () => {
  it("loads report and baseline NDJSON, renders summary/diff/tool panels, and opens trace details", async () => {
    const current = ndjsonFile("current.ndjson", [
      report({
        fixture_id: "fixture-pass",
        passed: true,
        total_input_tokens: 100,
        total_output_tokens: 50,
        session_duration_ms: 250,
        tool_count: 1,
        tool_calls_by_agent: [
          { agent_id: "planner", tool: "search", call_count: 2, failure_count: 0, total_duration_ms: 40 },
        ],
      }),
      report({
        fixture_id: "fixture-regression",
        passed: false,
        failures: [
          { kind: "answer_missing_phrase", phrase: "approved" },
          { kind: "judge_below_threshold", threshold: 0.8, actual: 0.5 },
        ],
        final_text: "missing required content",
        total_input_tokens: 30,
        total_output_tokens: 10,
        session_duration_ms: 600,
        elapsed_ms: 650,
        tool_count: 1,
        tool_failures: 1,
        tool_calls_by_agent: [
          { agent_id: "writer", tool: "browser", call_count: 1, failure_count: 1, total_duration_ms: 80 },
        ],
      }),
      report({ fixture_id: "fixture-fixed", passed: true, final_text: "fixed now" }),
      "{bad json",
    ]);
    const baseline = ndjsonFile("baseline.ndjson", [
      report({ fixture_id: "fixture-pass", passed: true }),
      report({ fixture_id: "fixture-regression", passed: true }),
      report({
        fixture_id: "fixture-fixed",
        passed: false,
        failures: [{ kind: "forbidden_tool_used", tool: "shell" }],
      }),
    ]);

    const { container } = renderReports();
    const fileInputs = Array.from(container.querySelectorAll('input[type="file"]')) as HTMLInputElement[];
    expect(fileInputs).toHaveLength(2);

    await upload(fileInputs[0], current);
    await upload(fileInputs[1], baseline);

    expect(screen.getByText("evals.title")).toBeTruthy();
    expect(screen.getByText("current.ndjson")).toBeTruthy();
    expect(screen.getByText(/1 parse issue\(s\)/)).toBeTruthy();
    expect(screen.getByText("baseline.ndjson")).toBeTruthy();
    expect(screen.getByText("Baseline diff")).toBeTruthy();
    expect(screen.getByText("Blocking")).toBeTruthy();
    expect(screen.getByText("Tool calls by agent")).toBeTruthy();
    expect(screen.getByText("planner")).toBeTruthy();
    expect(screen.getByText("writer")).toBeTruthy();
    expect(screen.getByText("Report parse issues (1)")).toBeTruthy();
    expect(screen.getByText(/line 4:/)).toBeTruthy();

    expect(screen.getByText("fixture-pass")).toBeTruthy();
    expect(screen.getByText("fixture-regression")).toBeTruthy();
    expect(screen.getByText('Missing phrase: "approved"')).toBeTruthy();
    expect(screen.getByText("Judge score 0.50 below threshold 0.80")).toBeTruthy();
    expect(screen.getByText("Regression: answer_missing_phrase, judge_below_threshold")).toBeTruthy();
    expect(screen.getAllByText("Fixed").length).toBeGreaterThanOrEqual(1);

    fireEvent.click(screen.getByRole("tab", { name: "Regressions" }));
    expect(filterState.apply).toHaveBeenCalledWith({ status: "regressions" });
    fireEvent.change(screen.getByPlaceholderText("Search by fixture id…"), {
      target: { value: "fixture-regression" },
    });
    expect(filterState.apply).toHaveBeenCalledWith({ search: "fixture-regression" });

    fireEvent.click(screen.getByText("fixture-regression").closest("tr")!);
    expect(await screen.findByRole("dialog", { name: "trace.title" })).toBeTruthy();
    expect(screen.getByText("missing required content")).toBeTruthy();
    expect(screen.getByText("trace.scorers.fail")).toBeTruthy();
    expect(screen.getByText("LLM judge")).toBeTruthy();
    expect(screen.getByText("heuristic")).toBeTruthy();

    fireEvent.click(screen.getAllByRole("button", { name: "common.close" })[1]);
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "trace.title" })).toBeNull());
  });

  it("disables diff-only filters until a baseline is loaded and shows empty filtered results", async () => {
    filterState.status = "regressions";
    filterState.search = "missing";
    const current = ndjsonFile("single.ndjson", [report({ fixture_id: "fixture-pass" })]);

    const { container } = renderReports();
    const input = container.querySelector('input[type="file"]') as HTMLInputElement;
    await upload(input, current);

    expect((screen.getByRole("tab", { name: "Regressions" }) as HTMLButtonElement).disabled).toBe(
      true,
    );
    expect((screen.getByRole("tab", { name: "Newly fixed" }) as HTMLButtonElement).disabled).toBe(
      true,
    );
    expect(screen.getByText("No fixtures match the current filter.")).toBeTruthy();
  });

  it("renders an empty report table and clears selected files", async () => {
    const empty = ndjsonFile("empty.ndjson", []);

    const { container } = renderReports();
    const input = container.querySelector('input[type="file"]') as HTMLInputElement;
    await upload(input, empty);

    expect(screen.getByText("The report contained no fixtures.")).toBeTruthy();

    fireEvent.click(screen.getAllByText("Clear")[0]);
    expect(screen.queryByText("empty.ndjson")).toBeNull();
    expect(screen.queryByText("The report contained no fixtures.")).toBeNull();
  });
});
