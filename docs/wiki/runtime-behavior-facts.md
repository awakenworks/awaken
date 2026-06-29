---
type: Fact Index
title: Runtime Behavior Facts
description: Cross-document facts for run phases, state/effects/events, plugins, scheduled work, cancellation, and eval.
tags: [runtime, lifecycle, state, effects, events, plugins, eval]
timestamp: 2026-06-27T00:00:00+08:00
---

# Runtime Behavior Facts

Owner: [runtime-behavior.md](../design/runtime-behavior.md).

## FACT-RUNTIME-001: Runtime phase names are neutral

- Status: active
- Owner: [Run lifecycle](../design/runtime-behavior.md#run-lifecycle)
- Fact: run phases and tool-call states are runtime concepts; product status names are adapter projections.
- Links: guardrails G2 and G10
- Verification: public API checks and runtime deny-list checks.

## FACT-RUNTIME-002: Durable runtime state appears only at commit

- Status: active
- Owner: [Runtime state model](../design/runtime-behavior.md#runtime-state-model)
- Fact: hooks and tools read snapshots and return `StateCommand`; applying a state patch updates only the live `StateStore` projection, while durable runtime truth appears through `ThreadCommit` / `CommitCoordinator`; ordinary effects are post-apply requests unless represented by a durable fact, outbox entry, or scheduled action.
- Links: guardrails G1 and G13
- Verification: staged command validation, atomic commit, persisted-state replay, and snapshot rebuild tests.

## FACT-RUNTIME-003: Plugin hooks use explicit extension seams

- Status: active
- Owner: [Plugins and hooks](../design/runtime-behavior.md#plugins-and-hooks)
- Fact: plugins can add tools, hooks, state-machine logic, gates, or sinks only through registered extension points and selected capability data.
- Links: guardrails G8, G9, and G14
- Verification: hook filter tests, no-bypass permission tests, and docs review checklist.

## FACT-RUNTIME-004: Scheduled work resumes through ingress

- Status: active
- Owner: [Scheduled and background work](../design/runtime-behavior.md#scheduled-and-background-work)
- Fact: scheduled actions, reminders, and deferred work are committed runtime requests (`ScheduledAction`) with neutral correlation keys/results; durable execution, wake, retry, and duplicate reconciliation stay in dispatch/server before resuming through ingress.
- Links: guardrails G5, G6, and G13
- Verification: committed request tests, correlation tests, durable wake tests, duplicate wake reconciliation, and ingress capability tests.

## FACT-RUNTIME-005: Eval and observability are projections

- Status: active
- Owner: [Observability, datasets, and eval](../design/runtime-behavior.md#observability-datasets-and-eval)
- Fact: traces, datasets, eval runs, and admin review consume committed runtime facts; they do not replace runtime truth.
- Links: guardrails G1 and G13
- Verification: trace projection and eval replay tests.

## FACT-RUNTIME-006: Runtime events are staged, committed, then projected

- Status: active
- Owner: [Runtime event model](../design/runtime-behavior.md#runtime-event-model)
- Fact: live `StreamEvent` values are best-effort output, while committed `EventRecord` values and `agent::fact::*` facts are the replayable source for downstream projection.
- Links: guardrails G1, G10, and G13
- Verification: event staging, projection ordering, sink failure, and replay tests.

## FACT-RUNTIME-007: State scopes split live projection from durable storage

- Status: active
- Owner: [Runtime state model](../design/runtime-behavior.md#runtime-state-model)
- Fact: `StateStore` is the live revisioned projection for an active run; durable storage uses `PersistedState` split by run-scoped and thread-scoped keys, while shared/product state stays behind approved ports.
- Links: guardrails G1, G8, and G13
- Verification: run/thread export-import tests, unknown-key policy tests, and product-resource boundary tests.

## FACT-RUNTIME-008: Run phase and thread checkpoint are separate axes

- Status: active
- Owner: [Run and thread lifecycle boundary](../design/runtime-behavior.md#run-and-thread-lifecycle-boundary)
- Fact: the durable run lifecycle value owns the execution phase on `RunRecord`, while `ThreadCommit` owns the thread checkpoint boundary for append-only messages, latest run projection, and optional thread-scoped state.
- Links: guardrails G1, G10, and G13
- Verification: lifecycle validation, append-fence, checkpoint atomicity, and resume snapshot consistency tests.

## FACT-RUNTIME-009: Call success is not commit visibility

- Status: active
- Owner: [Call timing and commit visibility](../design/runtime-behavior.md#call-timing-and-commit-visibility)
- Fact: hook, tool, and model call outputs are candidate state/event/effect values until a `ThreadCommit` succeeds through `CommitCoordinator`; replay and projection consume only committed records.
- Links: [commit taxonomy](../design/commit-fact-projection-taxonomy.md#runtime-call-staging-and-commit-layers); guardrails G1, G10, and G13
- Verification: staged command validation, durable event staging, sink failure, atomic commit, and replay tests.

## FACT-RUNTIME-010: Live state apply is not durable commit

- Status: active
- Owner: [Runtime state model](../design/runtime-behavior.md#runtime-state-model)
- Fact: applying a `MutationBatch` advances the active run's live `StateStore` projection, but durable runtime truth appears only after `ThreadCommit` succeeds through `CommitCoordinator`.
- Links: [commit taxonomy](../design/commit-fact-projection-taxonomy.md#runtime-call-staging-and-commit-layers); guardrails G1 and G13
- Verification: live-state revision tests, checkpoint atomicity tests, resume snapshot rebuild tests, and projection ordering tests.
