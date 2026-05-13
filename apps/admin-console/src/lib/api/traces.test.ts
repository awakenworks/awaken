// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";

import { tracesApi } from "./traces";
import { ConfigApiError } from "./http";

afterEach(() => {
  vi.restoreAllMocks();
});

interface FakeResponse {
  ok: boolean;
  status: number;
  headers: { get(name: string): string | null };
  text: () => Promise<string>;
}

function mockFetchOnce(response: FakeResponse) {
  vi.stubGlobal("fetch", vi.fn(async () => response) as unknown as typeof fetch);
}

describe("tracesApi.listAgentTraces — disabled vs missing-agent (R13)", () => {
  // `listAgentTraces` previously swallowed BOTH 503 and 404 to `null`,
  // so a missing agent id surfaced in the UI as "trace persistence not
  // enabled" — a misleading message that pointed the operator at a
  // server-build issue rather than the real cause (404). Match
  // `getTracePage` semantics: 503 is the "feature gate" signal and stays
  // null; everything else (including 404) re-throws so callers render the
  // actual error.
  it("returns null on 503 (trace store not configured)", async () => {
    mockFetchOnce({
      ok: false,
      status: 503,
      headers: { get: () => null },
      text: async () => "trace store not configured",
    });
    const result = await tracesApi.listAgentTraces("alpha");
    expect(result).toBeNull();
  });

  it("throws on 404 (unknown agent) — no longer collapsed to disabled", async () => {
    mockFetchOnce({
      ok: false,
      status: 404,
      headers: { get: () => null },
      text: async () => "agent not found: ghost",
    });
    await expect(tracesApi.listAgentTraces("ghost")).rejects.toBeInstanceOf(
      ConfigApiError,
    );
    await expect(tracesApi.listAgentTraces("ghost")).rejects.toMatchObject({
      status: 404,
    });
  });

  it("returns parsed list on 200", async () => {
    mockFetchOnce({
      ok: true,
      status: 200,
      headers: { get: () => null },
      text: async () => JSON.stringify({ runs: [{ run_id: "r1", agent_id: "alpha" }] }),
    });
    const result = await tracesApi.listAgentTraces("alpha");
    expect(result).not.toBeNull();
    expect(result?.runs).toHaveLength(1);
  });
});

describe("tracesApi.getTracePage — disabled vs missing-run (R10 #2)", () => {
  it("returns null on 503 (trace store not configured)", async () => {
    mockFetchOnce({
      ok: false,
      status: 503,
      headers: { get: () => null },
      text: async () => "trace store not configured",
    });

    const result = await tracesApi.getTracePage("run-abc");
    expect(result).toBeNull();
  });

  it("throws on 404 (unknown run) — no longer collapsed to empty events", async () => {
    // Pre-R10 behavior returned `{events: [], total: 0}` for a missing
    // run, so the UI silently rendered "0 events" instead of a real
    // not-found error. After R10 #2 the caller has to handle the
    // ConfigApiError.
    mockFetchOnce({
      ok: false,
      status: 404,
      headers: { get: () => null },
      text: async () => "run not found: ghost-run",
    });

    await expect(tracesApi.getTracePage("ghost-run")).rejects.toBeInstanceOf(
      ConfigApiError,
    );
    await expect(tracesApi.getTracePage("ghost-run")).rejects.toMatchObject({
      status: 404,
    });
  });

  it("returns parsed events on 200", async () => {
    mockFetchOnce({
      ok: true,
      status: 200,
      headers: {
        get: (name: string) => {
          if (name === "x-trace-total-events") return "3";
          if (name === "x-trace-next-offset") return null;
          return null;
        },
      },
      text: async () =>
        [
          `{"kind":"run_started","ts":1}`,
          `{"kind":"tool_call","ts":2}`,
          `{"kind":"run_finished","ts":3}`,
        ].join("\n"),
    });

    const result = await tracesApi.getTracePage("run-abc");
    expect(result).not.toBeNull();
    expect(result?.events).toHaveLength(3);
    expect(result?.total).toBe(3);
    expect(result?.next_offset).toBeNull();
  });
});
