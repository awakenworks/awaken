import { afterEach, describe, expect, it, vi } from "vitest";
import {
  errorRate,
  fetchAgentRuntimeStats,
  fetchAllAgentRuntimeStats,
  formatHistogramLabel,
  formatWindow,
  isAgentRuntimeSnapshot,
  maxHistogramCount,
  toolFailureRate,
  type AgentRuntimeSnapshot,
  type HistogramBucket,
  type ToolRuntimeStats,
} from "./agent-stats";
import { BACKEND_URL } from "./config-api";

// ── factories ──────────────────────────────────────────────────────

function makeSnapshot(
  overrides: Partial<AgentRuntimeSnapshot> = {},
): AgentRuntimeSnapshot {
  return {
    agent_id: "alpha",
    window_seconds: 86400,
    bucket_window_seconds: 600,
    bucket_count: 144,
    inference_count: 0,
    error_count: 0,
    input_tokens: 0,
    output_tokens: 0,
    avg_inference_duration_ms: 0,
    min_inference_duration_ms: 0,
    max_inference_duration_ms: 0,
    p50_inference_duration_ms: 0,
    p95_inference_duration_ms: 0,
    p99_inference_duration_ms: 0,
    inference_duration_histogram: [],
    suspensions: 0,
    handoffs: 0,
    delegations: 0,
    tool_calls_by_tool: [],
    ...overrides,
  };
}

function makeToolStats(overrides: Partial<ToolRuntimeStats> = {}): ToolRuntimeStats {
  return {
    tool: "search",
    call_count: 0,
    failure_count: 0,
    total_duration_ms: 0,
    avg_duration_ms: 0,
    min_duration_ms: 0,
    max_duration_ms: 0,
    p50_duration_ms: 0,
    p95_duration_ms: 0,
    p99_duration_ms: 0,
    duration_histogram: [],
    ...overrides,
  };
}

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

afterEach(() => {
  vi.unstubAllGlobals();
});

// ── isAgentRuntimeSnapshot ─────────────────────────────────────────

describe("isAgentRuntimeSnapshot", () => {
  it("accepts a complete snapshot", () => {
    expect(isAgentRuntimeSnapshot(makeSnapshot())).toBe(true);
  });

  it("rejects null and primitives", () => {
    expect(isAgentRuntimeSnapshot(null)).toBe(false);
    expect(isAgentRuntimeSnapshot(undefined)).toBe(false);
    expect(isAgentRuntimeSnapshot("text")).toBe(false);
    expect(isAgentRuntimeSnapshot(42)).toBe(false);
  });

  it("rejects when a required field is missing", () => {
    const s = makeSnapshot();
    delete (s as Partial<AgentRuntimeSnapshot>).inference_count;
    expect(isAgentRuntimeSnapshot(s)).toBe(false);
  });

  it("rejects when a required field has the wrong type", () => {
    const s = makeSnapshot();
    (s as unknown as Record<string, unknown>).inference_count = "10";
    expect(isAgentRuntimeSnapshot(s)).toBe(false);
  });

  it("rejects when tool_calls_by_tool entry is malformed", () => {
    const s = makeSnapshot({
      tool_calls_by_tool: [
        // @ts-expect-error - intentional malformed shape
        { tool: "search", call_count: 1 },
      ],
    });
    expect(isAgentRuntimeSnapshot(s)).toBe(false);
  });
});

// ── fetchAgentRuntimeStats ─────────────────────────────────────────

describe("fetchAgentRuntimeStats", () => {
  it("encodes the agent id in the URL", async () => {
    const fetchSpy = vi.fn().mockResolvedValue(jsonResponse(makeSnapshot()));
    await fetchAgentRuntimeStats("alpha/beta", fetchSpy);
    expect(fetchSpy).toHaveBeenCalledWith(
      `${BACKEND_URL}/v1/agents/alpha%2Fbeta/runtime-stats`,
    );
  });

  it("appends ?window= when options.window is provided", async () => {
    const fetchSpy = vi.fn().mockResolvedValue(jsonResponse(makeSnapshot()));
    await fetchAgentRuntimeStats("alpha", { window: "1h" }, fetchSpy);
    expect(fetchSpy).toHaveBeenCalledWith(
      `${BACKEND_URL}/v1/agents/alpha/runtime-stats?window=1h`,
    );
  });

  it("omits ?window= when options.window is absent", async () => {
    const fetchSpy = vi.fn().mockResolvedValue(jsonResponse(makeSnapshot()));
    await fetchAgentRuntimeStats("alpha", {}, fetchSpy);
    expect(fetchSpy).toHaveBeenCalledWith(
      `${BACKEND_URL}/v1/agents/alpha/runtime-stats`,
    );
  });

  it("remains backward-compatible when second arg is a fetch function", async () => {
    const fetchSpy = vi.fn().mockResolvedValue(jsonResponse(makeSnapshot()));
    await fetchAgentRuntimeStats("alpha", fetchSpy);
    expect(fetchSpy).toHaveBeenCalledWith(
      `${BACKEND_URL}/v1/agents/alpha/runtime-stats`,
    );
  });

  it("returns ok when server responds with 200", async () => {
    const snap = makeSnapshot({ agent_id: "x", inference_count: 7 });
    const fetchSpy = vi.fn().mockResolvedValue(jsonResponse(snap));
    const result = await fetchAgentRuntimeStats("x", fetchSpy);
    expect(result.kind).toBe("ok");
    if (result.kind === "ok") {
      expect(result.snapshot.inference_count).toBe(7);
    }
  });

  it("returns registry_disabled on 503", async () => {
    const fetchSpy = vi.fn().mockResolvedValue(
      new Response(
        JSON.stringify({ error: "runtime_stats registry not configured" }),
        { status: 503, headers: { "content-type": "application/json" } },
      ),
    );
    const result = await fetchAgentRuntimeStats("alpha", fetchSpy);
    expect(result.kind).toBe("registry_disabled");
  });

  it("returns not_found on 404", async () => {
    const fetchSpy = vi
      .fn()
      .mockResolvedValue(
        new Response(JSON.stringify({ error: "not found" }), { status: 404 }),
      );
    const result = await fetchAgentRuntimeStats("nobody", fetchSpy);
    expect(result.kind).toBe("not_found");
    if (result.kind === "not_found") {
      expect(result.agent_id).toBe("nobody");
    }
  });

  it("returns error for other non-2xx", async () => {
    const fetchSpy = vi
      .fn()
      .mockResolvedValue(new Response("upstream", { status: 502 }));
    const result = await fetchAgentRuntimeStats("alpha", fetchSpy);
    expect(result.kind).toBe("error");
    if (result.kind === "error") {
      expect(result.status).toBe(502);
      expect(result.message).toBe("upstream");
    }
  });

  it("returns error when payload fails the snapshot guard", async () => {
    const fetchSpy = vi
      .fn()
      .mockResolvedValue(jsonResponse({ wrong: "shape" }));
    const result = await fetchAgentRuntimeStats("alpha", fetchSpy);
    expect(result.kind).toBe("error");
    if (result.kind === "error") {
      expect(result.message).toContain("missing required fields");
    }
  });
});

// ── fetchAllAgentRuntimeStats ──────────────────────────────────────

describe("fetchAllAgentRuntimeStats", () => {
  it("returns the agents array on 200", async () => {
    const a = makeSnapshot({ agent_id: "alpha" });
    const b = makeSnapshot({ agent_id: "beta", inference_count: 3 });
    const fetchSpy = vi
      .fn()
      .mockResolvedValue(jsonResponse({ agents: [a, b] }));
    const result = await fetchAllAgentRuntimeStats(fetchSpy);
    expect(result.kind).toBe("ok");
    if (result.kind === "ok") {
      expect(result.agents).toHaveLength(2);
      expect(result.agents[1]?.inference_count).toBe(3);
    }
  });

  it("returns registry_disabled on 503", async () => {
    const fetchSpy = vi
      .fn()
      .mockResolvedValue(new Response(null, { status: 503 }));
    const result = await fetchAllAgentRuntimeStats(fetchSpy);
    expect(result.kind).toBe("registry_disabled");
  });

  it("returns error when the body is not the expected envelope", async () => {
    const fetchSpy = vi.fn().mockResolvedValue(jsonResponse({ wrong: 1 }));
    const result = await fetchAllAgentRuntimeStats(fetchSpy);
    expect(result.kind).toBe("error");
  });

  it("returns error when an item fails the snapshot guard", async () => {
    const fetchSpy = vi
      .fn()
      .mockResolvedValue(jsonResponse({ agents: [{ broken: true }] }));
    const result = await fetchAllAgentRuntimeStats(fetchSpy);
    expect(result.kind).toBe("error");
  });

  it("calls the documented endpoint", async () => {
    const fetchSpy = vi
      .fn()
      .mockResolvedValue(jsonResponse({ agents: [] }));
    await fetchAllAgentRuntimeStats(fetchSpy);
    expect(fetchSpy).toHaveBeenCalledWith(
      `${BACKEND_URL}/v1/agents/runtime-stats`,
    );
  });
});

// ── derived metrics helpers ────────────────────────────────────────

describe("errorRate", () => {
  it("returns 0 when no inferences", () => {
    expect(errorRate(makeSnapshot())).toBe(0);
  });

  it("computes ratio", () => {
    expect(
      errorRate(makeSnapshot({ inference_count: 10, error_count: 3 })),
    ).toBeCloseTo(0.3);
  });
});

describe("toolFailureRate", () => {
  it("returns 0 when no tool calls", () => {
    expect(toolFailureRate(makeSnapshot())).toBe(0);
  });

  it("aggregates calls and failures across all tools", () => {
    const snap = makeSnapshot({
      tool_calls_by_tool: [
        makeToolStats({
          tool: "search",
          call_count: 8,
          failure_count: 1,
        }),
        makeToolStats({
          tool: "write",
          call_count: 2,
          failure_count: 1,
        }),
      ],
    });
    expect(toolFailureRate(snap)).toBeCloseTo(0.2);
  });
});

// ── Histogram + parser back-compat ─────────────────────────────────

describe("isAgentRuntimeSnapshot M12 fields", () => {
  it("accepts snapshot with full histogram + extended percentiles", () => {
    const snap = makeSnapshot({
      inference_count: 5,
      min_inference_duration_ms: 1,
      max_inference_duration_ms: 100,
      p99_inference_duration_ms: 95,
      inference_duration_histogram: [
        { upper_bound_ms: 10, count: 1 },
        { upper_bound_ms: 100, count: 4 },
        { upper_bound_ms: null, count: 0 },
      ],
    });
    expect(isAgentRuntimeSnapshot(snap)).toBe(true);
  });

  it("accepts legacy snapshot without M12 fields and defaults to zero / empty", () => {
    // Strip the M12 fields to simulate a M10/M11 server.
    const legacy = {
      agent_id: "alpha",
      window_seconds: 86400,
      bucket_window_seconds: 600,
      bucket_count: 144,
      inference_count: 1,
      error_count: 0,
      input_tokens: 10,
      output_tokens: 5,
      avg_inference_duration_ms: 100,
      p50_inference_duration_ms: 100,
      p95_inference_duration_ms: 100,
      suspensions: 0,
      handoffs: 0,
      delegations: 0,
      tool_calls_by_tool: [],
    };
    expect(isAgentRuntimeSnapshot(legacy)).toBe(true);
    // Guard mutates the object in place to set defaults; verify them.
    const snap = legacy as unknown as AgentRuntimeSnapshot;
    expect(snap.min_inference_duration_ms).toBe(0);
    expect(snap.max_inference_duration_ms).toBe(0);
    expect(snap.p99_inference_duration_ms).toBe(0);
    expect(snap.inference_duration_histogram).toEqual([]);
  });

  it("rejects when inference_duration_histogram is not an array", () => {
    const snap = makeSnapshot();
    (snap as unknown as Record<string, unknown>).inference_duration_histogram =
      "garbage";
    expect(isAgentRuntimeSnapshot(snap)).toBe(false);
  });

  it("rejects malformed histogram bucket entry", () => {
    const snap = makeSnapshot({
      inference_duration_histogram: [
        // @ts-expect-error - malformed
        { upper_bound_ms: 10 },
      ],
    });
    expect(isAgentRuntimeSnapshot(snap)).toBe(false);
  });

  it("accepts tool stats with optional histogram + percentiles", () => {
    const snap = makeSnapshot({
      tool_calls_by_tool: [
        {
          tool: "search",
          call_count: 3,
          failure_count: 0,
          total_duration_ms: 30,
          avg_duration_ms: 10,
          min_duration_ms: 5,
          max_duration_ms: 15,
          p50_duration_ms: 10,
          p95_duration_ms: 15,
          p99_duration_ms: 15,
          duration_histogram: [{ upper_bound_ms: 25, count: 3 }],
        },
      ],
    });
    expect(isAgentRuntimeSnapshot(snap)).toBe(true);
  });

  it("accepts tool stats without M12 fields (legacy server)", () => {
    const snap = {
      ...makeSnapshot(),
      tool_calls_by_tool: [
        {
          tool: "search",
          call_count: 2,
          failure_count: 0,
          total_duration_ms: 20,
          avg_duration_ms: 10,
        },
      ],
    };
    expect(isAgentRuntimeSnapshot(snap as unknown)).toBe(true);
  });
});

describe("formatHistogramLabel", () => {
  it("formats finite upper bound", () => {
    expect(formatHistogramLabel({ upper_bound_ms: 100, count: 5 })).toBe(
      "≤100 ms",
    );
    expect(formatHistogramLabel({ upper_bound_ms: 10, count: 0 })).toBe(
      "≤10 ms",
    );
  });

  it("formats +infinity bucket", () => {
    expect(formatHistogramLabel({ upper_bound_ms: null, count: 0 })).toBe(
      "> 10000 ms",
    );
  });
});

describe("maxHistogramCount", () => {
  it("returns 0 for empty input", () => {
    expect(maxHistogramCount([])).toBe(0);
  });

  it("returns the max count across buckets", () => {
    const buckets: HistogramBucket[] = [
      { upper_bound_ms: 10, count: 1 },
      { upper_bound_ms: 100, count: 5 },
      { upper_bound_ms: null, count: 2 },
    ];
    expect(maxHistogramCount(buckets)).toBe(5);
  });

  it("treats all-zero counts as 0", () => {
    expect(
      maxHistogramCount([
        { upper_bound_ms: 10, count: 0 },
        { upper_bound_ms: null, count: 0 },
      ]),
    ).toBe(0);
  });
});

describe("formatWindow", () => {
  it("uses h units when divisible by 3600", () => {
    expect(formatWindow(3600)).toBe("1h");
    expect(formatWindow(86400)).toBe("24h");
  });

  it("uses m units when divisible by 60 but not by 3600", () => {
    expect(formatWindow(600)).toBe("10m");
    expect(formatWindow(120)).toBe("2m");
  });

  it("falls back to seconds otherwise", () => {
    expect(formatWindow(45)).toBe("45s");
  });

  it("returns 0s for non-positive input", () => {
    expect(formatWindow(0)).toBe("0s");
    expect(formatWindow(-1)).toBe("0s");
    expect(formatWindow(NaN)).toBe("0s");
  });
});
