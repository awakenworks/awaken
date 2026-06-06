// Client for the Rust-side agent runtime-stats endpoints.
//
// Mirrors `awaken_ext_observability::AgentRuntimeSnapshot` and the
// awaken-server routes added in M10.2.  All shapes are 1:1 with the
// Rust JSON serialisation; drift here will surface as parse errors.

import { BACKEND_URL } from "./config-api";
import { adminAuthHeaders } from "./api/http";

/// One bin of a duration histogram. `upper_bound_ms === null` is the
/// catch-all `+infinity` bucket.
export type HistogramBucket = {
  upper_bound_ms: number | null;
  count: number;
};

export type ToolRuntimeStats = {
  tool: string;
  call_count: number;
  failure_count: number;
  total_duration_ms: number;
  avg_duration_ms: number;
  min_duration_ms: number;
  max_duration_ms: number;
  p50_duration_ms: number;
  p95_duration_ms: number;
  p99_duration_ms: number;
  duration_histogram: HistogramBucket[];
};

export type AgentRuntimeSnapshot = {
  agent_id: string;
  window_seconds: number;
  bucket_window_seconds: number;
  bucket_count: number;
  inference_count: number;
  error_count: number;
  input_tokens: number;
  output_tokens: number;
  avg_inference_duration_ms: number;
  min_inference_duration_ms: number;
  max_inference_duration_ms: number;
  p50_inference_duration_ms: number;
  p95_inference_duration_ms: number;
  p99_inference_duration_ms: number;
  inference_duration_histogram: HistogramBucket[];
  suspensions: number;
  handoffs: number;
  delegations: number;
  tool_calls_by_tool: ToolRuntimeStats[];
};

/// What a `fetchAgentRuntimeStats` call resolves to.  We surface 503
/// (registry not configured) as a distinct case because it's an
/// expected operational state, not an error.
export type AgentRuntimeStatsResult =
  | { kind: "ok"; snapshot: AgentRuntimeSnapshot }
  | { kind: "registry_disabled" }
  | { kind: "not_found"; agent_id: string }
  | { kind: "error"; status: number; message: string };


export type FetchAgentRuntimeStatsOptions = {
  /** Optional window string forwarded as `?window=`, e.g. `"1h"`, `"7d"`. */
  window?: string;
};

/// Fetch the rolling-window snapshot for a single agent.
export async function fetchAgentRuntimeStats(
  agentId: string,
  options?: FetchAgentRuntimeStatsOptions | typeof fetch,
  fetchImpl: typeof fetch = globalThis.fetch,
): Promise<AgentRuntimeStatsResult> {
  // Back-compat: second param used to be `fetchImpl` directly.
  let opts: FetchAgentRuntimeStatsOptions = {};
  if (typeof options === "function") {
    fetchImpl = options;
  } else if (options != null) {
    opts = options;
  }
  let url = `${BACKEND_URL}/v1/agents/${encodeURIComponent(agentId)}/runtime-stats`;
  if (opts.window) {
    url += `?window=${encodeURIComponent(opts.window)}`;
  }
  const resp = await fetchImpl(url, { headers: adminAuthHeaders() });
  if (resp.status === 503) {
    return { kind: "registry_disabled" };
  }
  if (resp.status === 404) {
    return { kind: "not_found", agent_id: agentId };
  }
  if (!resp.ok) {
    const text = await safeText(resp);
    return { kind: "error", status: resp.status, message: text };
  }
  const json = (await resp.json()) as unknown;
  if (!isAgentRuntimeSnapshot(json)) {
    return {
      kind: "error",
      status: resp.status,
      message: "snapshot payload missing required fields",
    };
  }
  return { kind: "ok", snapshot: json };
}

export type AgentRuntimeStatsListResult =
  | { kind: "ok"; agents: AgentRuntimeSnapshot[] }
  | { kind: "registry_disabled" }
  | { kind: "error"; status: number; message: string };

/// Fetch snapshots for every known agent.  Returns sorted-by-id list.
export async function fetchAllAgentRuntimeStats(
  fetchImpl: typeof fetch = globalThis.fetch,
): Promise<AgentRuntimeStatsListResult> {
  const url = `${BACKEND_URL}/v1/agents/runtime-stats`;
  const resp = await fetchImpl(url, { headers: adminAuthHeaders() });
  if (resp.status === 503) {
    return { kind: "registry_disabled" };
  }
  if (!resp.ok) {
    const text = await safeText(resp);
    return { kind: "error", status: resp.status, message: text };
  }
  const json = (await resp.json()) as unknown;
  if (
    typeof json !== "object" ||
    json === null ||
    !Array.isArray((json as { agents?: unknown[] }).agents) ||
    !(json as { agents: unknown[] }).agents.every(isAgentRuntimeSnapshot)
  ) {
    return {
      kind: "error",
      status: resp.status,
      message: "list payload missing 'agents' array",
    };
  }
  return {
    kind: "ok",
    agents: (json as { agents: AgentRuntimeSnapshot[] }).agents,
  };
}

// ── Type guards ─────────────────────────────────────────────────────

function isHistogramBucket(value: unknown): value is HistogramBucket {
  if (typeof value !== "object" || value === null) return false;
  const v = value as Record<string, unknown>;
  return (
    (v.upper_bound_ms === null || typeof v.upper_bound_ms === "number") &&
    typeof v.count === "number"
  );
}

function isToolRuntimeStats(value: unknown): value is ToolRuntimeStats {
  if (typeof value !== "object" || value === null) return false;
  const v = value as Record<string, unknown>;
  const requiredOk =
    typeof v.tool === "string" &&
    typeof v.call_count === "number" &&
    typeof v.failure_count === "number" &&
    typeof v.total_duration_ms === "number" &&
    typeof v.avg_duration_ms === "number";
  if (!requiredOk) return false;
  // M12+ optional fields — defaulted to 0 / [] when absent so older
  // server snapshots still parse cleanly.
  defaultMissingNumber(v, "min_duration_ms");
  defaultMissingNumber(v, "max_duration_ms");
  defaultMissingNumber(v, "p50_duration_ms");
  defaultMissingNumber(v, "p95_duration_ms");
  defaultMissingNumber(v, "p99_duration_ms");
  if (v.duration_histogram === undefined) {
    (v as { duration_histogram?: HistogramBucket[] }).duration_histogram = [];
    return true;
  }
  if (!Array.isArray(v.duration_histogram)) return false;
  return v.duration_histogram.every(isHistogramBucket);
}

function defaultMissingNumber(v: Record<string, unknown>, key: string) {
  if (v[key] === undefined) {
    v[key] = 0;
  }
}

export function isAgentRuntimeSnapshot(
  value: unknown,
): value is AgentRuntimeSnapshot {
  if (typeof value !== "object" || value === null) return false;
  const v = value as Record<string, unknown>;
  const requiredOk =
    typeof v.agent_id === "string" &&
    typeof v.window_seconds === "number" &&
    typeof v.bucket_window_seconds === "number" &&
    typeof v.bucket_count === "number" &&
    typeof v.inference_count === "number" &&
    typeof v.error_count === "number" &&
    typeof v.input_tokens === "number" &&
    typeof v.output_tokens === "number" &&
    typeof v.avg_inference_duration_ms === "number" &&
    typeof v.p50_inference_duration_ms === "number" &&
    typeof v.p95_inference_duration_ms === "number" &&
    typeof v.suspensions === "number" &&
    typeof v.handoffs === "number" &&
    typeof v.delegations === "number" &&
    Array.isArray(v.tool_calls_by_tool);
  if (!requiredOk) return false;
  // M12+ optional snapshot fields.
  defaultMissingNumber(v, "min_inference_duration_ms");
  defaultMissingNumber(v, "max_inference_duration_ms");
  defaultMissingNumber(v, "p99_inference_duration_ms");
  if (v.inference_duration_histogram === undefined) {
    (v as { inference_duration_histogram?: HistogramBucket[] })
      .inference_duration_histogram = [];
  } else if (!Array.isArray(v.inference_duration_histogram)) {
    return false;
  } else if (!v.inference_duration_histogram.every(isHistogramBucket)) {
    return false;
  }
  // tool_calls_by_tool entries — recurse with normalisation.
  return (v.tool_calls_by_tool as unknown[]).every(isToolRuntimeStats);
}

// ── Display helpers ─────────────────────────────────────────────────

/// Pretty-print a window length in seconds as "Nh", "Nm", or "Ns".
export function formatWindow(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds <= 0) return "0s";
  if (seconds % 3600 === 0) return `${seconds / 3600}h`;
  if (seconds % 60 === 0) return `${seconds / 60}m`;
  return `${seconds}s`;
}

/// Compute an error rate in `[0, 1]` from an [`AgentRuntimeSnapshot`].
/// Returns 0 when the snapshot has no inferences.
export function errorRate(snapshot: AgentRuntimeSnapshot): number {
  if (snapshot.inference_count === 0) return 0;
  return snapshot.error_count / snapshot.inference_count;
}

/// Compute an aggregate tool failure rate.
export function toolFailureRate(snapshot: AgentRuntimeSnapshot): number {
  const totals = snapshot.tool_calls_by_tool.reduce(
    (acc, t) => {
      acc.calls += t.call_count;
      acc.fails += t.failure_count;
      return acc;
    },
    { calls: 0, fails: 0 },
  );
  if (totals.calls === 0) return 0;
  return totals.fails / totals.calls;
}

/// Pretty-print a histogram bucket boundary as a label like "≤100 ms" or "> 10000 ms".
export function formatHistogramLabel(bucket: HistogramBucket): string {
  if (bucket.upper_bound_ms === null) {
    return "> 10000 ms"; // matches DEFAULT_DURATION_BUCKETS_MS top
  }
  return `≤${bucket.upper_bound_ms} ms`;
}

/// Return the maximum count across the buckets, or 0 when empty.
/// Used by the bar-chart UI to size each row's fill width.
export function maxHistogramCount(buckets: HistogramBucket[]): number {
  let m = 0;
  for (const b of buckets) {
    if (b.count > m) m = b.count;
  }
  return m;
}

async function safeText(resp: Response): Promise<string> {
  try {
    return await resp.text();
  } catch {
    return "<unreadable response body>";
  }
}
