import { BACKEND_URL, ConfigApiError, fetchJson } from "./http";

// ── DatasetSpec / Fixture wire shapes ──────────────────────────────────
// Only the fields the admin console reads or sends. The backend's
// `awaken_eval` types carry more (provider_script_error,
// continued_turns, mock_response, etc.) — keep this minimal so we don't
// accidentally constrain UI features by forgetting to thread a field
// through. Unknown keys round-trip via index signature on records.

export interface Fixture {
  id: string;
  description?: string;
  user_input: string;
  /** Captured upstream events. Empty when the fixture is Live-only. */
  provider_script?: unknown[];
  /** Why provider_script is absent (Live-only). Set by trace curation. */
  provider_script_error?: string;
  source_run_id?: string;
  source_model_id?: string;
  expect: Expectation;
  continued_turns?: unknown[];
}

/** Operator-authored pass/fail criteria. The backend exposes more
 *  fields (substring lists, judge thresholds, schema checks); the UI
 *  passes the value through verbatim. `is_empty` on the Rust side
 *  rejects a fixture with zero criteria — the UI must populate at
 *  least one before submit. */
export interface Expectation {
  /** Substrings that must appear in the assistant's final answer. */
  final_answer_contains?: string[];
  /** Substrings that must NOT appear in the assistant's final answer. */
  final_answer_excludes?: string[];
  /** Tool names the agent must invoke. */
  tool_sequence?: string[];
  /** Minimum LLM-as-judge score (0..1). Requires a judge model on run. */
  min_judge_score?: number;
  [key: string]: unknown;
}

export interface DatasetSpec {
  description?: string;
  fixtures: Fixture[];
}

export interface ConfigRecord<T> {
  spec: T;
  meta: { revision: number; created_at?: number; updated_at?: number; source?: unknown };
}

export interface DatasetSummary {
  id: string;
  description: string;
  fixture_count: number;
  revision: number;
}

// ── EvalRun wire shapes ────────────────────────────────────────────────

export type EvalRunExecutionMode = "scripted" | "live";

export interface EvalRunSummary {
  id: string;
  dataset_id: string;
  dataset_revision?: number;
  execution_mode?: EvalRunExecutionMode;
  started_at_secs: number;
  ended_at_secs?: number;
  item_count: number;
  passed_count: number;
  /** `report.passed === false` count. Today equal to
   *  `item_count - passed_count`; the explicit field exists so the
   *  list page doesn't have to assume that invariant (partial-run
   *  schemas would produce `item_count - passed_count - failed_count`
   *  pending items, which the UI now treats separately). */
  failed_count?: number;
  [key: string]: unknown;
}

export interface EvalRunItem {
  fixture_id: string;
  report?: {
    passed?: boolean;
    failures?: unknown[];
    final_text?: string;
    [key: string]: unknown;
  };
  trace_run_id?: string;
  [key: string]: unknown;
}

/** Detailed eval run. `items` carries per-fixture reports; aggregate
 *  counts are derived in the UI rather than promised by the wire. */
export interface EvalRun {
  id: string;
  dataset_id: string;
  dataset_revision?: number;
  execution_mode?: EvalRunExecutionMode;
  started_at_secs: number;
  ended_at_secs?: number;
  items?: EvalRunItem[];
  [key: string]: unknown;
}

export interface EvalRunResponse {
  run: EvalRun;
  diff?: unknown;
  aggregates?: unknown;
}

// ── Online eval (model test) ──────────────────────────────────────────

export interface OnlineEvalRequest {
  user_input: string;
  models: string[];
  agent_id?: string;
  expectations?: Expectation;
  persist?: boolean;
  max_walltime_secs?: number;
  max_total_tokens?: number;
}

// ── API client ────────────────────────────────────────────────────────

/** Only collapse "route absent / feature disabled" to `null`. 503 means
 *  the route exists but the backing store is unreachable — let that
 *  propagate so detail pages can render a *store unreachable* error
 *  instead of a misleading "feature disabled" notice. Previously this
 *  collapsed both 404 *and* 503; operators saw "Eval disabled" when the
 *  real problem was a wiring/disk issue, and went hunting for a flag
 *  that didn't exist. */
function nullOnEvalDisabled<T>(promise: Promise<T>): Promise<T | null> {
  return promise.catch((err) => {
    if (classifyEvalError(err) === "disabled") {
      return null;
    }
    throw err;
  });
}

/** Categorize an eval-API error. The server returns 404 for both
 *  "route absent" (eval surface disabled) and "id not found" — they
 *  can't be distinguished from HTTP status alone, so detail pages pass
 *  `listAvailable` from the cached parent list query:
 *
 *    - `listAvailable: true`  → the list endpoint resolved, so 404 on a
 *      detail/mutation path means the *id* is missing, not the feature.
 *    - `listAvailable: false` → the list endpoint returned null
 *      (eval disabled) — propagate the disabled story consistently.
 *    - `listAvailable: "unknown"` → no parent query available; fall
 *      back to "disabled" as the safer assumption (operator gets a
 *      hint about enabling the feature instead of a generic error).
 *
 *  503 always means service-unavailable (config store missing, eval
 *  run store unwired, transient downstream issue) — separated so
 *  operators know to check the runtime, not their flags.
 */
export type EvalErrorCategory = "disabled" | "not_found" | "store_error" | "other";

export function classifyEvalError(
  err: unknown,
  ctx: { listAvailable: boolean | "unknown" } = { listAvailable: "unknown" },
): EvalErrorCategory {
  if (!(err instanceof ConfigApiError)) return "other";
  if (err.status === 503) return "store_error";
  if (err.status === 404) {
    return ctx.listAvailable === true ? "not_found" : "disabled";
  }
  return "other";
}

export const evalApi = {
  listDatasets: (): Promise<{ datasets: DatasetSummary[] } | null> =>
    nullOnEvalDisabled(
      fetchJson<{ datasets: DatasetSummary[] }>(`${BACKEND_URL}/v1/eval/datasets`),
    ),

  getDataset: (id: string): Promise<ConfigRecord<DatasetSpec>> =>
    fetchJson<ConfigRecord<DatasetSpec>>(
      `${BACKEND_URL}/v1/eval/datasets/${encodeURIComponent(id)}`,
    ),

  createDataset: (id: string, spec: DatasetSpec): Promise<ConfigRecord<DatasetSpec>> =>
    fetchJson<ConfigRecord<DatasetSpec>>(`${BACKEND_URL}/v1/eval/datasets`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ id, spec }),
    }),

  putDataset: (
    id: string,
    expectedRevision: number,
    spec: DatasetSpec,
  ): Promise<ConfigRecord<DatasetSpec>> =>
    fetchJson<ConfigRecord<DatasetSpec>>(
      `${BACKEND_URL}/v1/eval/datasets/${encodeURIComponent(id)}`,
      {
        method: "PUT",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ expected_revision: expectedRevision, spec }),
      },
    ),

  /** Delete a dataset. Pass `expectedRevision` to make it a
   *  compare-and-swap: the server only deletes when the dataset's
   *  current revision matches, returning 409 otherwise. The trace →
   *  fixture rollback uses this so it can't wipe a dataset a concurrent
   *  operator wrote to between create and a failed curate. */
  deleteDataset: (id: string, expectedRevision?: number): Promise<void> => {
    const qs =
      expectedRevision === undefined ? "" : `?expected_revision=${expectedRevision}`;
    return fetchJson<void>(
      `${BACKEND_URL}/v1/eval/datasets/${encodeURIComponent(id)}${qs}`,
      { method: "DELETE" },
    );
  },

  /** Curate one trace into a fixture and append to the dataset. */
  curateItems: (
    datasetId: string,
    body: {
      from_run_id: string;
      fixture_id?: string;
      description?: string;
      user_input?: string;
      provider_script_mode?: "optional" | "require" | "skip";
      allow_unused_provider_script?: boolean;
      expect: Expectation;
    },
  ): Promise<ConfigRecord<DatasetSpec>> =>
    fetchJson<ConfigRecord<DatasetSpec>>(
      `${BACKEND_URL}/v1/eval/datasets/${encodeURIComponent(datasetId)}/items`,
      {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(body),
      },
    ),

  listRuns: (params: { dataset_id?: string; limit?: number } = {}): Promise<{
    runs: EvalRunSummary[];
  } | null> => {
    const sp = new URLSearchParams();
    if (params.dataset_id) sp.set("dataset_id", params.dataset_id);
    if (params.limit !== undefined) sp.set("limit", String(params.limit));
    const qs = sp.toString();
    return nullOnEvalDisabled(
      fetchJson<{ runs: EvalRunSummary[] }>(
        `${BACKEND_URL}/v1/eval/runs${qs ? `?${qs}` : ""}`,
      ),
    );
  },

  getRun: (id: string, opts: { baseline?: string } = {}): Promise<EvalRunResponse> => {
    const sp = new URLSearchParams();
    if (opts.baseline) sp.set("baseline", opts.baseline);
    const qs = sp.toString();
    return fetchJson<EvalRunResponse>(
      `${BACKEND_URL}/v1/eval/runs/${encodeURIComponent(id)}${qs ? `?${qs}` : ""}`,
    );
  },

  startRun: (body: {
    dataset_id: string;
    mode?: EvalRunExecutionMode;
    models?: string[];
    agent_id?: string;
    baseline_run_id?: string;
    max_walltime_secs?: number;
    max_total_tokens?: number;
  }): Promise<EvalRunResponse> =>
    fetchJson<EvalRunResponse>(`${BACKEND_URL}/v1/eval/runs`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    }),

  /** One-shot ad-hoc eval against a single user_input, no dataset.
   *  Used by the model test panel. */
  online: (body: OnlineEvalRequest): Promise<EvalRunResponse> =>
    fetchJson<EvalRunResponse>(`${BACKEND_URL}/v1/eval/online`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    }),
};
