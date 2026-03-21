# ADR-0001: Phase Execution Model

- **Status**: Implemented
- **Date**: 2026-03-21

## Context

We evaluated two execution models: pure queue-based (plugins schedule actions externally, runtime consumes per phase) and pure hook-return (plugins return typed action sets per phase, runtime matches directly). Neither alone is sufficient — queue lacks plugin autonomy, hooks lack extensibility and convergence.

## Decision

Each phase executes in two stages:

```
GATHER  — run hooks concurrently against the same frozen snapshot;
          each returns StateCommand
          merge all commands in registration order
          commit once
          actions: enqueued for EXECUTE
          effects: dispatched after the merged commit

EXECUTE — convergence loop (max 16 rounds)
          dequeue actions matching this phase
          handler returns StateCommand → commit, enqueue new actions
          loop until queue empty or max_rounds exceeded
```

**Effect dispatch**: Both GATHER and EXECUTE produce effects via `StateCommand`. Effects are dispatched immediately after each commit (inline within `submit_command`), not deferred. During GATHER there is one merged commit, so effect handlers observe the post-merge snapshot. Effect handlers are terminal — they do not produce new actions or effects. This separation prevents feedback loops through the effect path.

**Phase-scoped consumption**: `ScheduledAction` carries a `phase` field. EXECUTE only dequeues actions matching the current phase; others remain queued. Cross-phase communication prefers state keys over cross-phase action scheduling.

## Consequences

- Hooks give plugins autonomy; queue gives extensibility and convergence
- New plugin capabilities require no core enum changes
- The upper-layer agent loop controls phase sequencing via `run_phase()` calls
