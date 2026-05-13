// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";

import { agentsApi } from "./agents";
import { ConfigApiError } from "./http";

afterEach(() => {
  vi.restoreAllMocks();
});

function mockFetch(status: number, body: unknown) {
  vi.stubGlobal(
    "fetch",
    vi.fn(async () => ({
      ok: status >= 200 && status < 300,
      status,
      text: async () =>
        typeof body === "string" ? body : JSON.stringify(body),
    })) as unknown as typeof fetch,
  );
}

describe("agentsApi.agentPermissionPreview — feature-gate vs missing-agent (R10 #1)", () => {
  it("returns null when the route reports 503 (feature not compiled)", async () => {
    mockFetch(503, { error: "permission feature not compiled into this server build" });

    const result = await agentsApi.agentPermissionPreview("any-agent");
    expect(result).toBeNull();
  });

  it("re-throws ConfigApiError on 404 (agent record not found)", async () => {
    // The route is registered unconditionally on the server, so a 404
    // means the agent itself is missing — NOT that the feature is
    // disabled. Surface as a real error so the editor doesn't
    // silently render "feature unavailable" for a stale agent id.
    mockFetch(404, { error: "agent not found: ghost-agent" });

    await expect(agentsApi.agentPermissionPreview("ghost-agent")).rejects.toBeInstanceOf(
      ConfigApiError,
    );
    await expect(agentsApi.agentPermissionPreview("ghost-agent")).rejects.toMatchObject({
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
