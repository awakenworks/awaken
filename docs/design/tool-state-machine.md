# Tool Call State Machine

A **tool call state machine** is a declaratively loaded finite-state machine that
constrains the *order* of tool calls in a run. A machine watches a family of tool
calls, keeps a small per-instance state (e.g. one state per `file_path`), and at
each call decides whether the call is allowed, denied, or must pause for approval;
after a call runs it advances the instance state and may surface a reminder to the
model. A canonical machine is *read-before-write*: a `Write` to a path is denied
until a `Read` of that path has moved the instance to `read`.

The state machine is an **extension**: a `Plugin` that contributes hooks and
state keys under its declared `CapabilityBound` (G30). It never writes a store or
grants authorization directly — permission remains the only authorization path
(G9/G21); a machine can only *narrow* what a gate allows, never widen it.

This document owns the state model, the runtime seams the extension needs, and the
durability guarantees (persistence, atomicity, transactionality, restart-safety).

## Why new seams are needed

Three capabilities the extension requires are not yet reachable from a plugin:

1. **Reading accumulated state inside the loop.** The execution loop seeds the
   transcript from committed messages but does not materialize the state `Store`
   into the loop, and `PhaseContext` / `PermissionContext` carry no state. A gate
   or advance step therefore cannot read "has this path been read yet?".
2. **A post-execution reaction point.** The loop has a pre-execution gate
   (`ToolGateHook`) but no symmetric post-execution seam that receives the
   `(ToolCall, ToolOutput)` pair. The four phase-hook points (`StepStart`,
   `BeforeInference`, `AfterInference`, `StepEnd`) carry no tool identity, and
   `StepEnd` is skipped on the step that awaits or ends.
3. **Composing more than one gate.** The loop consults a single host gate; a
   plugin cannot contribute an additional, state-aware constraint.

The design adds exactly four seams to close these, keeps the kernel state model
unchanged, and reuses the existing State and Conversation aggregates rather than
introducing a new effect vocabulary.

## Overview

```text
                     ┌ seam ① state materialization ─────────────┐
                     │  Store = replay(committed, scope-filter)   │
                     │        + this run's staged commands        │
                     └───────────────┬────────────────────────────┘
                                     │ &Store (read-only)
   tool call ──▶ seam ② ToolGateHook chain ──▶ execute ──▶ seam ③ ToolOutcomeHook
                (decision, read-only)                      (reaction: state + message)
                                     │                                │
   natural end ──▶ seam ④ RunEndGuard (reads &Store) ─▶ steer / complete
```

- **Extension** owns machine compilation, the pure evaluation engine, and the
  four hook implementations.
- **Kernel** owns the four seams, the commit boundary, and replay. Its state
  model (`Command` / `Store` / `validate_batch`) is unchanged.

## State model

### Instances and cells

Instance state is keyed by `(machine_name, instance_key)` and holds the current
state id of each instance. It is partitioned along two orthogonal dimensions:

- **Scope** — a machine declares `Thread` (persists across runs on the thread)
  or `Run` (reset at each new run). These live in two separate keys because the
  commit boundary binds `Scope::Run` to the run and `Scope::Thread` to the
  thread; a single key cannot hold both lifetimes.
- **Concern** — instance state, audit metrics, and a bounded violation log are
  separate keys because they carry different value shapes.

This yields four state keys, the minimal partition under `(scope × concern)`:

| Key | Value | Scope | Concern |
|---|---|---|---|
| `tool_fsm_thread_state` | `machine → (instance_key → state)` | Thread | instance state |
| `tool_fsm_run_state` | `machine → (instance_key → state)` | Run | instance state |
| `tool_fsm_metrics` | counters (total + per machine) | Thread | audit |
| `tool_fsm_violation_log` | bounded FIFO of samples | Thread | audit |

### Typed cells over the untyped command model

The kernel state model is deliberately untyped: a `Command` is a whole-value
`Set`/`Remove` under a `(Scope, MergePolicy, key)` address, and the `Store` is
rebuilt by replaying committed commands (G1/G13). The extension keeps a thin
typed façade — one `StateCell` per key — so extension code reads and writes typed
values while the wire form stays whole-value commands. No kernel change.

```rust
/// A typed view over one (scope, key) cell of the untyped runtime Store.
trait StateCell {
    const KEY:   &'static str;
    const SCOPE: Scope;
    const MERGE: MergePolicy;                 // Disjoint — see below
    type Value:  Serialize + DeserializeOwned + Default;
    type Update;
    fn apply(value: &mut Self::Value, update: Self::Update);   // the reducer

    fn load(store: &Store) -> Self::Value {   // read: deserialize whole value
        store.get(Self::SCOPE, &Key(Self::KEY.into()))
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default()
    }
    fn commit(store: &Store, update: Self::Update) -> Command {   // read-apply-write
        let mut v = Self::load(store);
        Self::apply(&mut v, update);
        Command::set(Self::SCOPE, Self::MERGE, Self::KEY, serde_json::to_value(v).unwrap_or_default())
    }
}

/// Declared once; kept in sync with the plugin's CapabilityBound.state_keys (G30).
const STATE_KEYS: &[&str] = &[/* the four keys above */];
```

The reducer (`apply`) folds one update into the value — insert a transition,
increment a counter, push-and-truncate a log sample. Reads go through the live
`Store` provided by seam ① (committed + this run's staged writes), so a
read-apply-write inside one run sees earlier transitions from the same run.

### Merge policy

The four cells use `MergePolicy::Disjoint`. Each cell has a **single producer**
(only this extension writes these keys, bounded by `CapabilityBound.state_keys`),
so "at most one producer, a later write replaces" is exactly the intended
semantics: within a commit batch the extension may write a key more than once
(once per tool result), and on replay the last write — the fully folded value —
wins. `Exclusive` is unsuitable here precisely because the extension legitimately
writes a key more than once per batch; `Commutative` shallow-merges objects and
cannot express counter increment or ordered log truncation.

## Runtime seams

### Seam ① — state materialization

The loop builds a live, read-only `Store` at the start of a fresh run and of a
resume:

```text
Store = replay(committed state for this thread, filtered by scope)
      + this run's staged commands
```

Filtering by scope re-hydrates `Scope::Thread` cells across runs and starts
`Scope::Run` cells empty for a new run. The loop **folds each staged command into
the live Store as it is produced** (`store.apply`), so a later gate/hook read
observes earlier same-run writes. The live `&Store` is passed, read-only, to
seams ②–④.

Committed state is read through the thread reader (the same source that already
serves committed messages) and replayed with `Store::rebuild`.

### Seam ② — tool gate (decision port)

The gate is a pre-execution **decision**: read-only, chainable, restrict-only.

```rust
trait ToolGateHook {
    fn id(&self) -> &str;                                          // bounded (G30)
    async fn gate(&self, ctx: &PermissionContext, state: &Store) -> GateOutcome;
}
```

The loop consults the host permission gate first, then the gates contributed by
active plugins, in dependency order. A call executes only if every gate allows
it. A permission `Deny` is absolute — a plugin gate can further block or suspend
an otherwise-allowed call, but can never turn a permission denial into an
allowance (G21). The state machine maps a denial to `Block { reason }` (the reason
becomes the model-visible tool result) and an approval requirement to
`Suspend { ticket_id }`.

`PermissionContext` stays plain, serializable data; the state view is a separate
read-only parameter, never embedded in it.

### Seam ③ — tool outcome hook (reaction port)

The symmetric post-execution seam receives the executed call and its output and
produces existing aggregates — state commands and conversation messages:

```rust
trait ToolOutcomeHook {
    fn id(&self) -> &str;                                          // bounded (G30)
    async fn after_tool(&self, call: &ToolCall, output: &ToolOutput, state: &Store)
        -> ToolReaction;
}
struct ToolReaction {
    state:    Vec<StateCommand>,   // transitions, metrics, log — via the State aggregate
    messages: Vec<Message>,        // reminders — via the Conversation aggregate
}
```

It fires at the one place a tool result is produced in the ordinary loop, and
again on the resume path where an approved pending call is executed, so an
approved-and-replayed call advances its machine exactly like a first-time call.
Its `state` and `messages` fold into the current checkpoint and commit atomically
with the tool result.

### Seam ④ — run-end guard state

The run-end guard already decides whether a natural end continues (`Steer`) or
completes; it gains the read-only state view so a continuation predicate can
inspect machine instances (e.g. "some instance is not yet terminal"):

```rust
struct RunEndContext<'a> {
    // …existing fields…
    state: &'a Store,     // added
}
```

## Emit reuses the conversation aggregate

A reminder to the model is a `Message` appended to the transcript — the same
mechanism a steered continuation already uses. `ToolReaction.messages` carries
these; the loop materializes them into the transcript as committed facts, so they
replay deterministically. There is no separate effect or action type: a reminder
is a conversation turn, a transition is a state command.

De-duplication ("do not repeat the same reminder for N turns") is the extension's
own policy: it records a last-emitted marker in a `StateCell` and only emits when
the cooldown has elapsed. Reminders are presentation, never authorization; they
cannot grant a protected operation (G9/G21).

## Capability bounding

Every id-bearing contribution is declared in the plugin's `CapabilityBound` and
enforced fail-closed at resolve (G30):

- `state_keys` — the four cell keys (`STATE_KEYS`).
- `tool_gates` — the gate id (seam ②).
- `tool_observers` — the outcome-hook id (seam ③).
- `run_end_guards` — the continuation guard id (seam ④).

`tool_gates` and `tool_observers` are new id-bearing axes on `CapabilityBound` /
`Contributions`; they follow the existing axis rules exactly (subset-of-bound at
`enforce_bound`, cross-plugin uniqueness at merge). `CapabilityBound` is a
contribution ceiling, never authorization, and shares no type with a permission
decision.

Configuration is validated when the plugin is constructed
(`StateMachinePlugin::from_config(cfg) -> Result<_, ConfigError>`): a malformed
pattern or template fails fast at construction rather than at first use.

## Tool State Machine Role Catalog

| Role / component | Responsibility |
|---|---|
| State machine extension (`Plugin`) | compiles machines from config, contributes the gate, outcome hook, run-end guard, and state keys under its declared `CapabilityBound` |
| `StateCell` | typed view over one `(scope, key)` cell of the untyped store; owns load / apply / commit for one value |
| State materialization (seam ①) | builds the read-only live `Store` for a run or resume from committed (scope-filtered) plus this run's staged commands |
| Tool gate chain (seam ②) | pre-execution decision port; read-only and restrict-only, with permission remaining the only grant |
| Tool outcome hook (seam ③) | post-execution reaction port; emits state commands and reminder messages for the executed call |
| Run-end guard state (seam ④) | run-end continuation predicate reading the live `Store` |

## Durability guarantees

All four guarantees rest on the existing commit/replay machinery; the extension
adds no persistence path of its own.

### Persistence

A transition is a `StateCommand`. It is staged, committed in `ThreadCommit.state`
at the single finish boundary, and reconstructed by `Store::rebuild` on replay.
The `Store` is never durable in itself — committed commands are the truth (G1/G13).
Modeling transitions as commands (not messages or transient effects) is what makes
them durable and queryable.

### Atomicity

Every run end funnels through one commit boundary that writes one `ThreadCommit
{ run: RunDisposition, messages, state, events }` (G1/G31). A tool result and
the transition, metrics, and reminder it produced land in the *same* commit — all
or nothing. When a call suspends for approval, the machine's state at that point
commits atomically with the `ResumeTicket` and `RunState::Awaiting`.

### Transactionality

`validate_batch` rejects a batch that violates a merge policy; a conflict becomes
a `StateConflict` fault that drops any pause and commits no state — a run whose
state did not commit cleanly is not resumable. Deny/ask counters are not written
by the gate (which stays a pure read-only decision); they are derived from the
permission-audit events already committed with the call, so they are consistent
with the decision by construction.

### Restart safety

- Committed state replays via `Store::rebuild`; seam ① re-hydrates it into a
  restarted run, scope-filtered.
- A aawaiting run is recovered by durable ingress (lease reclaim) and resumed
  through a committed `ResumeTicket`; `validate_resume` requires every identity
  to match, including `snapshot_id` and `catalog_fingerprint`, so a resume against
  a changed machine definition fails closed.
- Work not yet committed (a crash between tool execution and the finish commit)
  is discarded and re-driven from the last committed checkpoint. A transition is
  idempotent (`set machine[key] = to`), so replaying an at-least-once tool
  execution is safe.

### Failure modes

| Crash point | State | Run state | Recovery |
|---|---|---|---|
| Before tool executes | unchanged | uncommitted | re-run the step |
| After tool, before finish | staged, uncommitted | uncommitted | discard; re-run; transition idempotent |
| During finish commit | atomic (all or nothing) | atomic | success → replay; failure → treat as uncommitted |
| Awaiting for approval, committed | durable | `Awaiting` + ticket | recover → resume → re-hydrate |
| Terminal, committed | durable | `Ended` | recovery reads the fact; never re-runs |

## Configuration (DSL)

Machines are declared as data (JSON/YAML) and compiled once when the plugin is
constructed:

```yaml
machines:
  - name: read-before-write
    scope: thread                 # thread | run
    key: "{file_path}"            # instance key template over tool arguments
    key_normalizer: path          # none | trim | lowercase | path | url
    initial: unread
    terminal: [written]
    transitions:
      - on: 'Read(file_path ~ "*")'
        from: [unread, written, read]
        to: read
      - on: 'Write(file_path ~ "*")'
        from: read
        to: written
        when: { status: success }
        emit:  { content: "Wrote {file_path}", cooldown_turns: 2 }
        on_violation: { action: deny, reason: "Read {file_path} before writing." }

continuation:
  max_continuations: 25
  message: "Finish protocol work: {summary}"
```

- `on` is a tool-call pattern (name plus optional argument conditions).
- `when` conditions a post-execution transition on the tool result (success,
  error, or content match), so an error can route to a different state.
- `emit` injects a reminder (seam ③) subject to the machine's cooldown.
- `on_violation` selects `deny` (block with feedback), `ask` (suspend for
  approval), or `warn` (allow but inject guidance).
- `continuation` steers the run until every machine instance reaches a terminal
  state, up to a cap (seam ④).

## Verification

- **Persistence** — write a transition, restart the process, and confirm a gate
  reads the state back after seam ① re-hydration.
- **Atomicity** — inject a finish failure and confirm the transition, reminder,
  metrics, tool result, and `RunState` are all present or all absent.
- **Transactionality** — multiple machines writing the same scoped key in one
  step commit without conflict (Disjoint + fold); a genuine conflict yields
  `StateConflict` with no state committed.
- **Restart safety** — suspend for approval, kill the process, recover, resume,
  and confirm the machine advances (the approved call replays); resume against a
  changed machine definition fails closed on fingerprint.
- **Scope lifecycle** — a thread machine survives across runs; a run machine
  starts empty each run.
- **Behavior** — write-before-read is denied and the model corrects to
  read-then-write; a warn reminder reaches the next model turn; a continuation
  nudge keeps the run going until terminal.

## Guardrails touched

G1/G13 (commit is the single durable write; state replays from commands),
G9/G21 (permission is the only authorization; gates and reminders never grant),
G30 (every contributed id is within the declared `CapabilityBound`),
G31/G32 (one `RunState` authority written once; replay reads the fact log).
