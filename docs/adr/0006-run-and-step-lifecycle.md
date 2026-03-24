# ADR-0006: Run and Step Lifecycle

- **Status**: Partially Implemented
- **Date**: 2026-03-21
- **Depends on**: ADR-0001, ADR-0004, ADR-0007

## Context

The agent loop drives inference → tool execution → state cycles on top of the phase system (ADR-0001). Need to define what a Run and Step are, how they map to phases, and how the loop terminates.

## Decision

**Step = one inference + tool execution cycle**. A Run consists of N steps:

```
RunStart → [StepStart → BeforeInference → [inference] → AfterInference
            → (if tools) BeforeToolExecute → [execution] → AfterToolExecute
            → StepEnd] × N → RunEnd
```

Phase enum expands from 6 to 8 (+`StepStart`, +`StepEnd`). StepStart runs even when inference is skipped (decision replay). StepEnd runs even without tool calls.

**Run state machine**:

```
Running ←→ Waiting → Done
```

- Running: actively executing steps
- Waiting: suspended, awaiting external decisions
- Done: terminal, with reason (NaturalEnd / Stopped / BehaviorRequested / Cancelled / Error)

Implemented as a built-in `StateKey` with Run scope.

**Stop conditions are plugins**: Implemented as regular plugins with AfterInference hooks, not hardcoded loop logic. Built-in conditions (MaxRounds, Timeout, TokenBudget, etc.) provided as a default plugin bundle. Termination flows through the `RunLifecycle` state machine — stop condition hooks write `RunLifecycleUpdate::Done` directly via `StateCommand`, and the loop runner checks `RunStatus` at phase boundaries. This eliminates the need for `RuntimeEffect::Terminate` and the `TerminationFlag` side channel; all lifecycle transitions are state-driven.

**Configuration is resolved at run start and re-resolved on handoff**: `AgentSpec` is resolved once at run start. Re-resolution occurs at step boundaries only when `ActiveAgentIdKey` indicates a handoff has changed the active agent. This means configuration is stable within a step and only changes between steps when an agent switch occurs. See ADR-0014 for details.

## Implementation Status

- Phase enum: implemented (8 variants)
- Run state machine (`RunLifecycle` StateKey): implemented
- Stop conditions as plugins: implemented (MaxRounds, TokenBudget, Timeout, ConsecutiveErrors)
- State-driven termination: hooks write `RunLifecycleUpdate::Done`, loop checks `RunStatus`
- `RuntimeEffect::Terminate` / `TerminationFlag`: removed (replaced by state machine)
- Configuration resolved at run start, re-resolved on handoff at step boundaries (see ADR-0014)

## Consequences

- Phase enum expansion requires updating existing code
- Stop conditions are replaceable/extensible without modifying the loop
- Run lifecycle state is observable via standard state reads
