// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import "../lib/i18n";
import { TraceDetailPanel } from "./trace-detail-panel";
import type { ReplayReport } from "../lib/eval-reports";

afterEach(() => cleanup());

function makeReport(overrides: Partial<ReplayReport> = {}): ReplayReport {
  return {
    fixture_id: "fx-001",
    passed: true,
    failures: [],
    final_text: "the final response",
    inference_count: 3,
    tool_count: 2,
    tool_failures: 0,
    total_input_tokens: 1234,
    total_output_tokens: 567,
    session_duration_ms: 1840,
    elapsed_ms: 1840,
    tool_calls_by_agent: [],
    ...overrides,
  };
}

describe("TraceDetailPanel", () => {
  it("renders the fixture id in the header", () => {
    render(<TraceDetailPanel report={makeReport()} onClose={() => {}} />);
    expect(screen.getByText("fx-001")).toBeDefined();
  });

  it("renders pass pill when report passed and no failures", () => {
    render(<TraceDetailPanel report={makeReport({ passed: true })} onClose={() => {}} />);
    expect(screen.getByText(/pass/i)).toBeDefined();
  });

  it("renders fail pill + failure rows for a failed report", () => {
    const r = makeReport({
      passed: false,
      failures: [
        { kind: "judge_below_threshold", threshold: 0.8, actual: 0.5 },
        { kind: "answer_missing_phrase", phrase: "summary" },
      ],
    });
    render(<TraceDetailPanel report={r} onClose={() => {}} />);
    expect(screen.getAllByText(/fail/i).length).toBeGreaterThan(0);
    // Both scorer categories surface
    expect(screen.getByText(/LLM judge/i)).toBeDefined();
    expect(screen.getByText(/heuristic/i)).toBeDefined();
  });

  it("invokes onClose on the header Close button", () => {
    const onClose = vi.fn();
    render(<TraceDetailPanel report={makeReport()} onClose={onClose} />);
    // Both the backdrop and the header carry aria-label="Close" — the header
    // button has a textContent, the backdrop is empty.
    const closeButtons = screen.getAllByRole("button", { name: /close/i });
    const headerClose = closeButtons.find((b) => (b.textContent ?? "").trim().length > 0);
    expect(headerClose).toBeDefined();
    fireEvent.click(headerClose!);
    expect(onClose).toHaveBeenCalled();
  });

  it("invokes onClose on the backdrop click", () => {
    const onClose = vi.fn();
    render(<TraceDetailPanel report={makeReport()} onClose={onClose} />);
    const closeButtons = screen.getAllByRole("button", { name: /close/i });
    const backdrop = closeButtons.find((b) => (b.textContent ?? "").trim().length === 0);
    expect(backdrop).toBeDefined();
    fireEvent.click(backdrop!);
    expect(onClose).toHaveBeenCalled();
  });

  it("invokes onClose on Escape key", () => {
    const onClose = vi.fn();
    render(<TraceDetailPanel report={makeReport()} onClose={onClose} />);
    fireEvent.keyDown(document, { key: "Escape" });
    expect(onClose).toHaveBeenCalled();
  });

  it("formats latency in seconds with 2 decimals", () => {
    render(<TraceDetailPanel report={makeReport({ session_duration_ms: 3840 })} onClose={() => {}} />);
    expect(screen.getByText("3.84s")).toBeDefined();
  });

  it("renders per-agent tool table when stats are present", () => {
    const r = makeReport({
      tool_calls_by_agent: [
        { agent_id: "research", tool: "web.fetch", call_count: 5, failure_count: 1, total_duration_ms: 1200 },
      ],
    });
    render(<TraceDetailPanel report={r} onClose={() => {}} />);
    expect(screen.getByText("research")).toBeDefined();
    expect(screen.getByText("web.fetch")).toBeDefined();
  });
});
