# ADR-0047: Compaction as an Agent Run — the Agent / Context-Plane Boundary, the Sub-run Substrate, and the `thread_context_compacted` Event

- Status: Proposed
- Date: 2026-07-08
- Builds on: [ADR-0034](0034-runtime-axis-model-and-orthogonality.md) (the axis
  model — protocol vs. runtime vs. environment are orthogonal; the protocol is a
  presentation projection), [ADR-0036](0036-skills-as-runtime-extension-single-tool.md)
  (an extension contributes behavior at a seam; it never speaks the wire),
  [ADR-0039](0039-runtime-persistence-port-convergence-and-store-naming.md)
  (`CommitCoordinator` is the single durable write boundary)
- Relates to: [ADR-0037](0037-managed-capability-advertisement-wire-alignment.md)
  (advertise only what a real producer backs), G3, G13, G14, G16, G21
- Reference cross-check: the Managed Agents events contract
  (`anthropics/skills` → `managed-agents-events.md`) defines
  `agent.thread_context_compacted` (a session-stream event carrying
  `pre_compaction_tokens`), emitted "when the conversation history was summarized
  to fit context."

## Context

The Managed Agents events contract has a compaction event —
`agent.thread_context_compacted` — that awaken does **not** surface. awaken
implements the compaction *behavior* (fold the older slice into a summary,
inject it request-only) but the moment is never projected onto the session event
stream: it is neither emitted nor asserted. `grep compact` across
`awaken-protocol-managed/src` is empty; `OutboundKind` (`dto.rs`) has no variant
for it.

A question — *"can the compaction agent just be a normal agent?"* — surfaced a
deeper one: how much of "everything is an agent" already holds here, and where
does it bottom out. Two traces answered it precisely.

### What already holds (ground truth)

The compactor is **already a normal agent run**, at the same execution altitude
as a top-level turn:

- Every locally-runnable agent — the main assistant, native delegates, and the
  auxiliary agents (memory extractor, judge, compactor) — is one entry in the
  same catalog (`awaken-runtime-host/src/agent_catalog.rs:4`).
- The compactor is a plain `RunnableConfig`
  (`awaken-ext-compact/src/agent.rs:23`, `default_compact_agent`).
- Its host adapter `AgentSummarizer::summarize`
  (`awaken-runtime-host/src/compact.rs:64`) runs it through the shared sub-run
  substrate `run_configured_agent(...)` (`awaken-runtime-host/src/subagent.rs`),
  which builds a `Runtime` and calls `run_to_completion`
  (`subagent.rs:72`) → `self.execute(...)` (`awaken-runtime/src/run.rs:63`) →
  the `RunExecutor::execute` trait (`awaken-runtime-contract/src/execution.rs:18`).
- Native delegates (`awaken-runtime-host/src/delegate.rs`, `native_run` →
  `run_agent`) and the goal judge
  (`awaken-runtime-host/src/judge.rs`, `KernelJudgeRunner` → `run_configured_agent`)
  ride the **same** substrate. A top-level session turn reaches the **same**
  `RunExecutor::execute` (`awaken-runtime-host/src/turn_exec.rs:19` → ingress →
  `runtime.execute`).

So at the execution layer the compactor and the main assistant's Run go through
one seam. A delegated Agent receives the initiating Run's durable commit/history
wiring, first-class child identity, cancellation lineage, and delegation
capability. Auxiliary housekeeping currently chooses an ephemeral context and
sandbox as a host policy; that choice is not a different Runtime lifecycle.

### Why the compactor is *invisible* today

Not because it "isn't an agent" — it is — but because of (a) above: its
thread/events commit to a throwaway coordinator, so they can never be enumerated
as a session thread. Independently, the managed adapter's `list_threads` returns
**only** the primary thread (`awaken-protocol-managed/src/state.rs:1050`), so no
child thread would be visible even if it were durable.

### The precedent to mirror

`span.outcome_evaluation_*` is the existing "lifecycle span" event. It is **not**
transcoded from committed messages (`project.rs:126` `ManagedEncoder` handles
only message-derived events). It is a side-band projection: the goal plugin
commits opaque neutral detail, the host reconstructs a neutral `OutcomeReport`
from durable truth, and the adapter projects it in `append_outcome`
(`state.rs:1229`, the `SpanOutcomeEvaluation*` pushes at `state.rs:1250`).
Compaction is the same shape.

## Decision

### D1: Name the boundary — "everything is an agent" bottoms out at context management

Compaction decomposes into two parts of **different nature**, and this split is
the durable design principle:

- **The labor** — summarize an older slice — is *agent-shaped*, and is already a
  normal agent run (`compactor`). It stays that way.
- **The trigger + the context rewrite** — *when* to fold (the context-budget
  decision at `BeforeInference`) and injecting the summary **request-only** into
  the parent's next inference — is *context-plane policy*, and is **definitionally
  not** agent-shaped.

An agent cannot "as just an agent" decide to compact itself and rewrite its own
inference request: that is the platform reshaping the parent's context, a
context-plane responsibility. Pushing it into an agent yields either the
self-management paradox (the model must reliably self-trim — an infra concern
leaking into the agent's task) or infinite regress (the deciding agent itself
grows context and needs a decider). Therefore the trigger + rewrite stays a thin
`PhaseHook` (`CompactPlugin`, `awaken-ext-compact/src/plugin.rs:137`). This is
the irreducible non-agent core; everything else is an agent run.

### D2: The event is a **projected** parent-thread marker, never emitted by the extension (G16)

`agent.thread_context_compacted` is a lifecycle marker on the parent thread. The
wire string lives **only** in the managed adapter (`dto.rs` + one projection
arm). The chain is: the compaction extension produces a *protocol-neutral fact* →
the runtime commits it through the one `CommitCoordinator` (G13) → the host
reconstructs it from durable truth → the managed adapter projects it to
`OutboundKind::ThreadContextCompacted`. `awaken-ext-compact` and
`awaken-runtime` never learn the wire type — the neutral-vocabulary invariant
(G16). Other protocols (A2A, ACP) project or drop the same neutral fact.

Both carrier options in D3/D4 emit the *same* parent marker; they differ only in
how observable the summarization *labor* is. D3 is therefore forward-compatible
with D4.

### D3: Ship now — the neutral fact rides the existing `StateCommand` channel

The compaction hook already returns a `PhaseReaction { state, context }`
(`awaken-runtime-contract/src/plugin.rs:82`) and today fills only `context` (the
request-only summary), leaving `state` empty. On a fold it also stages a durable
marker (G3) via the generic KV command it is already allowed to use
(`awaken-agent-contract/src/agent/state.rs:62`, `Command::set`), keyed by the
turn's `run_id` so the host reads it back exactly once:

```
Command::set(Scope::Thread, MergePolicy::Commutative,
             format!("compaction/{run_id}"), json!(true))
```

Carriage and projection are symmetric to the outcome-eval precedent:

- `TurnOutcome` (`state.rs:46`) gains `compacted: bool` (neutral).
- `SessionRuntime::run_turn`'s host impl reads that marker back from durable
  thread state at the **terminal** step only (`finish_step`), so an awaiting→resumed
  turn — which shares one `run_id` — surfaces it exactly once.
- `append_turn` (`state.rs:1208`) pushes an `OutboundKind::ThreadContextCompacted {}`
  **before** the turn's message events (compaction runs `BeforeInference`), then
  the existing `project_turn`.
- `dto.rs` adds the variant + `type_str` (`"agent.thread_context_compacted"`). The
  `AgentEvent` transcoder (`project.rs:126`) is **not** touched — compaction is
  not a committed message.

**Wire shape — aligned to the installed SDK, not the prose.** `@anthropic-ai/sdk@0.105.0`
types `BetaManagedAgentsAgentThreadContextCompactedEvent` as exactly
`{ id, type, processed_at }` — **no `pre_compaction_tokens`**, despite the events
reference mentioning it. Per the standing "align to the installed SDK, never
guess" constraint, the event is a payload-free marker; the field is dropped until
an SDK release types it. This also collapses the whole path to a single `bool`
(no token estimator, no fact struct). No new mechanism, no new trait, no wire leak
below the adapter.

### D4: North-star (deferred) — promote aux sub-runs to durable child session threads

The elegant end state makes compaction a first-class *system-triggered subagent*:
run the compactor on a **child session thread** (`parent_thread_id` = the main
thread) against the session's **durable** commit store, so the summarization turn
is observable through the thread API for free and the marker references its child
thread. The concrete gap is exactly (a) from Context — the isolated
`MemoryCommitCoordinator` in `subagent.rs` — not any missing "agent-ness."
Migrating aux sub-runs to the session's durable store with parent-thread linkage
is the whole change, subject to:

- **G13**: sub-run commits must go through the session's single `CommitCoordinator`.
- **Capability policy**: an auxiliary Agent receives only the tools/plugins in
  its compiled Agent config; delegated Agents retain the normal delegation
  capability and may create further child Runs subject to depth/cycle/budget limits.
- **Depends on** full multiagent child-thread enumeration in the managed adapter
  (`list_threads` is primary-only today, `state.rs:1050`).

This is its own initiative; compaction is its first client.

### D5: Converge the three aux-agent ports into one run-subagent port (deferred)

`Summarizer` (ext-compact, `plugin.rs:29`), `DelegateRunner` (ext-goal,
`lib.rs:218`), and `RunDelegationService` / `agent_run` (delegation) are three traits
whose host impls all bottom out in `run_configured_agent`. Collapse them into a
single neutral "run a sub-agent" capability port; remote A2A delegates
(`delegate.rs:87`, `remote_run`) stay separate (network semantics). Fewer ports,
one substrate — a simplification, not a feature cut.

## Development-Ready Design (G14) — D3

Change list, each additive:

1. `awaken-ext-compact/src/plugin.rs` (`on_phase`): on a non-empty fold, return
   `PhaseReaction { state: vec![compaction Command::set], context: summary }`;
   replay (cache hit) returns context only. Only the folding step stages the fact.
2. `awaken-runtime-host/src/compact.rs` / `subagent.rs`: no behavior change; the
   staged command commits through the existing path.
3. `awaken-runtime-host` `SessionRuntime::run_turn` impl: read back
   `compaction/*` thread keys committed this turn → `TurnOutcome.compactions`.
4. `awaken-protocol-managed/src/dto.rs`: add
   `OutboundKind::ThreadContextCompacted { pre_compaction_tokens: Option<u64> }`
   + `type_str` arm.
5. `awaken-protocol-managed/src/state.rs`: `TurnOutcome.compactions`; `append_turn`
   pushes the marker before `project_turn`.
6. Test — `e2e/managed_compaction_e2e.mjs`: keep the summary-injection assertion;
   add that the event stream contains `agent.thread_context_compacted` **before**
   the folding turn's `agent.message`. Unit: an ext-compact test that a fold
   stages a `compaction/<step>` state command.

## Consequences

- **Contract parity**: the compaction event the Managed events contract defines
  is emitted and covered end-to-end (no stub).
- **A reusable principle**: D1 draws the agent / context-plane line, so future
  "make X an agent" proposals have a test — is X *labor* (agent) or *context /
  platform policy* (not)?
- **Forward-compatible**: D3 ships now; the marker is identical under D4, so the
  later child-thread migration only adds observability of the summarization
  labor, changing no wire shape.
- **Deferred cost**: D4/D5 are larger and gated on multiagent child-thread
  maturity; recorded here so the increment is intentional, not forgotten.

### Risks

- `pre_compaction_tokens`: the events reference mentions it but the installed SDK
  (`0.105.0`) does not type it, so it is **not** emitted (align-to-SDK). If a
  future SDK adds it, revisit — the extension would then estimate it (message
  text ≈ 4 chars/token) or thread real model usage through `PhaseContext`.
- Multiple folds per session: `Scope::Thread` + per-step key
  `compaction/<step>` + `Commutative` merge yields one event per fold and
  replays idempotently across restart (rebuilt from durable keys).

## References

- Managed Agents events contract, `agent.thread_context_compacted`
  (`anthropics/skills` → `managed-agents-events.md`).
- Sub-run substrate: `awaken-runtime-host/src/subagent.rs`, `compact.rs`,
  `judge.rs`, `delegate.rs`; `awaken-runtime/src/run.rs`;
  `awaken-runtime-contract/src/execution.rs`.
- Projection precedent: `awaken-protocol-managed/src/{project.rs,state.rs,dto.rs}`.
- [ADR-0034](0034-runtime-axis-model-and-orthogonality.md),
  [ADR-0039](0039-runtime-persistence-port-convergence-and-store-naming.md).
