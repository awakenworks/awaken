import { afterEach, describe, expect, it, vi } from "vitest";
import { agentsApi } from "./agents";
import { BACKEND_URL, ConfigApiError } from "./http";
import type { AgentRuntimeSnapshot } from "./types";

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function runtimeSnapshot(overrides: Partial<AgentRuntimeSnapshot> = {}): AgentRuntimeSnapshot {
  return {
    agent_id: "agent/a",
    window_seconds: 3600,
    bucket_window_seconds: 60,
    bucket_count: 60,
    inference_count: 2,
    error_count: 0,
    input_tokens: 10,
    output_tokens: 20,
    avg_inference_duration_ms: 100,
    min_inference_duration_ms: 80,
    max_inference_duration_ms: 120,
    p50_inference_duration_ms: 100,
    p95_inference_duration_ms: 115,
    p99_inference_duration_ms: 120,
    suspensions: 0,
    handoffs: 0,
    delegations: 0,
    tool_calls_by_tool: [],
    ...overrides,
  };
}

function mockFetch(status: number, body: unknown) {
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue(jsonResponse(body, status)));
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("agentsApi", () => {
  it("returns all runtime stats when the registry is available", async () => {
    const payload = { agents: [runtimeSnapshot({ agent_id: "alpha" })] };
    const fetchSpy = vi.fn().mockResolvedValue(jsonResponse(payload));
    vi.stubGlobal("fetch", fetchSpy);

    await expect(agentsApi.agentsRuntimeStats()).resolves.toEqual(payload);
    expect(fetchSpy).toHaveBeenCalledWith(
      `${BACKEND_URL}/v1/agents/runtime-stats`,
      undefined,
    );
  });

  it("returns null when runtime stats are disabled", async () => {
    mockFetch(503, { error: "runtime stats disabled" });

    await expect(agentsApi.agentsRuntimeStats()).resolves.toBeNull();
  });

  it("rethrows non-503 runtime stats errors", async () => {
    mockFetch(403, { error: "forbidden" });

    await expect(agentsApi.agentsRuntimeStats()).rejects.toMatchObject({
      name: "ConfigApiError",
      status: 403,
      message: "forbidden",
    });
  });

  it("patches agent overrides with encoded ids and JSON", async () => {
    const fetchSpy = vi.fn().mockResolvedValue(jsonResponse({ id: "agent/a" }));
    vi.stubGlobal("fetch", fetchSpy);

    await agentsApi.patchAgentOverrides("agent/a", { model_id: "fast", max_rounds: null });

    expect(fetchSpy.mock.calls[0][0]).toBe(
      `${BACKEND_URL}/v1/config/agents/agent%2Fa/overrides`,
    );
    const init = fetchSpy.mock.calls[0][1] as RequestInit;
    expect(init.method).toBe("PATCH");
    expect(new Headers(init.headers).get("content-type")).toBe("application/json");
    expect(init.body).toBe(JSON.stringify({ model_id: "fast", max_rounds: null }));
  });

  it("clears all overrides or a single override field", async () => {
    const fetchSpy = vi
      .fn()
      .mockResolvedValueOnce(jsonResponse({ id: "agent/a" }))
      .mockResolvedValueOnce(jsonResponse({ id: "agent/a" }));
    vi.stubGlobal("fetch", fetchSpy);

    await agentsApi.clearAgentOverrides("agent/a");
    await agentsApi.clearAgentOverrideField("agent/a", "system/prompt");

    expect(fetchSpy.mock.calls[0]).toEqual([
      `${BACKEND_URL}/v1/config/agents/agent%2Fa/overrides`,
      { method: "DELETE" },
    ]);
    expect(fetchSpy.mock.calls[1]).toEqual([
      `${BACKEND_URL}/v1/config/agents/agent%2Fa/overrides/system%2Fprompt`,
      { method: "DELETE" },
    ]);
  });
});

describe("agentsApi.agentPermissionPreview", () => {
  it("returns null when the route reports 503", async () => {
    mockFetch(503, { error: "permission feature not compiled into this server build" });

    const result = await agentsApi.agentPermissionPreview("any-agent");
    expect(result).toBeNull();
  });

  it("re-throws ConfigApiError on 404", async () => {
    mockFetch(404, { error: "agent not found: ghost-agent" });

    const request = agentsApi.agentPermissionPreview("ghost-agent");
    await expect(request).rejects.toBeInstanceOf(ConfigApiError);
    await expect(request).rejects.toMatchObject({
      status: 404,
    });
  });

  it("returns the response on 200", async () => {
    mockFetch(200, {
      agent_id: "alpha",
      permission_plugin_enabled: false,
      default_behavior: null,
      candidate_tools: [],
      unconditionally_denied: [],
      effective_tools: [],
      args_conditional_rules: [],
    });

    const result = await agentsApi.agentPermissionPreview("alpha");
    expect(result?.agent_id).toBe("alpha");
  });
});
