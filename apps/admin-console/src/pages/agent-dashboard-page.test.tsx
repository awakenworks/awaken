// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router";
import { AgentDashboardPage, type AgentRuntimeSnapshot } from "./agent-dashboard-page";
import type { AgentRuntimeStatsResult } from "@/lib/agent-stats";

const statsState = vi.hoisted(() => ({
  value: {
    data: undefined as AgentRuntimeStatsResult | undefined,
    error: null as unknown,
    isFetching: false,
    refetch: vi.fn(),
  },
  calls: [] as Array<{ id: string | undefined; window: string }>,
}));

vi.mock("@/lib/query/hooks/agent-stats", () => ({
  useAgentRuntimeStatsQuery: (id: string | undefined, window: string) => {
    statsState.calls.push({ id, window });
    return statsState.value;
  },
}));

function snapshot(overrides: Partial<AgentRuntimeSnapshot> = {}): AgentRuntimeSnapshot {
  return {
    agent_id: "research-agent",
    window_seconds: 3600,
    bucket_window_seconds: 60,
    bucket_count: 60,
    inference_count: 10,
    error_count: 2,
    input_tokens: 1234,
    output_tokens: 567,
    avg_inference_duration_ms: 321.5,
    min_inference_duration_ms: 50,
    max_inference_duration_ms: 900,
    p50_inference_duration_ms: 300,
    p95_inference_duration_ms: 700,
    p99_inference_duration_ms: 850,
    inference_duration_histogram: [
      { upper_bound_ms: 250, count: 4 },
      { upper_bound_ms: null, count: 1 },
    ],
    suspensions: 1,
    handoffs: 2,
    delegations: 3,
    tool_calls_by_tool: [
      {
        tool: "web.search",
        call_count: 5,
        failure_count: 1,
        total_duration_ms: 250,
        avg_duration_ms: 50,
        min_duration_ms: 10,
        max_duration_ms: 100,
        p50_duration_ms: 45,
        p95_duration_ms: 90,
        p99_duration_ms: 100,
        duration_histogram: [
          { upper_bound_ms: 50, count: 3 },
          { upper_bound_ms: null, count: 2 },
        ],
      },
    ],
    ...overrides,
  };
}

function renderDashboard(entry = "/agents/research-agent/dashboard") {
  return render(
    <MemoryRouter initialEntries={[entry]}>
      <Routes>
        <Route path="/agents/:id/dashboard" element={<AgentDashboardPage />} />
        <Route path="/missing" element={<AgentDashboardPage />} />
      </Routes>
    </MemoryRouter>,
  );
}

beforeEach(() => {
  statsState.value = {
    data: undefined,
    error: null,
    isFetching: false,
    refetch: vi.fn(),
  };
  statsState.calls = [];
});

afterEach(() => {
  cleanup();
});

describe("AgentDashboardPage", () => {
  it("renders missing id, loading, query error, and server error states", () => {
    renderDashboard("/missing");
    expect(screen.getByText("Missing agent id.")).not.toBeNull();

    cleanup();
    const { rerender } = renderDashboard();
    expect(screen.getByText("Loading runtime stats…")).not.toBeNull();

    statsState.value = { ...statsState.value, error: new Error("stats unavailable") };
    rerender(
      <MemoryRouter initialEntries={["/agents/research-agent/dashboard"]}>
        <Routes>
          <Route path="/agents/:id/dashboard" element={<AgentDashboardPage />} />
        </Routes>
      </MemoryRouter>,
    );
    expect(screen.getByText("stats unavailable")).not.toBeNull();

    statsState.value = {
      ...statsState.value,
      error: null,
      data: { kind: "error", status: 500, message: "runtime failed" },
    };
    rerender(
      <MemoryRouter initialEntries={["/agents/research-agent/dashboard"]}>
        <Routes>
          <Route path="/agents/:id/dashboard" element={<AgentDashboardPage />} />
        </Routes>
      </MemoryRouter>,
    );
    expect(screen.getByText("HTTP 500: runtime failed")).not.toBeNull();
    expect(screen.getByRole("link", { name: "Edit configuration" }).getAttribute("href")).toBe(
      "/agents/research-agent",
    );
  });

  it("renders registry disabled and not-yet-seen states with retry actions", () => {
    statsState.value = {
      ...statsState.value,
      data: { kind: "registry_disabled" },
    };
    const { rerender } = renderDashboard();

    expect(screen.getByText("Runtime stats not configured")).not.toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Retry" }));
    expect(statsState.value.refetch).toHaveBeenCalledTimes(1);

    statsState.value = {
      ...statsState.value,
      data: { kind: "not_found", agent_id: "new-agent" },
      refetch: vi.fn(),
    };
    rerender(
      <MemoryRouter initialEntries={["/agents/research-agent/dashboard"]}>
        <Routes>
          <Route path="/agents/:id/dashboard" element={<AgentDashboardPage />} />
        </Routes>
      </MemoryRouter>,
    );

    expect(screen.getByText("No runtime activity yet")).not.toBeNull();
    expect(screen.getByText("new-agent")).not.toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    expect(statsState.value.refetch).toHaveBeenCalledTimes(1);
    expect(screen.getByRole("link", { name: "Audit history" }).getAttribute("href")).toContain(
      "agents%2Fnew-agent",
    );
  });

  it("renders a populated runtime snapshot with histograms, tools, quick actions, and window changes", () => {
    statsState.value = {
      ...statsState.value,
      data: { kind: "ok", snapshot: snapshot() },
    };

    renderDashboard("/agents/research-agent/dashboard?window=1h");

    expect(screen.getByText("Dashboard · research-agent")).not.toBeNull();
    expect(screen.getByText("Runtime health")).not.toBeNull();
    expect(screen.getAllByText("20.0%")).toHaveLength(2);
    expect(screen.getByText("Inference latency distribution")).not.toBeNull();
    expect(screen.getByText("Lifecycle events")).not.toBeNull();
    expect(screen.getByText("Tool failure rate")).not.toBeNull();
    expect(screen.getAllByText("web.search")).toHaveLength(2);
    expect(screen.getByText("Tool latency distributions")).not.toBeNull();
    expect(screen.getByRole("link", { name: "Permission rules" }).getAttribute("href")).toBe(
      "/agents/research-agent?tab=plugins",
    );

    fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    expect(statsState.value.refetch).toHaveBeenCalledTimes(1);

    fireEvent.change(screen.getByLabelText("Window:"), { target: { value: "7d" } });
    expect(statsState.calls.at(-1)).toEqual({ id: "research-agent", window: "7d" });

    fireEvent.change(screen.getByLabelText("Window:"), { target: { value: "" } });
    expect(statsState.calls.at(-1)).toEqual({ id: "research-agent", window: "" });
  });

  it("renders zero-error and no-tools snapshots without optional histogram sections", () => {
    statsState.value = {
      ...statsState.value,
      isFetching: true,
      data: {
        kind: "ok",
        snapshot: snapshot({
          error_count: 0,
          inference_duration_histogram: [],
          tool_calls_by_tool: [],
        }),
      },
    };

    renderDashboard();

    expect(screen.getAllByText("0.0%")).toHaveLength(2);
    expect(screen.getByText("Refreshing…")).not.toBeNull();
    expect(screen.queryByText("Inference latency distribution")).toBeNull();
    expect(screen.getByText("No tool invocations recorded in the current window.")).not.toBeNull();
    expect(screen.queryByText("Tool latency distributions")).toBeNull();
  });
});
