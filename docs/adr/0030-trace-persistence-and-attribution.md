# ADR-0030: Trace Persistence and Attribution

- **Status**: 📐 Proposed
- **Date**: 2026-05-10
- **Depends on**: ADR-0010, ADR-0014, ADR-0023, ADR-0029

## Context

`crates/awaken-ext-observability` emits structured spans (`GenAISpan`,
`ToolSpan`, `HandoffSpan`, `SuspensionSpan`, `DelegationSpan`,
`BackgroundTaskSpan`) at six lifecycle points (`plugin/hooks.rs:111–519`)
and fans them out to a composite of `InMemorySink`, `OtelMetricsSink` (OTLP
export aligned with GenAI semantic conventions, ADR-aligned with PR #181),
and `PrometheusSink`. An optional `PersistentSink` writes NDJSON to disk
under `AWAKEN_PERSISTENT_SINK_DIR` as a tail-store fallback for failed
exports.

Three problems remain:

1. **No attribution.** `SpanContext` (`metrics.rs:19–37`) carries `run_id`,
   `thread_id`, `agent_id`, `parent_run_id`, `parent_tool_call_id`. It does
   not record which version of the agent's prompt, which tool description,
   or which skill content produced each span. `RegistrySnapshot` exposes a
   single monotonic `version: u64` (`registry/snapshot.rs:11`) for the
   entire registry set; it cannot answer "which prompt rendered this turn?"
   at fine granularity. ADR-0029 makes tool descriptions overridable but
   the override never reaches a span attribute.

2. **No queryable persistence.** `PersistentSink` writes NDJSON shards but
   exposes no read API. `awaken-server` has no service that can return a
   trace by `run_id`. The admin console's only path to trace data today is
   either OTLP export (Phoenix) or no path at all.

3. **No retention discipline.** Without a server-managed store there is no
   sampling policy and no TTL. NDJSON files accumulate indefinitely.

These gaps block ADR-0032 (server-side eval execution): eval needs a
canonical place to read traces from, and replay results need to be tagged
back to the exact prompt / tool versions they ran against. They also block
ADR-0031 (experiment routing), which requires recording variant
assignment on the trace stream.

## Non-Goals

- Replacing OTLP export. OTel attribute mapping is additive — new fields
  surface as `awaken.*` attributes alongside the GenAI spec attributes
  already defined.
- A general-purpose observability database. The store is shaped for
  awaken's run/trace topology (per `run_id` shard) and is not optimised
  for ad-hoc analytics; long-term analytical workloads continue to use
  Phoenix.
- Cross-tenant trace isolation. Permission boundaries follow the existing
  admin-bearer-token model from ADR-0023.

## Decisions

### D1: Content-Addressed Identity in `awaken-contract`

A new `awaken-contract::identity` module exposes derived identity helpers:

```rust
// crates/awaken-contract/src/identity.rs
pub fn agent_prompt_id(agent_id: &str, role: &str, content: &str) -> String;
pub fn tool_desc_id(tool_name: &str, description: &str, schema_json: &str) -> String;
pub fn skill_content_id(skill_name: &str, content: &str) -> String;
```

Each returns the first 12 hex characters of `sha256(...)`. The identity
is:

- **Stable**: equal inputs always yield the same id; no clock or counter
  involvement.
- **Agent-scoped for prompts**: `agent_prompt_id` includes the
  `agent_id` in the hash. Identical prompt **content** under two
  different `agent_id`s deliberately produces different ids — agent
  identity is part of the attribution so the same string copied across
  agents remains distinguishable on the trace stream. Analysts who need
  to cluster traces by prompt content rather than by `(agent, prompt)`
  drop the `agent_id` axis at the query layer.
  - `tool_desc_id` and `skill_content_id` are not agent-scoped: the
    same tool description / skill content produces the same id
    regardless of which agent advertises it.
- **Cheap to carry**: 12 hex characters keeps OTel attribute cardinality
  bounded and allows efficient indexing.

`RegistrySnapshot::replace` / `update` (`registry/snapshot.rs:60–88`)
compute the relevant id for each entry alongside the existing `version`
bump and store them on the resolved entry. The coarse `version: u64` is
retained as a monotonic snapshot cache key for callers that want the
"any change at all" signal.

### D2: `SpanContext` Attribution Fields

`SpanContext` (`crates/awaken-ext-observability/src/metrics.rs:19–37`)
gains six fields:

```rust
pub struct SpanContext {
    // existing fields preserved...
    pub prompt_id: Option<String>,         // agent's effective system prompt
    pub tool_desc_ids: Vec<String>,        // tools advertised at this turn
    pub skill_ids: Vec<String>,            // RESERVED — see note below
    pub release_tag: Option<String>,       // human-readable rollout alias
    pub experiment_id: Option<String>,     // populated by ADR-0031
    pub variant_name: Option<String>,      // populated by ADR-0031
}
```

The hooks fill `prompt_id` from the resolved `AgentSpec` at `RunStart`
(falling back to the registry snapshot when the spec was handed off
without a hydrated prompt — see ADR-0014 D3) and `tool_desc_ids` from
the registry snapshot at `BeforeInference`, then overridden at
`AfterInference` from `PhaseContext.effective_tool_ids` so the GenAI
span records the post-filter tool list the LLM actually saw.
`skill_ids` is a **reserved** schema slot — hooks stamp it as
`Vec::new()` in this delivery. Populating it requires
`RegistrySnapshot::skill_content_id`, which is intentionally deferred
because skills live in `awaken-ext-skills`'s own `SkillRegistry` and
do not flow through `RegistrySet` / `PluginSource` today; the
follow-up that widens that surface is tracked in
`registry/snapshot.rs`. Until then consumers must treat an empty
`awaken.skill_ids` as "not yet implemented", **not** "no skills
participated".

The experiment fields are schema-present from this ADR but stay `None`
until ADR-0031 lands; this keeps the trace schema stable across
deliveries.

OTel attribute mapping is additive and uses the `awaken.*` namespace to
avoid collisions with the GenAI semantic conventions PR #181 standardised:

| Field           | OTel attribute key            |
|-----------------|-------------------------------|
| `prompt_id`     | `awaken.prompt_id`            |
| `tool_desc_ids` | `awaken.tool_desc_ids`        |
| `skill_ids`     | `awaken.skill_ids` (reserved) |
| `release_tag`   | `awaken.release.tag`          |
| `experiment_id` | `awaken.experiment.id`        |
| `variant_name`  | `awaken.experiment.variant`   |

A resource attribute `awaken.replay = true` is set on traces produced by
ADR-0032 replay runs so Phoenix-side dashboards can filter eval traffic
out of production aggregates.

### D3: `TraceStore` Trait

`crates/awaken-ext-observability/src/persistent.rs` is restructured:

```rust
pub trait TraceStore: Send + Sync {
    fn append(&self, run_id: &str, event: TraceEvent) -> Result<()>;
    fn read(&self, run_id: &str) -> Result<Vec<TraceEvent>>;
    fn list(&self, filter: TraceFilter) -> Result<Vec<RunSummary>>;
    fn prune(&self, older_than: SystemTime, except_referenced: &HashSet<String>) -> Result<u64>;
    fn mark_referenced(&self, run_id: &str, by: ReferenceKind) -> Result<()>;
}

pub struct RunSummary {
    pub run_id: String,
    pub agent_id: String,
    pub started_at: SystemTime,
    pub ended_at: Option<SystemTime>,
    pub prompt_ids: Vec<String>,
    pub experiment_id: Option<String>,
    pub variant_name: Option<String>,
    pub final_status: Option<RunStatus>,
    pub judge_score: Option<f32>,
}
```

`PersistentSink` becomes a thin adapter that writes through `TraceStore`.
Existing OTLP and in-memory sinks are unchanged.

### D4: `FileTraceStore` Default Implementation

The default implementation is filesystem-backed and matches the existing
deployment model (a single server process with local disk):

- Per-run NDJSON shards under `{root}/{yyyy-mm}/{run_id}.ndjson`. The
  month bucket bounds directory entry counts. The first append for a
  `run_id` pins the shard month in an in-process `run_dirs` cache so
  subsequent appends and the matching `.idx.json` always land next to
  one another, even when a run crosses midnight at month-end.
- Per-run index file `{root}/{yyyy-mm}/{run_id}.idx.json` recording the
  fields surfaced by `RunSummary`. The index is written via the
  `write_index_for_run` trait method at `RunEnd` (after the sampling
  gate, so it is omitted for sampled-out or buffer-overflowed runs).
  Judge-result events trigger no separate index rewrite; the score is
  captured by the same `RunEnd` path because evaluations are recorded
  on the `AgentMetrics` snapshot the hook sees.
- `list(filter)` performs a scan of `{root}/*/*.idx.json` and applies
  the requested filters in memory. There is no separate `by_agent`
  index tree or sqlite catalogue today — the working set is bounded
  by the TTL prune, and adding a secondary index is left for the
  point at which scan latency becomes the bottleneck.
- Appends use `OpenOptions::append(true)` and write one newline-
  terminated JSON record per event. Readers (`read`, the iterator
  driving `list`) parse strictly: a malformed line surfaces an error
  rather than being silently skipped, so partial-write damage is
  visible to operators instead of being masked.
- Reference-counting via `mark_referenced` writes a sentinel file
  `{run_id}.ref` so `prune` can skip referenced runs cheaply without
  loading every index file. `prune` derives a run's age from the
  shard's mtime when the index is unreadable, so a corrupted index
  cannot pin a run forever.

A future SQLite-backed implementation may be slotted in behind the same
trait when query patterns demand it; this ADR does not pre-commit to it.

### D5: Sampling Policy

`SamplingPolicy` is a per-run decision evaluated at `RunEnd`:

```rust
pub struct SamplingPolicy {
    pub error_traces: SamplingMode,         // default: Always
    pub low_judge_score: SamplingMode,      // default: Always
    pub explicit_flag: SamplingMode,        // default: Always (reserved)
    pub normal_traces: SamplingMode,        // default: Proportional(0.01)
}

pub enum SamplingMode { Always, Never, Proportional(f32) }
```

Every span is buffered in-process under its `run_id` until `RunEnd`,
at which point the policy is evaluated and the buffer is either
flushed to `TraceStore` or dropped. This avoids losing the head of a
run that turns out to be interesting only at the end (e.g., a judge
score that arrives at the final step). To bound memory for runaway
or never-ending runs, each per-run buffer has a hard event cap; once
exceeded the run is marked `Overflowed` and dropped at `RunEnd` with
a warning. An **overflowed run is dropped unconditionally** — its
final outcome (error, low judge score, explicit flag) is **not**
honoured because the head of the buffer has already been discarded
and persisting a tail fragment would mis-attribute the run on the
index. Every overflow transition emits a `tracing::warn!` event, and
embedders that hold a typed reference to `PersistentSink` can also
read the lifetime count via `PersistentSink::overflow_count()` — the
counter is intentionally not surfaced on `WiringSummary` to avoid
widening the wiring API for a diagnostic that already shows up in
logs. No periodic mid-run flush exists today; adding one is tracked
as a follow-up if real workloads start hitting the overflow cap.

`run_had_error(metrics)` is the single source of truth for "this run
failed at the run level" — it spans inference, tool, background-task,
evaluation, and delegation errors. Both the sampling gate and the
`RunSummary.final_status` derivation read it, so the policy and the
list endpoint always agree on which runs are errored.

The proportional decision is deterministic per run: a 64-bit FNV-1a
hash of the `run_id` produces the same keep/drop verdict on every
evaluation, so a `cargo update` of the toolchain cannot silently
re-bucket the fleet.

`explicit_flag` is reserved schema: the policy slot, the
`RunOutcome.explicit_flag` field, and the routing through
`should_persist` are all wired so that an operator-flagged run
(HITL reject, thumbs-down) can be force-kept once a producer
exists. No call site sets the flag in this delivery; the field is
hardcoded `false` at the `RunEnd` evaluation point. Errors and
low judge scores already cover the common promotion cases that
keep the default policy useful without the flag.

The sampling policy is attached once at startup through
`WiringSettings.sampling_disabled` (env: `AWAKEN_TRACE_SAMPLING_DISABLE=1`
opts out entirely) and `WiringHandle::with_sampling_policy` for callers
that wire the observability extension programmatically. Live reload of
the policy without a server restart is not implemented; introducing it
would require a config-watch path through `AppState` and is deferred
until operations demands it.

### D6: Retention

Default TTL: 7 days for unreferenced traces, retained indefinitely for
referenced traces.

A trace becomes referenced when:

1. A dataset item references it via `from_run_id` (ADR-0032).
2. An experiment guardrail report retains it as evidence (ADR-0031).
3. An operator pins it via the admin console.

`prune` runs on a `/loop` cadence (default daily) and skips runs in the
referenced set. Pruning is a best-effort, eventually-consistent cleanup;
a missed run on one cycle is reaped on the next.

### D7: Server Query API

`crates/awaken-server/src/services/trace_service.rs` (new) exposes:

| Method | Path                         | Response                              |
|--------|------------------------------|---------------------------------------|
| `GET`  | `/v1/traces`                 | Paginated `RunSummary` list           |
| `GET`  | `/v1/traces/:run_id`         | One response page of NDJSON events    |
| `POST` | `/v1/traces/:run_id/pin`     | Marks the run referenced              |

All routes are gated by `ensure_admin_auth` (ADR-0023). A new
`AdminApiConfig.expose_trace_routes` boolean (default `false`) follows the
same pattern as `expose_config_routes`. The default is opt-in because
the trace surface exposes prompt content, tool arguments, and tool
results — strictly more sensitive than the existing admin metadata
routes. A non-loopback bind without a bearer token still fails startup
through `validate_admin_surface`.

The list query supports filters by `agent_id`, `prompt_id`,
`experiment_id`, `variant_name`, and `since`. `since` is RFC 3339
and rejected with `400` if unparseable; `limit` is clamped to at least
`1` and similarly rejected on `0`. The filter set mirrors the attribution
fields from D2 so any field that can be tagged on a trace can be queried;
a `final_status` filter is surfaced on the `RunSummary` body but not yet
on the query string — the storage layer can serve it once a real
consumer asks for it.

`GET /v1/traces/:run_id` paginates the **response** only: the full
NDJSON shard is read from disk per request and a `?offset=&limit=`
window is sliced out before serialising. The `x-trace-next-offset`
and `x-trace-total-events` response headers make resumable consumption
mechanical without exposing pagination cursors. Storage-level
paginated reads (so the server does not re-read the whole shard for
every page) require a `TraceStore::read_page` method on the trait and
are deferred to the first deployment where the full-read cost
becomes measurable; until then this endpoint is intended for
human-driven admin inspection, not high-throughput streaming.

### D8: Backfill of GenAI Span Fields

While extending span data, this ADR also closes the gaps left by PR #181
in `hooks.rs:199–201`:

- `finish_reasons` populated from `StreamResult::stop_reason` via
  `stop_reason_to_finish_reason`. The single GenAI wire string for
  `EndTurn`/`MaxTokens`/`ToolUse`/`StopSequence` is recorded on the
  `GenAISpan` and reflected onto `gen_ai.response.finish_reasons` on
  the tracing span.
- `response_model` and `response_id` are **not** populated in this
  delivery: `StreamCollector::finish` discards the upstream model id
  and response id, so the values are not reachable from the hook
  today. Populating them requires extending
  `awaken_contract::contract::inference::StreamResult` to surface
  both; the hook already records the tracing-span fields when the
  span carries them, so once the contract change lands the
  end-to-end path lights up without further hook work. Tracked as the
  next D8 step.

These are GenAI semantic-convention attributes that were left as `None`
or empty `Vec` in the initial alignment and are required for downstream
analysis (e.g., distinguishing model auto-routing decisions, deduping by
upstream id, attributing context-overflow truncations).

### D9: Trace Schema Versioning

The trace event schema is not yet stamped with an explicit
`schema_version` on the wire. The persisted NDJSON record format relies
on serde's "ignore unknown fields" + optional-field defaults to remain
forward-compatible across additive changes (the D2 attribution fields
landed under that contract). An explicit `schema_version: u16` is
deferred until the first breaking schema change actually requires
reader-side branching — at that point the field is added with a
default of `1` for missing values so older shards parse without
migration.

## Consequences

- Every span emitted from an agent run answers the "what version
  produced this?" question through `prompt_id` and `tool_desc_ids`.
  Operators can correlate a regression in production to the exact
  prompt change that caused it. `skill_ids` is a reserved slot —
  populated to an empty vector pending the cross-crate snapshot
  decision in `registry/snapshot.rs`.
- The admin console drops its drag-and-drop NDJSON requirement and reads
  trace data through `GET /v1/traces`. ADR-0032 builds eval reports as a
  view over `TraceStore`.
- ADR-0031 has a place to record experiment assignment without
  introducing a parallel attribution channel.
- Trace volume grows: six new attributes per span plus per-run index
  files. The sampling policy + TTL bound the working set; production
  capacity planning includes a budget for the per-month directory tree.
- OTel attribute cardinality grows. `prompt_id` and `tool_desc_ids`
  introduce one new high-cardinality dimension per agent definition.
  Phoenix dashboards that aggregate by these attributes must be sized
  accordingly; the `awaken.replay` resource attribute lets eval traffic
  be filtered out of production aggregates.
- Sampling is configured at startup, not live-tunable. Operators
  changing the policy roll a restart. The startup-only wiring is
  intentionally conservative: a misconfigured `Proportional(1.0)`
  on a high-volume deployment fills disk quickly, and gating the
  change on a deployment step keeps that decision behind the same
  review path as any other config rollout. A live-reload surface can
  be added when an incident-response workflow actually needs it.
- `FileTraceStore` ties the deployment to a single host. Multi-node
  deployments either share the directory via NFS (operationally
  acceptable for lab-scale traffic) or implement a network-backed
  `TraceStore`. The trait keeps that door open.

## Alternatives Considered

**Push to a third-party trace database** (Tempo, Jaeger, ClickHouse).
Rejected as the **primary** store because awaken's operations need
per-run pinning, reference-counting, and cheap programmatic prune — all
of which are awkward retrofits onto general-purpose trace databases. OTLP
export remains in place for shops that already run Phoenix or Tempo.

**Skip persistence; rely on OTLP downstream.** Rejected because eval
(ADR-0032) needs deterministic re-reads of historical traces, including
their request/response payloads, and OTLP backends typically do not
expose per-event APIs.

**Per-event SQLite from day one.** Rejected for now to match the
existing `ConfigStore` pattern (file-backed default, swap-in trait) and
to defer the schema design until query patterns are observed in
practice.

## Open Questions

- Should `prompt_id` cover only the system prompt, or also the user
  message template if one is in use? Current decision: system prompt
  only; user-message attribution is observable through input tokens
  already.
- How are skill content_ids reconciled across hot-reload of the skill
  registry? Hot-reload changes the resolved skill content; the
  attribution field reflects the version at run start, not later
  changes mid-run. This matches the ADR-0014 "snapshot at run start"
  rule.
- Trace export to non-OTLP third parties (S3 cold storage, BigQuery)
  is not addressed here. A future ADR can introduce an exporting trait
  on top of `TraceStore::list`.
