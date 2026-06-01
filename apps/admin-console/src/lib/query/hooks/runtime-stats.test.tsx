// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, renderHook, waitFor } from "@testing-library/react";

import { agentsApi, ConfigApiError, type AgentRuntimeSnapshot } from "../../api";
import { createQueryClientWrapper } from "../../../test/query";
import { useRuntimeStatsQuery } from "./runtime-stats";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

const SNAP: AgentRuntimeSnapshot = {
  agent_id: "alpha",
  window_seconds: 3600,
  bucket_window_seconds: 60,
  bucket_count: 60,
  inference_count: 1,
  error_count: 0,
  input_tokens: 1,
  output_tokens: 1,
  avg_inference_duration_ms: 0,
  min_inference_duration_ms: 0,
  max_inference_duration_ms: 0,
  p50_inference_duration_ms: 0,
  p95_inference_duration_ms: 0,
  p99_inference_duration_ms: 0,
  suspensions: 0,
  handoffs: 0,
  delegations: 0,
  tool_calls_by_tool: [],
};

describe("useRuntimeStatsQuery", () => {
  it("unwraps the { agents: [...] } envelope on success", async () => {
    vi.spyOn(agentsApi, "agentsRuntimeStats").mockResolvedValue({ kind: "ok", agents: [SNAP] });

    const { result } = renderHook(() => useRuntimeStatsQuery(), {
      wrapper: createQueryClientWrapper(),
    });

    await waitFor(() => expect(result.current.isSuccess).toBe(true));
    expect(result.current.data).toEqual({ kind: "ok", agents: [SNAP] });
  });

  it("passes through route_absent and registry_unavailable results", async () => {
    vi.spyOn(agentsApi, "agentsRuntimeStats").mockResolvedValue({ kind: "route_absent" });

    const { result } = renderHook(() => useRuntimeStatsQuery(), {
      wrapper: createQueryClientWrapper(),
    });

    await waitFor(() => expect(result.current.isSuccess).toBe(true));
    expect(result.current.data).toEqual({ kind: "route_absent" });
  });

  it("surfaces auth and unexpected errors via the query error state", async () => {
    const failure = new ConfigApiError(401, "unauthorized");
    vi.spyOn(agentsApi, "agentsRuntimeStats").mockRejectedValue(failure);

    const { result } = renderHook(() => useRuntimeStatsQuery(), {
      wrapper: createQueryClientWrapper(),
    });

    await waitFor(() => expect(result.current.isError).toBe(true), { timeout: 3_000 });
    expect(result.current.error).toBe(failure);
  });
});
