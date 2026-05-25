# ADR-0042: Model Pool — Sticky Routing and Failover

- **Status**: 🚧 Proposed
- **Date**: 2026-05-25
- **Depends on**: ADR-0035 (published versioned registry & runtime pinning), ADR-0040 (resolver + resolved run)
- **Breaking**: no (additive)

## Context

An agent is bound to a single model via `AgentSpec.model_id`. Two pressures
make a single fixed binding insufficient:

- **Availability**: a provider can rate-limit, exhaust an account quota, or
  fail for a sustained period. A single binding has no recourse beyond the
  existing intra-model retry (`RetryingExecutor`) and per-model circuit
  breaker, which retry the *same* upstream model.
- **Cache efficiency**: upstream prompt caching (e.g. Anthropic
  `cache_control`) keys on the exact model plus the conversation prefix.
  Spreading an agent's traffic across models, or switching models eagerly,
  cold-starts the cache and inflates cost and latency.

These pull in opposite directions: availability wants to move between models;
cache efficiency wants to stay put. The old retry-layer upstream-model list
only swapped the model *string* on the same provider, so it could not escape a
provider-wide quota or outage, and it had no notion of a stable home.

## Decision

Introduce a **model pool**: a named set of member models that presents the
single-model `LlmExecutor` contract. An agent references a pool exactly where
it would a model. Each agent is pinned to a deterministic *home* member for
cache affinity; the pool only moves off it on sustained failure or quota
pressure, and prefers to return once the home recovers.

### D1: `ModelPoolSpec` shares the model id namespace

`ModelPoolSpec { id, members: Vec<PoolMemberSpec>, routing, switch }` lives in
`awaken-contract::registry_spec`. A `PoolMemberSpec` references a `ModelSpec`
by id (each member is a full model with its own provider, enabling
cross-provider/cross-account failover) and carries an optional `weight` and a
`role` (`Member` = home-eligible, `FailoverOnly`). `AgentSpec.model_id`
resolves to **either** a `ModelSpec` or a `ModelPoolSpec`; ids are unique
across the combined namespace. Validation requires a non-empty member list, no
duplicate members, positive weights, and at least one home-eligible member.

### D2: `PoolRouter` makes pure routing decisions

`PoolRouter` (in `awaken-runtime::engine`) owns member metadata + policy and
answers three pure questions, with member health passed in as a mask:

- **Home** — weighted-rendezvous (HRW) hash of the routing key (the thread id
  for agent-loop requests, with agent id as a fallback outside a run) over
  healthy home-eligible members. Stable per conversation (cache affinity) and
  spread across the pool; degrades gracefully as members change.
- **Failover** — the best-scoring healthy member other than the current one.
- **Switch** — whether an `InferenceExecutionError` warrants leaving the
  current member (quota / permanent), per `PoolSwitchPolicy`.

### D3: `PoolExecutor` presents the model contract

`PoolExecutor` implements `LlmExecutor`, so streaming, retry, and
context-window clamp treat a pool identically to a model. Resolution builds one
over the members, each paired with its resolved provider executor. Agent-loop
requests carry the thread identifier in the request `routing_key`, and routing
is **sticky per thread**: a home is chosen on first use and held; a switch only
happens when:

- the active member returns a **quota** (`RateLimited`/`Overloaded`, gated by an
  optional retry-after threshold) or **permanent** (`Unauthorized`/
  `ModelNotFound`) error — an in-call switch to another member; or
- the active member's circuit breaker has **opened** (sustained transient
  failure = "long-term failure") — a later call re-homes off it.

Transient single failures are returned to the caller (absorbed by the member's
own retry policy); request-level errors (`ContextOverflow`, `InvalidRequest`,
`ContentFiltered`) and `Cancelled` never switch, since they fail identically on
every member. A shared `CircuitBreaker` keyed by member model id carries health
across sessions: while a member is unhealthy every session avoids it, and
sessions return once it heals — giving thread-like stickiness without persisted
per-thread state. `max_switches_per_session` bounds churn.

### D4: Capability reconciliation

The pool resolves to a synthetic `ModelSpec` whose `context_window` and
`max_output_tokens` are the **minimum known bound** across members (unknown
members ignored), so the context-window clamp is safe regardless of which
member serves. Modalities, knowledge cutoff, and pricing are left unset — they
have no runtime effect today and cannot be soundly attributed to one member.

### D5: Resolution via the model registry

`ModelRegistry` gains default `get_pool` / `pool_ids` methods (registries
without pools return `None`). `resolve_model_and_executor` checks `get_pool`
before `get_model`; a hit builds a `PoolExecutor` via
`registry::resolve::pool::build_pool_executor`. The in-memory `MapModelRegistry`
holds models and pools in one namespace.

## Consequences

- A pool is a drop-in for a model. Requests carry an optional routing key so
  pool executors can keep per-thread affinity; single-model executors ignore it.
- Cache efficiency is preserved by deterministic homing + sticky sessions;
  switching is deliberately conservative so the cache only cold-starts on real
  availability loss.
- Cross-provider failover handles account-scoped quota exhaustion, which the
  prior retry-layer upstream-model list could not.

### Durable runs and replay

Durable/pinned runs are pool-aware. A `model_pool` is a published versioned
kind: it is included in the publication graph (an agent's `model_id` resolves
as a model **or** a pool; a pool depends on its member models), frozen into the
run's pinned manifest, and resolved at resume via `PinnedModelRegistry`, which
serves models and pools from one id namespace exactly as the live
`MapModelRegistry` does.

Replay determinism rests on two properties rather than a recorded member log:
the pinned manifest freezes the pool and member specs, and home selection is a
stable hash of the thread id — so a resumed run resolves the identical pool
configuration and homes to the same member for that conversation. The
per-session circuit breaker is process-local, so after a restart a
previously-avoided member is re-probed (desirable: health is re-evaluated).
Completed turns replay their recorded
outputs and do not re-invoke routing.

### Follow-ups

- **Server config write API**: pools are registrable via `AgentRuntimeBuilder`
  and through the config store's `model-pools` namespace (so they publish and
  freeze), but a dedicated admin HTTP surface for pool CRUD is not yet added.
- **Eval-grade replay**: for byte-identical eval reproduction across differing
  breaker state, record per-session routing decisions (home + switches) as an
  event and replay them; not required for durable resume correctness.
- **`RoundRobin` home strategy** currently scores like `Deterministic` in the
  pure router (no shared cursor); true round-robin needs session-spanning
  state and is deferred. `Deterministic` (the default) is the cache-optimal
  strategy and the intended primary path.
