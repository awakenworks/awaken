# ADR-0032: Unified Eval Execution

- **Status**: 📐 Proposed
- **Date**: 2026-05-10
- **Supersedes**: ADR-0027
- **Depends on**: ADR-0010, ADR-0014, ADR-0023, ADR-0030, ADR-0031

## Context

ADR-0027 framed server-side eval as a **report-ingestion** problem: the
CLI runs the evaluation locally and pushes NDJSON to
`POST /v1/eval/reports`. That model preserved the offline-first
property of `awaken-eval` but bequeathed two harder problems:

1. **Synthetic spans.** `crates/awaken-eval` today drives evaluation
   through `MockReplayer` (`replay.rs:44–137`) which constructs
   `GenAISpan` instances directly from string inputs
   (`replay.rs:83–117, 152–156`) and approximates tokens from
   `s.len() / 4`. The numbers in `baseline.ndjson` and the spans the
   admin console renders bear no causal relationship to what
   `awaken-runtime` would produce on the same input. CI gates pass on
   data that is structurally valid but semantically detached from
   production.

2. **No path from production traces to eval datasets.** Online
   regressions cannot be replayed offline because eval consumes its
   own fixture format, not the trace format that observability
   produces. Operators transcribe failures by hand or accept that some
   regressions are simply not reproducible in CI.

ADR-0030 introduces a queryable trace store with content-addressed
attribution. ADR-0031 introduces routed variants whose performance is
visible on the same traces. With both in place, the right model for
eval is **execution**, not ingestion: the server runs replays through
the real `awaken-runtime` against a deterministic provider, the result
is just another row in the trace store, and reports are a view over
those rows. ADR-0027's `POST /v1/eval/reports` ingestion endpoint is
no longer the right surface and is superseded.

The offline-first constraint from ADR-0027 is preserved as a hard
requirement of this ADR: developers running on a laptop without a
remote server still get an interactive eval loop. The mechanism
changes (embedded server + HTTP loopback) but the user-facing
property does not.

## Non-Goals

- Recording live LLM responses for later replay. Deterministic replay
  here uses an in-process `MockProvider` driven by a script embedded
  in the dataset item. Real-provider record/replay is a future ADR.
- Auto-promote of an eval-passing candidate into production. The
  admin console surfaces a manual "ship this variant" action that
  goes through ADR-0031's `:ship` endpoint.
- Multi-judge composition (consensus across multiple LLM judges).
  Only `TensorZeroJudge` is supported, gated at runtime by the
  presence of judge configuration on the eval-run payload.
- A new eval dataset format that supersedes the existing fixture
  files. The current fixture schema evolves additively; old fixtures
  keep loading through a migration shim and are cleaned up in a
  follow-up.

## Decisions

### D1: Eval is Server-Executed, Not Server-Ingested

`POST /v1/eval/reports` from ADR-0027 is **not implemented**. In its
place, `awaken-server` exposes `POST /v1/eval/runs`:

```
POST /v1/eval/runs
{
  "dataset_id": "regressions",
  "mode": "scripted" | "live",
  "models": ["claude-sonnet"],        // required for live, invalid for scripted
  "agent_id": "weather",              // optional, live only
  "agent_overrides": { ... },          // optional, live only
  "judge": { "model_id": "judge" },    // optional, live only
  "max_walltime_secs": 60              // optional, live only
}
```

The handler:

1. Loads the dataset and chooses an explicit execution mode. `scripted`
   treats each fixture's `provider_script` as a deterministic
   `LlmExecutor`; `live` ignores `provider_script` and resolves real
   provider executors from the request's `models` axis. For backward
   compatibility, omitted `mode` is inferred from `models`, but clients
   should send `mode` explicitly.
2. Submits each `user_input` through the engine and lets the
   `awaken-ext-observability` hooks emit spans naturally. Spans land
   in `TraceStore` (ADR-0030) tagged with the `awaken.replay = true`
   resource attribute.
3. Reads `AgentMetrics` back from the `InMemorySink` after each run
   completes and combines it with the trace into a `ReplayOutcome`.
4. Scores via `awaken-eval::score` and, if `judge` is present,
   invokes `TensorZeroJudge`.
5. Persists a thin `EvalRun` index document with `execution_mode`,
   trace `run_id` links, and the per-fixture pass / fail / failures
   vector. Baseline diffs reject runs whose `execution_mode` differs.
6. Returns the `EvalRun` id; full report is fetched via `GET
   /v1/eval/runs/:id`.

This makes a replayed evaluation indistinguishable from any other run
at the trace layer. The eval surface is a thin orchestrator + index
on top of the existing infrastructure.

### D2: `RuntimeReplayer` Replaces `MockReplayer`

`crates/awaken-eval/src/replay.rs` is restructured:

- `MockReplayer`'s direct span synthesis (`replay.rs:83–117, 152–156`)
  is deleted.
- A new `RuntimeReplayer` implements the existing `Replayer` trait by
  building an `awaken-runtime::Engine`, registering a `MockProvider`
  for the duration of the replay, and submitting the fixture's input.
- The `Replayer` trait stays. CLI and server depend on the trait so a
  future `RecordedProviderReplayer` can slot in without API churn.
- Token counts and finish reasons come from the `provider_script`
  events directly. The string-length heuristic is gone.

### D3: `MockProvider` Lives in `awaken-runtime`

A new provider implementation under
`crates/awaken-runtime/src/providers/mock.rs`:

```rust
pub struct MockProvider {
    script: VecDeque<ProviderScriptEvent>,
}

pub enum ProviderScriptEvent {
    ChatResponse {
        content: String,
        tokens: TokenCounts,
        finish_reason: FinishReason,
        // optional fields populated when present:
        response_model: Option<String>,
        response_id: Option<String>,
    },
    ToolCall {
        tool: String,
        arguments: Value,
    },
    // … one variant per upstream behaviour the runtime needs to model
}
```

The provider is registered via the standard `ProviderRegistry`
(ADR-0010 D2) but is gated by an `AWAKEN_ENABLE_MOCK_PROVIDER` env
var so production deployments cannot accidentally route real traffic
through it. The eval service flips the var on for its own process
when it spins up its `Engine`.

The provider's responses produce real `GenAISpan` records via the
same hooks any other provider goes through. The numbers on the trace
are exactly the numbers in the script — no heuristics, no drift.

### D4: `provider_script` in Dataset Items

`crates/awaken-eval/fixtures/*.json` evolves additively. Existing
fields stay. A new `provider_script` array is preferred when scripted
replay is available:

```jsonc
{
  "id": "01_simple_qa",
  "user_input": "What's the weather?",
  "provider_script": [
    {
      "kind": "chat_response",
      "content": "...",
      "tokens": { "input": 12, "output": 8 },
      "finish_reason": "stop"
    },
    {
      "kind": "tool_call",
      "tool": "weather.get",
      "arguments": { "city": "..." }
    }
  ],
  "provider_script_error": "parallel tool calls are not representable",
  "source_run_id": "01HXYZ...",         // optional, when curated from a trace
  "source_model_id": "claude-opus-4-7", // optional, mismatch guard
  "expect": { "final_answer_contains": [...], "tool_sequence": [...] },

  "mock_response": "..."  // legacy, superseded; loader synthesises a
                          // single-element provider_script when only
                          // this is present
}
```

`provider_script_error` is present only for Live-only curated fixtures:
the trace supplied a useful `user_input` / expectation seed, but today's
`ProviderScriptEvent` schema cannot replay the provider turn
losslessly. Scripted replay fails closed for such fixtures instead of
falling through to the legacy empty `mock_response` shim.

`Fixture::load` synthesises `provider_script` from `mock_response` when
needed and no `provider_script_error` is present, so existing committed
fixtures keep loading. After one cycle of validation the legacy field is
removed.

### D5: Dataset Curation From Traces

`POST /v1/eval/datasets/:id/items` accepts:

```
{
  "from_run_id": "01HXYZ...",
  "provider_script_mode": "optional" | "require" | "skip",
  "expected": { "final_answer_contains": [...], ... }
}
```

The handler reads the trace from `TraceStore::read` (ADR-0030 D3),
recovers fixture source metadata (`user_input`, `source_model_id`) from
captured `GenAISpan::request_messages`, and then handles
`provider_script_mode`:

- `optional` (default): transcribe a `provider_script` when possible;
  otherwise write a Live-only fixture with `provider_script_error`.
- `require`: unsupported traces fail the request (or are skipped by
  bulk import when `skip_uncuratable=true`).
- `skip`: do not attempt scripted conversion; write a Live-only fixture.

The dataset item is written with `source_run_id` set to the originating
`run_id` and `source_model_id` set to the recovered source model. The
trace is marked referenced (`mark_referenced`) so retention does not
delete it.

The expected payload is operator-supplied — it is the human judgement
of "what should this run have produced". The handler never auto-derives
expectations from the trace; that would defeat the point of curation.

`POST /v1/eval/datasets/:id/items` is the only path through which
production observations enter the eval system. Every other dataset
mutation (rename, reorder, edit expectations) goes through standard
`PATCH` operations on the dataset record.

Bulk import endpoints (`/import-traces`, `/import-dialogue`) use the
same `provider_script_mode` semantics so single-trace and batched
curation cannot drift.

### D6: Dataset is a `ConfigRecord` Kind

Datasets sit alongside agents, tools, providers, and experiments as
operator-managed config:

| Method   | Path                                        | Purpose                       |
|----------|---------------------------------------------|-------------------------------|
| `GET`    | `/v1/eval/datasets`                         | List                          |
| `GET`    | `/v1/eval/datasets/:id`                     | Full dataset                  |
| `POST`   | `/v1/eval/datasets`                         | Create (empty)                |
| `PATCH`  | `/v1/eval/datasets/:id`                     | Update metadata               |
| `DELETE` | `/v1/eval/datasets/:id`                     | Hard delete (no items in use) |
| `POST`   | `/v1/eval/datasets/:id/items`               | Add item (curate from trace) |
| `PATCH`  | `/v1/eval/datasets/:id/items/:item_id`      | Edit expectations / script    |
| `DELETE` | `/v1/eval/datasets/:id/items/:item_id`      | Remove item                   |

Audit, restore, version-switching, and seeding inherit from the
existing `ConfigRecord` machinery (ADR-0028, ADR-0029).

### D7: Eval Run Index and Diff

`EvalRun`:

```rust
pub struct EvalRun {
    pub id: String,                       // ULID
    pub dataset_id: String,
    pub dataset_revision: u64,
    pub execution_mode: EvalRunExecutionMode,
    pub items: Vec<EvalRunItem>,
    pub started_at_secs: u64,
    pub ended_at_secs: u64,
}

pub struct EvalRunItem {
    pub fixture_id: String,
    pub cell: Option<MatrixCell>,
    pub report: ReplayReport,
    pub trace_run_id: Option<String>,     // points at TraceStore
    pub sample_index: Option<u32>,
}
```

`GET /v1/eval/runs/:id?baseline=:baseline_id` returns the run plus a
diff computed by the existing `report.rs::diff_against_baseline`
function. Regression / fixed / still-failing / missing / newly-added
classifications are unchanged.

### D8: CLI as Server Client With Embedded Fallback

`awaken-eval` CLI is rewritten:

- Default mode: HTTP client of an `awaken-server`. `--server <url>`
  selects the target. Authentication uses the same admin bearer
  token mechanism as ADR-0023.
- Embedded mode: when `--server` is omitted **and**
  `--offline` is not set, the CLI starts an in-process
  `awaken-server` on a random local port, connects to it via HTTP
  loopback, runs the requested operations, and tears down the server
  on exit. This guarantees both code paths share the same
  implementation; the server is the single source of truth for
  replay logic.
- `--offline` mode preserves a developer-only fast path that calls
  the eval pipeline in-process without HTTP. It is intended for tight
  TDD loops and is documented as a developer convenience, not a
  production interface. The output format matches the server's
  `EvalRun` shape so toggling between modes does not break tooling.
- The legacy `awaken-eval check --baseline ... --new ...` flow keeps
  working: in HTTP mode it calls
  `GET /v1/eval/runs/:id?baseline=:baseline_id`; offline it runs the
  diff directly.

### D9: Judge Becomes Runtime-Configured

The `llm-judge` Cargo feature flag is removed from
`crates/awaken-eval/Cargo.toml`. `TensorZeroJudge` construction is
gated at runtime by the presence of judge configuration on the
eval-run payload. Builds always include the judge code; activation
is by configuration. This eliminates the previous mismatch where
the admin console could expose judge UI but the underlying CLI
build might lack the feature.

Judge results are written into the trace as
`EvaluationResultEvent` (already supported by the observability
plugin) and indexed on `EvalRunItem`.

### D10: Schema Compatibility With ADR-0030

The eval service relies on ADR-0030's `awaken.replay = true`
resource attribute to keep replay traces out of production
aggregates in Phoenix dashboards. It also uses the
`source_model_id` mismatch guard: replaying a script captured
against `claude-opus-4-7` against a different model id fails fast
unless the request body includes `"allow_model_mismatch": true`.

### D11: Authentication

All `/v1/eval/*` routes are gated by `ensure_admin_auth` (ADR-0023).
A new `AdminApiConfig.expose_eval_routes` boolean (default `true`)
follows the established pattern. CI environments running eval against
a production server use the same bearer token mechanism; a future ADR
may introduce a narrower eval-runner credential scope.

### D12: Distributed Deployment

Inherits the constraints from ADR-0029 D11 / ADR-0031 D8. Eval runs
are not partitioned across replicas; a single `POST /v1/eval/runs`
executes on the receiving replica. Concurrent runs of the same
dataset against the same override are tolerated and produce
independent `EvalRun` records (no deduplication). Operators running
eval at scale either route eval traffic through a dedicated replica
(via L7 routing) or spread runs across replicas explicitly through
their orchestration layer.

## Consequences

- The MockReplayer / runtime split is gone. The same code paths run
  production traffic, A/B candidates (ADR-0031), and offline eval.
- `baseline.ndjson` numbers shift: tokens come from
  `provider_script` instead of string-length heuristics. The
  baseline is regenerated through the new path during the cutover.
  A `replay_baseline_compat` integration suite documents the
  per-field tolerance rules used during the transition.
- Eval reports become a view over `TraceStore`. The admin console's
  drag-and-drop NDJSON parser remains as an offline-only
  affordance; primary data flow is `GET /v1/eval/runs`.
- Production traces feed eval datasets directly. A regression observed
  in production is one click away from being a regression fixture.
- The CLI gains a runtime dependency on the server crate (for
  embedded mode). This is acceptable: `awaken-eval` is already an
  internal binary that ships with the rest of the workspace, and
  embedded mode is the only way to keep "developer laptop without
  external server" workflows on the same code path as production.
- ADR-0027's offline-first constraint is preserved by the embedded
  fallback. Existing scripts that run `awaken-eval replay` without
  a server keep working; their output now traverses the same logic
  the server uses.

## Alternatives Considered

**Keep `POST /v1/eval/reports` from ADR-0027 alongside `POST
/v1/eval/runs`.** Rejected: maintaining two ingestion paths means
two definitions of "what an eval run is" and two pieces of code to
keep in sync. The CLI becomes a thin client of the server-side
executor instead.

**Run replay in-process in the CLI; have the server only persist
results.** Rejected: this perpetuates the dual-track problem ADR-0030
exists to solve. As soon as the server runs the same code, all
replays look like regular traces, so persisting results separately
becomes redundant.

**Skip `MockProvider`; use a `mock` adapter inside the existing
genai stack.** Rejected: the genai stack is the live LLM client.
Adding a mock adapter there couples test scaffolding to production
client code. A first-class `MockProvider` is cleaner and the env
var gate ensures it cannot be used in production.

**Treat eval datasets as flat NDJSON files committed to the repo.**
Rejected: the existing fixture files keep working through D4's
backwards-compatibility shim, but new datasets curated from
production traces need server-side persistence so the admin console
and CI can both refer to them by id.

## Open Questions

- Should `provider_script` allow streaming events
  (delta-by-delta replay) for fidelity with streaming responses?
  Current decision: scripts emit terminal `chat_response` events
  only; streaming-fidelity replay is a future extension when a
  concrete need surfaces.
- Should the eval service support running multiple overrides in
  one request (e.g., evaluate a matrix of `prompt_id` × `tool_id`)?
  Current decision: one override per request; matrix orchestration
  belongs in the caller (CI script or admin UI). The server stays
  simple.
- How are dataset items garbage-collected when a referenced trace
  is pruned by ADR-0030's TTL? Current decision: dataset references
  pin the trace via `mark_referenced`, so prune skips it. A
  dataset item's `provider_script` is fully self-contained anyway,
  so even a hypothetical reference loss does not break the item.
