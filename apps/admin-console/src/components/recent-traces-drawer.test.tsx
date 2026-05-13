// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { RecentTracesDrawer } from "./recent-traces-drawer";
import { tracesApi } from "@/lib/api/traces";

function renderWithClient(ui: React.ReactElement) {
  // Each test gets its own fresh client so caches don't leak between tests.
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return render(<QueryClientProvider client={client}>{ui}</QueryClientProvider>);
}

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("RecentTracesDrawer (G7)", () => {
  it("renders nothing when closed", () => {
    renderWithClient(
      <RecentTracesDrawer agentId="agent-a" open={false} onClose={() => {}} />,
    );
    expect(screen.queryByTestId("recent-traces-drawer")).toBeNull();
  });

  it("shows the not-configured state when listAgentTraces returns null", async () => {
    vi.spyOn(tracesApi, "listAgentTraces").mockResolvedValue(null);
    renderWithClient(
      <RecentTracesDrawer agentId="agent-a" open onClose={() => {}} />,
    );
    await waitFor(() => {
      expect(screen.getByTestId("recent-traces-not-configured")).toBeTruthy();
    });
  });

  it("shows the empty state when no runs are recorded", async () => {
    vi.spyOn(tracesApi, "listAgentTraces").mockResolvedValue({ runs: [] });
    renderWithClient(
      <RecentTracesDrawer agentId="agent-a" open onClose={() => {}} />,
    );
    await waitFor(() => {
      expect(screen.getByTestId("recent-traces-empty")).toBeTruthy();
    });
  });

  it("renders the run list with status / experiment / variant / judge pills", async () => {
    vi.spyOn(tracesApi, "listAgentTraces").mockResolvedValue({
      runs: [
        {
          run_id: "abc123def456789",
          agent_id: "agent-a",
          started_at: Math.floor(Date.now() / 1000) - 90,
          ended_at: Math.floor(Date.now() / 1000) - 30,
          prompt_ids: ["p1"],
          final_status: "succeeded",
          experiment_id: "exp-x",
          variant_name: "v1",
          judge_score: 0.87,
        },
        {
          run_id: "deadbeefcafe9999",
          agent_id: "agent-a",
          started_at: Math.floor(Date.now() / 1000) - 10,
          prompt_ids: [],
        },
      ],
    });
    renderWithClient(
      <RecentTracesDrawer agentId="agent-a" open onClose={() => {}} />,
    );
    await waitFor(() => {
      expect(screen.getByTestId("recent-traces-list")).toBeTruthy();
    });
    // First run: succeeded + exp + variant + judge pills.
    expect(screen.getByText("succeeded")).toBeTruthy();
    expect(screen.getByText("exp: exp-x")).toBeTruthy();
    expect(screen.getByText("variant: v1")).toBeTruthy();
    expect(screen.getByText(/judge: 0\.87/)).toBeTruthy();
    // Second run: still-in-flight badge.
    expect(screen.getByText("in flight")).toBeTruthy();
  });

  it("loads events when a run is selected and renders them", async () => {
    vi.spyOn(tracesApi, "listAgentTraces").mockResolvedValue({
      runs: [
        {
          run_id: "run-1",
          agent_id: "agent-a",
          started_at: Math.floor(Date.now() / 1000),
          prompt_ids: [],
          final_status: "succeeded",
        },
      ],
    });
    vi.spyOn(tracesApi, "getTracePage").mockResolvedValue({
      events: [
        { kind: "run_start", ts: 1_000_000 },
        { kind: "tool_call", ts: 1_000_005, payload: { tool: "Bash" } },
      ],
      total: 2,
      next_offset: null,
    });

    renderWithClient(
      <RecentTracesDrawer agentId="agent-a" open onClose={() => {}} />,
    );
    await waitFor(() => screen.getByTestId("recent-traces-list"));
    fireEvent.click(screen.getByText(/run-1/));

    // Wait for the events page to actually load — react-query needs a tick
    // and another paint after the row click before the rows show up.
    const rows = await waitFor(
      () => {
        const found = screen.queryAllByTestId("recent-traces-event-row");
        if (found.length !== 2) throw new Error(`expected 2 rows, got ${found.length}`);
        return found;
      },
      { timeout: 1500 },
    );
    expect(rows.length).toBe(2);
    expect(screen.getByText("run_start")).toBeTruthy();
    expect(screen.getByText("tool_call")).toBeTruthy();
  });

  it("clicking the scrim calls onClose", async () => {
    vi.spyOn(tracesApi, "listAgentTraces").mockResolvedValue({ runs: [] });
    const onClose = vi.fn();
    renderWithClient(
      <RecentTracesDrawer agentId="agent-a" open onClose={onClose} />,
    );
    await waitFor(() => screen.getByTestId("recent-traces-drawer"));
    fireEvent.click(screen.getByTestId("recent-traces-drawer-scrim"));
    expect(onClose).toHaveBeenCalled();
  });
});
