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

**Stop conditions are plugins**: Implemented as regular plugins with AfterInference hooks, not hardcoded loop logic. Built-in conditions (MaxRounds, Timeout, TokenBudget, etc.) provided as a default plugin bundle. Termination flows through `RuntimeEffect::Terminate` — the agent loop's effect handler sets a flag checked at phase boundaries. Effects (not actions) because termination is control flow, not state mutation.

**Configuration is boundary-resolved**: Run/step lifecycle boundaries are also configuration resolution boundaries (ADR-0004). In particular, `run_phase`, `BeforeInference`, and `BeforeToolExecute` re-resolve live config sources into an execution-local view so handoff, profile switching, and request-level overrides take effect without rebuilding the whole runtime.

## Implementation Status

- Phase enum: 6 of 8 variants implemented (missing StepStart, StepEnd)
- `RuntimeEffect::Terminate`: implemented
- Run state machine: not implemented
- Stop conditions as plugins: not implemented
- Boundary-resolved configuration: not implemented (depends on ADR-0004)

## Consequences

- Phase enum expansion requires updating existing code
- Stop conditions are replaceable/extensible without modifying the loop
- Run lifecycle state is observable via standard state reads
