# ADR-0001: Phase Execution Model

- **Status**: Partially Implemented
- **Date**: 2026-03-21
- **Updated**: 2026-03-24

## Context

We evaluated two execution models: pure queue-based (plugins schedule actions externally, runtime consumes per phase) and pure hook-return (plugins return typed action sets per phase, runtime matches directly). Neither alone is sufficient — queue lacks plugin autonomy, hooks lack extensibility and convergence.

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
          dequeue all actions matching this phase
          run handlers concurrently against the same frozen snapshot
          merge commands by MergeStrategy
          on Exclusive conflict: hard error (no auto-retry)
          commit once per round, enqueue new actions
          loop until queue empty or max_rounds exceeded
```

### GATHER: parallel hooks with conflict auto-fallback

All hooks in a phase run concurrently against the same frozen snapshot. Each returns a `StateCommand` — pure data (state patch + scheduled actions + effects), no side effects. Results are merged via `MergeStrategy`:

- **Disjoint keys**: always merged.
- **Overlapping keys, `Commutative`**: merged (order irrelevant).
- **Overlapping keys, `Exclusive`**: conflict.

On Exclusive conflict, the runtime automatically falls back: the conflicting hooks are re-executed serially against fresh snapshots while non-conflicting results are preserved. This is safe because `PhaseHook::run` is a pure function — it reads a frozen snapshot and returns a `StateCommand` with no external side effects, so re-execution is always safe.

This means plugin authors never need to think about scheduling. Marking a `StateKey` as `Exclusive` does not sacrifice parallelism — it only means the runtime will serialize if contention actually occurs (which is rare in practice when hooks target different keys).

### EXECUTE: parallel actions with merge-or-fail

Currently EXECUTE processes actions serially. The target design parallelizes same-phase actions:

1. Dequeue all actions matching the current phase.
2. Run their handlers concurrently against the same frozen snapshot.
3. Merge resulting `StateCommand`s via `MergeStrategy`.
4. On success: commit once, enqueue any new actions, next round.
5. On Exclusive conflict: **hard error** — no automatic retry.

Unlike hooks, scheduled action handlers are not constrained to be pure functions. `TypedScheduledActionHandler::handle_typed` may perform external I/O, generate one-time tokens, or trigger side effects. Automatic retry would risk duplicating these effects. Therefore conflicts in EXECUTE are treated as errors, surfaced to the caller.

Future extension: if a handler is known to be replay-safe (pure, idempotent), a `replay_safe` marker on the handler registration could opt in to auto-fallback. This is not part of the initial implementation.

### Effect dispatch

Both GATHER and EXECUTE produce effects via `StateCommand`. Effects are dispatched immediately after each commit (inline within `submit_command`), not deferred. During GATHER there is one merged commit, so effect handlers observe the post-merge snapshot. Effect handlers are terminal — they do not produce new actions or effects. This separation prevents feedback loops through the effect path.

### Phase-scoped consumption

`ScheduledAction` carries a `phase` field. EXECUTE only dequeues actions matching the current phase; others remain queued. Cross-phase communication prefers state keys over cross-phase action scheduling.

### ToolCall parallelism (deferred)

Tool calls involve external resources (files, network, git worktrees) whose conflicts are not captured by `StateKey` merge strategies. Parallelizing tool execution requires a resource/object declaration mechanism beyond `MergeStrategy`. This is out of scope for the current design; tool calls remain governed by the execution mode in ADR-0007.

## Incremental scope

Guiding principle: maximize parallelism on pure-data paths first; stay conservative on side-effectful paths.

- **GATHER Exclusive auto-fallback** — pure-data path, no new concepts for plugin authors.
- **InferenceOverride via StateKey** — `BeforeInference` hooks write to an override key; `loop_runner` reads and applies to `InferenceRequest`.
- **CancellationToken** — cooperative cancellation through `PhaseContext`, `ToolCallContext`, `InferenceRequest`; `LlmExecutor` and `Tool` check token.
- **InferenceRequestTransform** — evaluate whether `BeforeInference` hooks + `StateKey` suffice or a dedicated transform pipeline is needed.
- **EXECUTE parallel actions** — frozen-snapshot parallel + merge-or-fail; no auto-retry. Optional `replay_safe` marker later.
- **ToolCall parallelism** — requires resource/object declaration beyond `MergeStrategy`; deferred.

## Implementation Status

- GATHER parallel hooks: implemented
- GATHER Exclusive auto-fallback: not implemented
- InferenceOverride wiring: implemented via `InferenceOverrideEffect` EffectSpec
- CancellationToken: not implemented
- EXECUTE parallel actions: not implemented (currently serial)

## Consequences

- Hooks give plugins autonomy; queue gives extensibility and convergence
- New plugin capabilities require no core enum changes
- The upper-layer agent loop controls phase sequencing via `run_phase()` calls
- Plugin authors only interact with `MergeStrategy` on `StateKey`; scheduling is fully automated by the runtime
- GATHER auto-fallback maximizes parallelism without sacrificing correctness for hooks
- EXECUTE merge-or-fail is conservative: no silent re-execution of side-effectful handlers
