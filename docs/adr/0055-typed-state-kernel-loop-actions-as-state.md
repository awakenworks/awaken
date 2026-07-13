# ADR-0055: A Typed State Kernel — Loop-Action Steering Is State, Not a Second Channel; Hooks Return One `Reaction`; External Input Stays Off State

- Status: Proposed
- Date: 2026-07-13
- Builds on: the untyped `Command`/`Store`/`validate_batch` state model
  (`awaken-agent-contract::agent::state`); the typed `StateCell` façade proven in
  the state-machine extension (`awaken-ext-state-machine::state`); the
  `CapabilityBound`/`IdBound` axes (ADR-0004/0027); the per-step commit +
  `Store::rebuild` replay discipline (`awaken-runtime::engine`); the live-inbox
  drain-at-boundary seam (ADR-0054).

## Context

The runtime already has a good typed-state *seed* and a good reason to grow it,
but three seams are inconsistent with it.

**The state store is read untyped at its edges.** `Store::get` returns
`&serde_json::Value`, and every real read out of it is the
`from_value(...).ok().unwrap_or_default()` dance (engine `__usage` at
`engine/mod.rs:728,1699`; `StateCell::load`). This has one dangerous property: a
persisted run whose value *shape* drifts does not error — it silently
deserializes to `Default`, resetting accumulated usage / machine state instead of
failing closed. The typed seed (`StateCell` in `awaken-ext-state-machine`,
`const KEY/SCOPE/MERGE + type Value/Update + apply`) already fixes the *ergonomic*
half of this in one crate, but it is not shared and still reads through the same
silent-default path.

**Request-only context injection has two overlapping semantics on one struct.**
A `PhaseHook` returns `PhaseReaction { state, context }`, where `context:
Vec<Message>` is prepended to the model request but never committed. A
`ToolOutcomeHook` returns `ToolReaction { state, messages }`, where `messages`
*are* committed. "Injected, request-only" vs "committed to the transcript" is
distinguished by *which field* of *which struct* a plugin happens to populate —
not by type. The same duality forces every plugin that wants "inject once per
run" to hold its own throttle: both `awaken-ext-memory::RecallHook` and
`awaken-ext-compact::CompactHook` carry a `Mutex<HashMap<RunId, Vec<Message>>>`
and recompute-guard by hand. That is a cross-cutting invariant (once-per-run
injection) leaking into every plugin, and it does not survive resume — a resumed
run recomputes the recall/compaction sub-agent from scratch.

**External inbound messages and durable state are already — correctly — separate,
and must stay so.** Live steer (`LiveInbox`) and durable pending
(`{prefix}_pending`) land in the *message* log (`{prefix}_message`), never in
`{prefix}_state_command`. Their authority is "committed position in the message
log", their concerns are delivery boundary / provenance / correlation — none of
which the state fold models. This ADR does **not** move them.

The reference reimplementation (`~/Codes/awaken-worktrees/goal`) resolves the
first two seams the same way, and the resolution is the load-bearing idea here:
it has a shared `StateKey` trait, and it expresses **loop-action steering**
(injected context, tool-surface narrowing, inference overrides) as ordinary
**state keys** (`ContextMessageStore`, `ToolFilterState`, `InferenceOverrideState`),
read by the kernel at the relevant phase — *not* as a separate "directive"
channel. External inbound messages it keeps on a dedicated inbox, exactly as we
do.

## Decision

### 1. Promote `StateKey` into the contract; reads are typed and fail-closed.

Lift the `StateCell` trait from `awaken-ext-state-machine` into
`awaken-agent-contract::agent::state` as `StateKey` (same shape:
`const KEY/SCOPE/MERGE`, `type Value/Update`, `apply`). Add a typed accessor
`Store::get_typed::<K>() -> Result<K::Value, StateError>` that **fails closed on a
shape mismatch** rather than silently returning `Default`. `validate_batch`'s
`Exclusive` conflict continues to derive from the command's `merge`, which for a
typed write is `K::MERGE`.

The persisted wire is unchanged: commits remain `Vec<Command>` carrying opaque
JSON (`{prefix}_state_command` / `commits.ndjson`). Promotion is a read/write
*veneer*; as long as a key's `KEY` string and serde shape are unchanged, this is a
pure-code cutover with zero data migration. `Store::rebuild` still replays without
validation, so `apply` must stay total and deterministic (an illegal update is
recorded — e.g. the FSM violation log — never a panic).

`StateCell` in the state-machine extension becomes a re-export of the promoted
trait; its five keys are unchanged.

### 2. Loop-action steering is expressed as typed state keys — there is no
`Directive`/`ExecutionDirective` type.

The three things a hook does to steer the *current* run — inject request-only
context, narrow the tool surface for a step, override inference parameters —
become three `StateKey`s in the contract:

- `ContextMessages` (`Scope::Run`, `Value = Vec<Message>`): messages the kernel
  prepends to the model **request** at assembly time and **does not append to the
  transcript**. This is the typed home of "request-only injection" — the D3
  duality disappears because request-only-ness is now a property of *which key*,
  read at request assembly, versus a `Command` that targets a transcript.
- `ToolFilter` (`Scope::Run`): the per-step tool-surface narrowing the kernel
  applies when it assembles the offered tools.
- `InferenceOverride` (`Scope::Run`): parameters the kernel applies to the next
  inference.

The kernel reads these keys at fixed points (request assembly / before
inference); no closed enum, no handler registry, no new capability axis. We
deliberately reject a `Directive`/`ExecutionDirective`/`DeferredAction` umbrella
type: the concept resisted three naming attempts because it is not a natural
kind. A concrete named key (`ContextMessages`) states its consumer and content;
an umbrella "directive" states neither.

### 3. Once-per-run injection is a state property, not a plugin cache; it survives
resume.

Because `ContextMessages` is `Scope::Run` state, "compute the recall block /
compaction summary at most once per run" is expressed by the hook **reading the
key first**: if it is already populated for this run, skip the sub-agent and let
the committed state replay; otherwise compute and patch it. On resume the key is
replayed from committed truth, so the expensive sub-agent is **not** re-run. The
two `Mutex<HashMap<RunId, Vec<Message>>>` caches in `awaken-ext-memory` and
`awaken-ext-compact` are deleted; the cross-cutting throttle invariant now lives
in the state model, not in each plugin. The compaction key family
(`compaction/<run_id>`) collapses to a single `Scope::Run` key — the scope
supplies per-run identity, so the `<run_id>` suffix is dropped.

### 4. A hook contributes one `Reaction`; `PhaseHook` absorbs `ToolOutcomeHook`.

`PhaseReaction` and `ToolReaction` unify into a single `Reaction { state:
Vec<Command> }`. With loop-actions now living in `state`, the near-term `Reaction`
needs no other field. A fire-and-forget outbound channel (`effects`/`Signal`,
Elm-`Cmd`/MVI-`Signal` in convention) is **deferred** until it has a first
concrete consumer (generative UI); it is not added speculatively.

`ToolOutcomeHook` folds into `PhaseHook` via a new `PhaseHookPoint::AfterTool`,
with the tool call and output carried on `PhaseContext`. The four hook families
collapse to two roles by the "does the kernel branch on the return?" test:
**Reactors** (`PhaseHook`, all points including `AfterTool`) stage a `Reaction`;
**Deciders** (`ToolGate`, `RunEndGuard`) return a verdict the kernel branches on.
`ScheduledAction` (ADR-0020: durable park/resume) remains a **Decider** (gate)
output and does not enter `Reaction`.

### 5. External inbound messages stay off state (unchanged).

`LiveInbox` + `{prefix}_pending` remain the inbound path, committing to the
message log at the safe boundary (ADR-0054). This ADR does not fold them into
state; state and messages remain orthogonal commit channels.

### Convention alignment

The result is the canonical `(State, Effect[])` shape of the Elm/Redux lineage:
the phase event / tool result is the **Action/Msg** (input, carried on
`PhaseContext`, not in the output); `Command` + `Store::apply` is **State +
reducer**; loop-action steering lives in **State** (as `goal` does); the deferred
`effects` channel is **Cmd/Signal**. We keep the codebase's existing `Command`,
`apply`, and `ScheduledAction` names and add only `StateKey` and the three
concrete loop-action keys.

## Guardrails (fitness rules)

1. **Typed follows invariant, not fashion.** A thing gets a `StateKey` only if it
   has a meaningful `apply`; a bare `bool`/counter uses a lightweight generic key,
   not a bespoke type.
2. **A contract type the kernel does not consume does not belong in the contract.**
   Loop-action keys qualify because the kernel reads them; an unused `effects`
   axis does not, so it is deferred.
3. **Cross-cutting invariants live in the kernel (as a state read), not in
   plugins.** A plugin holding a `HashMap<RunId, _>` is a signal the kernel is
   missing a state key or a read point.
4. **Cut a bounded context over in one move; no double-write.** State reads/writes
   flip to typed together; the untyped `from_value` state-read path dies with the
   cutover, not gradually.
5. **Name by domain role.** Steering is named concrete keys, never an umbrella
   `Directive`; a behaviour trait is not a `*Context` (that is data).

## Consequences

- The silent-reset-on-drift hazard is closed: a shape mismatch on a persisted run
  now fails closed with a versioned error instead of resetting to `Default`.
- Resume no longer recomputes recall/compaction sub-agents; injected context
  replays from committed run-scoped state.
- Injected context becomes durable run-scoped state (a small once-per-run commit),
  in exchange for resume-without-recompute — the tradeoff `goal` also takes.
- Capability governance reuses the existing `state_keys: IdBound` axis for
  loop-action keys; no new axis is introduced until `effects` has a consumer.
- The change is a promotion of three seeds already in the tree (`StateCell`, the
  `ResolvedExecutionEnv::merge` topological order, the `capability.rs` governance
  TODO) plus the kernel read points — not a port of `goal`.
