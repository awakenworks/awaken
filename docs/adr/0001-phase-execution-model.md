# ADR-0001: Phase Execution Model

- **Status**: Partially Implemented
- **Date**: 2026-03-21
- **Updated**: 2026-03-24

## Context

We evaluated two execution models: pure queue-based (plugins schedule actions externally, runtime consumes per phase) and pure hook-return (plugins return typed action sets per phase, runtime matches directly). Neither alone is sufficient — queue lacks plugin autonomy, hooks lack extensibility and convergence.

## Architecture Invariants

The system has exactly four mechanisms. Every plugin interaction, every control flow decision, and every external operation must use one of them. No mechanism may be invented that circumvents these four.

### State: persistent observable data

State is the single source of truth for all shared data. Values stored via `StateKey` persist across phases and steps until explicitly modified by a reducer. State is **never used as a transient queue** — if a value is written only to be read once and cleared, it should be an Action instead.

Invariants:

- All state mutations go through `StateCommand.update::<K>(update)` → `MutationBatch` → `StateStore.commit()`.
- Reads always go through immutable `Snapshot` — no mutable reference to the live store is exposed.
- Snapshots are `Arc<StateMap>`: multiple readers (hooks, action handlers) can hold snapshots without blocking writers.
- Each `StateKey` declares a `MergeStrategy` (`Exclusive` or `Commutative`) that governs how concurrent writes to the same key are reconciled.
- Merge is deterministic: given the same set of `StateCommand`s and the same strategy function, `merge_parallel` always produces the same result. This is the foundation for replay safety.
- During GATHER, all hooks see the **same frozen snapshot**. No hook can observe another hook's writes within the same phase. This snapshot isolation eliminates read-write dependencies between concurrent hooks and is the correctness prerequisite for auto-fallback.
- State is not a message queue. The write-read-clear pattern violates state semantics. If something needs to be consumed exactly once, use an Action.

### Action: deferred one-shot work

Actions are the single mechanism for scheduling work at a specific phase. All actions use the same API, the same queue, and the same lifecycle — the only difference is who consumes them.

Every action is:

- Created via `StateCommand.schedule_action::<A>(payload)`.
- Stored in the `PendingScheduledActions` queue (itself a `StateKey`).
- Tagged with a `phase` field that determines when it becomes eligible.
- Declared in `ExecutionEnv` at setup time via `PluginRegistrar::register_scheduled_action`.
- Rejected at `submit_command` if the action key has no registered handler. This catches typos and misconfigurations immediately.

**Consumption**: during the EXECUTE stage of the target phase, the runtime processes actions that have a registered handler — dequeuing them, calling the handler, and committing the resulting `StateCommand`. The orchestrator can also consume actions directly from the `StateCommand` returned by `collect_commands()` (GATHER-only) using `extract_actions`, bypassing the handler/accumulator pattern entirely. Loop actions (`SetInferenceOverride`, `ExcludeTool`, `IncludeOnlyTools`, `ToolInterceptAction`) are consumed this way — extracted from the collected command, with remaining actions (`AddContextMessage`) submitted for handler-based EXECUTE processing. This eliminates write-read-clear accumulator state keys.

Invariants:

- All action keys must have a registered handler. Unknown keys are rejected at `submit_command`.
- EXECUTE only processes actions that have a registered handler AND match the current phase.
- Actions are consumed exactly once. After processing, the action is removed from `PendingScheduledActions` by EXECUTE.
- The convergence loop terminates when no processable actions remain for the current phase, or after `max_rounds` (default 16).
- Action handlers return `StateCommand`, which can contain further state mutations, new actions, and effects. This enables action chaining within a phase.

Current loop-consumed action types:

| Action | Consumed when | Purpose |
|--------|--------------|---------|
| `SetInferenceOverride` | Before building `InferenceRequest` | Per-inference model/parameter overrides |

### Effect: fire-and-forget external I/O

Effects are for irreversible external operations where the system does not need to observe the result. They are emitted via `StateCommand.emit::<E>(payload)` and dispatched immediately after each commit, inline within `submit_command`.

Invariants:

- Effect handlers are **terminal**: they do not return `StateCommand`, do not produce new actions, and do not produce new effects. This prevents feedback loops through the effect path.
- Effect handlers observe the **post-commit snapshot** — they see the state after the commit that triggered their dispatch.
- The core framework defines **no built-in effect types**. All effects are plugin-defined via the `EffectSpec` trait. This keeps the core free of domain-specific concerns.
- Effects are NOT for control flow. Terminate, suspend, block, and similar signals are state mutations (via `RunLifecycle`), not effects. Using effects for control flow would reintroduce side channels.
- Effects are NOT for data that the loop runner needs to read back. If the loop needs to observe the result, use State or Action instead.

### State machines: lifecycle control

Run and tool-call lifecycles are modeled as `StateKey`s with well-defined state machines. The loop runner checks these at phase boundaries to make control flow decisions.

**RunLifecycle** (`__runtime.run_lifecycle`):

```
Running ←→ Waiting → Done
```

- `Running`: actively executing steps.
- `Waiting`: suspended, awaiting external decisions (tool approval, user input).
- `Done`: terminal, with a `done_reason` string encoding the termination cause.

The loop runner checks `RunLifecycle.status` after every phase. If status is no longer `Running`, the loop breaks with a `TerminationReason` reconstructed from `done_reason`. Stop condition plugins trigger termination by writing `RunLifecycleUpdate::Done` via `StateCommand` — no special effect or flag needed.

**ToolCallStates** (`__runtime.tool_call_states`):

```
New → Running → Succeeded / Failed
                → Suspended → Resuming → Running (re-execute)
                              → Cancelled
```

Per-tool-call lifecycle within a step. Each entry also persists the active
`resume_mode`, `suspension_id`, `suspension_reason`, and latest `resume_input`
for the currently suspended or resuming call. Cleared at step boundaries via
`ToolCallStatesUpdate::Clear`.

## Decision

Each phase executes in two stages:

```
GATHER  — run hooks concurrently against the same frozen snapshot;
          each returns StateCommand
          merge all commands by MergeStrategy
          on Exclusive conflict: auto-fallback to serial for conflicting hooks
          commit once
          actions: enqueued for EXECUTE
          effects: dispatched after the merged commit

EXECUTE — convergence loop (max 16 rounds)
          dequeue actions matching this phase that have registered handlers
          run handlers, commit results, enqueue new actions
          actions without handlers remain in queue (for loop consumption)
          loop until no processable actions remain or max_rounds exceeded
```

### GATHER: parallel hooks with conflict auto-fallback

All hooks in a phase run concurrently against the same frozen snapshot. Each returns a `StateCommand` — pure data (state patch + scheduled actions + effects), no side effects. Results are merged via `MergeStrategy`:

- **Disjoint keys**: always merged.
- **Overlapping keys, `Commutative`**: merged (order irrelevant).
- **Overlapping keys, `Exclusive`**: conflict.

On Exclusive conflict, the runtime automatically falls back: the conflicting hooks are re-executed serially against fresh snapshots while non-conflicting results are preserved. This is safe because `PhaseHook::run` is a pure function — it reads a frozen snapshot and returns a `StateCommand` with no external side effects, so re-execution is always safe.

This means plugin authors never need to think about scheduling. Marking a `StateKey` as `Exclusive` does not sacrifice parallelism — it only means the runtime will serialize if contention actually occurs (which is rare in practice when hooks target different keys).

### EXECUTE: handler-consumed actions with merge-or-fail

EXECUTE processes only actions that match the current phase **and** have a registered handler. Actions without handlers are left in the queue for the loop runner to consume at the appropriate time.

Unlike hooks, scheduled action handlers are not constrained to be pure functions. `TypedScheduledActionHandler::handle_typed` may perform external I/O, generate one-time tokens, or trigger side effects. Automatic retry would risk duplicating these effects. Therefore conflicts in EXECUTE are treated as errors, surfaced to the caller.

Future extension: if a handler is known to be replay-safe (pure, idempotent), a `replay_safe` marker on the handler registration could opt in to auto-fallback. This is not part of the initial implementation.

### Effect dispatch

Both GATHER and EXECUTE produce effects via `StateCommand`. Effects are dispatched immediately after each commit (inline within `submit_command`), not deferred. During GATHER there is one merged commit, so effect handlers observe the post-merge snapshot. Effect handlers are terminal — they do not produce new actions or effects.

### Phase-scoped consumption

`ScheduledAction` carries a `phase` field. EXECUTE only dequeues actions matching the current phase (and having handlers); others remain queued. Cross-phase communication prefers state keys over cross-phase action scheduling.

### ToolCall parallelism (deferred)

Tool calls involve external resources (files, network, git worktrees) whose conflicts are not captured by `StateKey` merge strategies. Parallelizing tool execution requires a resource/object declaration mechanism beyond `MergeStrategy`. This is out of scope for the current design; tool calls remain governed by the execution mode in ADR-0007.

## Implementation Status

- GATHER parallel hooks: implemented
- GATHER Exclusive auto-fallback: implemented
- State-driven termination via RunLifecycle: implemented
- Loop actions (overrides, tool filters, intercept) consumed directly by orchestrator via collect_commands + extract_actions — no accumulator state keys
- RuntimeEffect enum: deleted (all variants replaced by State/Action)
- CancellationToken: implemented
- EXECUTE parallel actions: not implemented (currently serial)

## Consequences

- Hooks give plugins autonomy; queue gives extensibility and convergence
- New plugin capabilities require no core enum changes
- The upper-layer agent loop controls phase sequencing via `run_phase()` calls
- Plugin authors only interact with `MergeStrategy` on `StateKey`; scheduling is fully automated by the runtime
- GATHER auto-fallback maximizes parallelism without sacrificing correctness for hooks
- EXECUTE merge-or-fail is conservative: no silent re-execution of side-effectful handlers
- Four mechanisms (State, Action, Effect, State Machine) cover all interaction patterns; no side channels needed
