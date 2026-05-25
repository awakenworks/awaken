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
cache efficiency wants to stay put. The existing `fallback_upstream_models`
list only swaps the upstream model *string* on the same provider, so it cannot
escape a provider-wide quota or outage, and it has no notion of a stable home.

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

- **Home** — weighted-rendezvous (HRW) hash of the routing key (the agent id)
  over healthy home-eligible members. Stable per agent (cache affinity) and
  spread across the pool; degrades gracefully as members change.
- **Failover** — the best-scoring healthy member other than the current one.
- **Switch** — whether an `InferenceExecutionError` warrants leaving the
  current member (quota / permanent), per `PoolSwitchPolicy`.

### D3: `PoolExecutor` presents the model contract

`PoolExecutor` implements `LlmExecutor`, so the run loop, streaming, retry, and
context-window clamp treat a pool identically to a model. Resolution builds one
per session over the members, each paired with its resolved provider executor.
Routing is **sticky per session**: a home is chosen on first use and held; a
switch only happens when:

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

- A pool is a drop-in for a model: no changes to `InferenceRequest`, the run
  loop, or downstream plugins. Single-model executors are unaffected.
- Cache efficiency is preserved by deterministic homing + sticky sessions;
  switching is deliberately conservative so the cache only cold-starts on real
  availability loss.
- Cross-provider failover handles account-scoped quota exhaustion, which the
  prior `fallback_upstream_models` could not.

### Known limitation and follow-ups

- **Durable / pinned runs are not yet pool-aware.** Pinned resolution reads
  models from `PinnedSpecMap` (ADR-0035), whose `get_pool` is the default
  `None`, so a durable run whose agent references a pool fails resolution on
  resume. Supporting it requires a `model_pool` kind in the versioned-config
  store and publication pipeline, plus recording the per-session routing
  decisions (home + switches) in the run manifest so replay reproduces the
  identical member sequence.
- **Server config API**: pools are registrable via `AgentRuntimeBuilder`; the
  admin/config write surface for pools follows the durable-config work above.
- **`RoundRobin` home strategy** currently scores like `Deterministic` in the
  pure router (no shared cursor); true round-robin needs session-spanning
  state and is deferred. `Deterministic` (the default) is the cache-optimal
  strategy and the intended primary path.
