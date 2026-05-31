# ADR-0031: Experiment Routing and Guardrails

- **Status**: 📐 Proposed
- **Date**: 2026-05-10
- **Depends on**: ADR-0010, ADR-0014, ADR-0023, ADR-0028, ADR-0029, ADR-0030

## Context

ADR-0029 made tool descriptions a first-class `ConfigRecord` with override
patches; ADR-0028 added version switching and rollback semantics. These
mechanisms let an operator change a prompt or tool description in one shot
and (separately) revert it. They do not let an operator **compare** two
versions in production by routing a fraction of traffic through the
candidate while the control keeps serving the remaining traffic.

The current registry resolution path (`registry/resolve/pipeline.rs`,
ADR-0010) takes one input — the canonical agent / tool / skill id — and
returns a single `ResolvedAgent` / `ToolDescriptor`. There is no hook to
substitute the resolved content based on a routing decision.
`RegistrySnapshot::version` (`registry/snapshot.rs:11`) is monotonic over
the whole snapshot; it cannot represent "agent A is at content_id v1 for
half of users and v2 for the other half".

ADR-0030 introduced content-addressed `prompt_id` / `tool_desc_id` /
`skill_content_id` attribution and reserved `experiment_id` /
`variant_name` slots in `SpanContext`. This ADR fills those slots: it
defines the experiment data model, the resolution-time routing primitive,
the bucket-assignment function, the guardrail loop that auto-rolls back a
breaching variant, and the promote semantics that turn a winning
candidate into the new control.

## Non-Goals

- A multi-arm bandit or any adaptive allocation. Allocation is fixed
  weights set by the operator. Adaptive allocation is a future ADR built
  on top of the same data model.
- Sequential tests, false-discovery correction, or any inferential
  statistics. Guardrails operate on directly-observed thresholds
  (p95 latency, error rate, judge score). Significance testing happens
  off-server using the trace export.
- Per-user feature flags unrelated to prompts / tools / skills. Generic
  flag-flipping is out of scope; this ADR addresses only content
  variation that flows through the registry resolve pipeline.
- Auto-promote of a winning candidate. Promotion always requires a human
  action even when guardrails pass. Auto-promote is intentionally
  deferred until operators have lived with the manual flow.
- Cross-experiment interaction analysis (mutually exclusive layers,
  factorial designs). Each experiment is independent; collisions where a
  single resolution touches two experiments at once are rejected at
  experiment-creation time.

## Decisions

### D1: `Experiment` is a First-Class `ConfigRecord` Kind

A new `Experiment` is added to `awaken-contract`, stored in `ConfigStore`
under namespace `experiments` with `source: User`. Built-in seeded
experiments are not allowed; experiments are operator-created.

```rust
// crates/awaken-runtime-contract/src/experiment.rs (new)
pub struct Experiment {
    pub id: String,                       // ULID
    pub target: ExperimentTarget,         // Agent | Tool | Skill, with target id
    pub bucket_key: BucketKey,            // ThreadId | UserId | RequestId
    pub variants: Vec<ExperimentVariant>, // weights sum to 1.0; at least 2
    pub guardrails: Guardrails,
    pub status: ExperimentStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub enum ExperimentTarget {
    Agent { agent_id: String },
    Tool { tool_id: String },
    Skill { skill_id: String },
}

pub struct ExperimentVariant {
    pub name: String,                     // "control" | "candidate" | ...
    pub content_id: String,               // ADR-0030 content-addressed id
    pub weight: f32,                      // [0.0, 1.0]
}

pub enum BucketKey { ThreadId, UserId, RequestId }

pub struct Guardrails {
    pub p95_latency_ms_max: Option<u32>,
    pub error_rate_max: Option<f32>,
    pub min_judge_score: Option<f32>,
    pub min_samples: u32,                 // before evaluation kicks in
    pub evaluation_window: Duration,      // rolling window over trace data
}

pub enum ExperimentStatus {
    Draft,
    Ramping,
    Shipped,
    RolledBack { reason: RollbackReason, at: DateTime<Utc> },
}

pub enum RollbackReason {
    GuardrailBreach { metric: String, observed: f32, threshold: f32 },
    ManualRollback { actor: String, note: Option<String> },
}
```

Validation rules applied at create / update:

- `variants.len() >= 2` and `<= 8`.
- `variants.iter().map(|v| v.weight).sum::<f32>()` is within `1.0 ± 1e-4`.
- Each `content_id` resolves to a known content under the experiment's
  target kind. Unknown ids are rejected with 422.
- The target id refers to an existing agent / tool / skill.
- No two `Ramping` experiments share the same `(target_kind, target_id)`.
  Operators must roll back or ship one before starting the other.

### D2: `ExperimentResolver` Step in the Resolve Pipeline

`registry/resolve/pipeline.rs` (ADR-0010) gains a single new step that
runs after canonical resolution but before `ResolvedAgent` /
`ToolDescriptor` materialisation:

```
canonical resolve  →  ExperimentResolver  →  finalise resolved entity
```

The resolver:

1. Looks up the active `Ramping` experiment for the target. If none,
   returns the canonical resolution unchanged.
2. Computes `bucket = consistent_hash(experiment.id, bucket_key_value)
   % 10_000`. The experiment id is included in the hash so the same
   bucket value across experiments does not always select the same
   variant — avoids correlated assignments that would otherwise bias
   downstream guardrail signals.
3. Picks the variant where `bucket / 100.0 < cumulative_weight`.
4. Substitutes the variant's `content_id` content into the resolution
   result. Substitution is opaque to downstream code; the result is
   still a regular `ResolvedAgent` or `ToolDescriptor` whose
   `invoke()` / inference path is unchanged.
5. Stamps `experiment_id` + `variant_name` onto the resolution context
   so the observability hooks pick them up at `BeforeInference` /
   `BeforeToolExecute` (ADR-0030 D2).

`bucket_key_value` resolution:

| `BucketKey` | Source                                        |
|-------------|-----------------------------------------------|
| `ThreadId`  | The active `thread_id` on the run             |
| `UserId`    | The authenticated principal id, if present    |
| `RequestId` | The incoming request id (one-shot bucketing)  |

If the chosen `bucket_key` cannot be resolved (e.g., `UserId` requested
on an unauthenticated run), the resolver falls back to the canonical
content and emits a `experiment.bucket_missing` warning event on the
trace. It does not fail the run.

### D3: Sticky Assignment Within an Experiment Lifecycle

`bucket_key` selection determines what stays consistent and what does
not:

- `ThreadId`: a thread's variant assignment is stable for the
  experiment's `Ramping` lifetime. Mid-conversation variant flips are
  prevented because the consistent hash is deterministic and the same
  `thread_id` always maps to the same bucket.
- `UserId`: a user crosses sessions on the same variant; useful when
  measuring multi-session retention or longitudinal judge scores.
- `RequestId`: every request is independently bucketed; useful for
  one-shot tools where session-level consistency is not needed and
  variance reduction wants the larger sample.

Status transitions affect assignment:

- `Draft → Ramping`: resolver starts substituting.
- `Ramping → RolledBack` or `Shipped`: resolver stops substituting on the
  next resolution call. **In-flight runs that already captured a
  resolved entity at `RunStart` keep their assignment** — this matches
  the ADR-0014 "snapshot at run start" invariant. The trace continues
  to carry the assignment recorded at run start; new runs see the new
  status.

### D4: Guardrail Evaluation Loop

`crates/awaken-server/src/services/experiment_service.rs::evaluate_guardrails`
is invoked on a `/loop` cadence (default 1 minute). For each `Ramping`
experiment:

1. Read trace summaries from `TraceStore::list` (ADR-0030 D7) filtered
   by `experiment_id` and `since = now - guardrails.evaluation_window`.
2. Group by `variant_name`.
3. For each variant, compute p95 latency, error rate, and (if judge
   results are present) mean judge score.
4. Skip the experiment if any variant has fewer than
   `guardrails.min_samples` traces in the window.
5. If any variant breaches a guardrail threshold, transition the
   experiment to `RolledBack { reason: GuardrailBreach { metric,
   observed, threshold } }` atomically through `ConfigService` and emit
   an audit entry.

The loop is single-writer per cluster, mirroring the ADR-0029 D11
`--leader-seed` constraint. Followers read experiment state through the
existing `start_periodic_refresh` path.

A breached experiment does not auto-revert traffic on in-flight runs
(by D3); new runs immediately revert. The audit entry includes the
control `content_id` so operators can confirm what traffic returns to.

### D5: Promote Semantics

When an operator decides a `Ramping` experiment has won, they call
`POST /v1/config/experiments/:id:ship`. The handler:

1. Asserts current status is `Ramping`.
2. Identifies the winning variant from the request body
   (`variant_name`).
3. Updates the canonical content for the experiment's target so the
   winning variant's `content_id` becomes the new control. The
   mechanism depends on the target kind:
   - **Agent / Tool**: writes a `ConfigRecord` patch through the
     existing override path (ADR-0029) that pins the canonical
     content to the winning `content_id`. The override is recorded as
     `source: User` with a `provenance: ExperimentShipped { id }`
     marker.
   - **Skill**: the skill registry's content for the target is
     updated through the same patch path.
4. Transitions the experiment to `Shipped`.
5. Emits an audit entry citing the rollback path: the previous
   canonical `content_id` is preserved on the experiment record so
   `:rollback` after ship can restore it.

`POST /v1/config/experiments/:id:rollback` works on either `Ramping` or
`Shipped` experiments and reverts the canonical content to the
pre-experiment state. Rollback after `Shipped` is the operational
escape hatch for "we shipped the wrong thing".

### D6: HTTP API

| Method   | Path                                          | Purpose                              |
|----------|-----------------------------------------------|--------------------------------------|
| `GET`    | `/v1/config/experiments`                      | List with status filter              |
| `GET`    | `/v1/config/experiments/:id`                  | Full record                          |
| `POST`   | `/v1/config/experiments`                      | Create (status: `Draft`)             |
| `PATCH`  | `/v1/config/experiments/:id`                  | Update mutable fields                |
| `DELETE` | `/v1/config/experiments/:id`                  | Hard delete (only when `Draft`)      |
| `POST`   | `/v1/config/experiments/:id:start`            | `Draft → Ramping`                    |
| `POST`   | `/v1/config/experiments/:id:ship`             | `Ramping → Shipped` + canonical patch|
| `POST`   | `/v1/config/experiments/:id:rollback`         | Force `RolledBack`                   |
| `GET`    | `/v1/config/experiments/:id/metrics`          | Per-variant aggregate over window    |

All routes gated by `ensure_admin_auth` (ADR-0023). A new
`AdminApiConfig.expose_experiment_routes` boolean (default `true`)
follows the existing pattern.

### D7: Audit and Restore

Reuses `AuditEvent` with `resource = "experiments/:id"`. Status
transitions, weight changes, and ship/rollback all carry the
`before` / `after` payload. The restore endpoint from ADR-0028 works
on experiments without modification; restoring a deleted experiment
recreates the `Draft` record but does not auto-resume rotating
traffic.

### D8: Distributed Deployment

Inherits the existing config pipeline's distributed semantics
(ADR-0029 D11):

- Each replica resolves variants from its own snapshot. Cross-replica
  drift on weight changes is bounded by the periodic refresh interval.
  Buckets remain stable across replicas because the consistent hash
  uses only `experiment.id` + `bucket_key_value` — no replica-local
  state.
- Guardrail evaluation runs on the leader replica only
  (`--leader-seed=true`). Followers see status flips via refresh.
- Concurrent `:ship` and `:rollback` calls to different replicas are
  serialised through the same store-level mutex used by other config
  patches; second writer wins. Operators are advised to use
  sticky-session routing for admin traffic, as ADR-0029 D11
  recommends.

### D9: Observability of the Resolver Itself

The `ExperimentResolver` step emits a `experiment.assignment` trace
event at the resolution boundary (before run start) carrying:

- `experiment_id`, `variant_name`
- `bucket_key`, `bucket_value`
- `cumulative_weights` snapshot at decision time

This is the audit trail for "why did this run see this variant?". The
event lands on the run's trace alongside the rest of the spans; admin
console diagnostics can reconstruct any individual run's assignment.

`experiment.bucket_missing` and `experiment.config_drift` (when a
follower's snapshot is stale on `:ship`) are emitted on the same
channel.

## Consequences

- Operators can ramp a prompt or tool description change at any
  weight in `[0.0, 1.0]`, observe per-variant trace metrics through
  `GET /v1/config/experiments/:id/metrics`, and ship or roll back
  with a single API call.
- Trace data carries variant assignment, so post-hoc analysis (in
  Phoenix or off-server tooling) can slice production behaviour by
  variant without any sidecar logging.
- The resolve pipeline gains a small constant-time overhead per
  resolution: one hash computation and one experiment lookup. Both
  are O(1) over the count of `Ramping` experiments per target,
  which is bounded to one by D1.
- Guardrail auto-rollback caps the blast radius of a bad variant.
  The `evaluation_window` and `min_samples` knobs let operators
  trade detection latency against false-rollback rate.
- The `Shipped` transition writes through the canonical override
  path. From the rest of the system's point of view, "shipping a
  candidate" is indistinguishable from "operator manually patched
  the description with the candidate's content". This means the
  rollback story, the audit trail, and the version-switching
  mechanism (ADR-0028) all work on shipped variants without
  modification.

## Alternatives Considered

**Implement bucketing in the application layer at request boundary,
above the registry.** Rejected: A/B routing belongs inside the
resolution that already takes id → content. Putting it above the
registry would duplicate the snapshot semantics, fragment audit, and
prevent skill / tool-level experiments where the routing point is not
the request boundary.

**Use the existing `ConfigRecord` override mechanism (ADR-0029) with
a per-bucket override.** Rejected: an override is a single patch;
representing N variants at once would distort the override schema and
break the assumption that an override produces one canonical content.
A separate first-class `Experiment` keeps the override schema clean.

**Adaptive (multi-arm bandit) allocation as the default.** Rejected:
adaptive allocation requires a clear reward signal that not all
target kinds have (judge scores are not always present), and it
makes guardrail interpretation harder because the variant
distribution shifts mid-window. Fixed weights are the right default;
adaptive can ride on top later.

**Skip routing; rely on dark launches and manual side-by-side
comparison.** Rejected: dark launches double the inference cost and
cannot exercise tool-call paths whose decisions depend on the model's
output, so they are not a substitute for true online routing.

## Open Questions

- Should guardrail breach trigger a webhook or notification in
  addition to the audit entry? Current decision: audit only; ops
  teams that want paging can subscribe via the existing audit
  feed.
- Should `bucket_key` be settable to an arbitrary header value
  (`HeaderName`) for integrations that have neither thread nor
  user identity available? Deferred until a concrete need surfaces;
  the existing three are sufficient for the agent runtime.
- Should `min_judge_score` guardrails wait for all in-window runs to
  have judge results, or proceed with available samples? Current
  decision: proceed with available samples once `min_samples` is
  reached; runs without judge results are excluded from the mean.
