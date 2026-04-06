# ADR-0007: Tool Call Lifecycle

- **Status**: Implemented
- **Date**: 2026-03-21
- **Depends on**: ADR-0001, ADR-0008

## Context

Tool calls are the primary agent action mechanism. They can succeed, fail, or suspend (awaiting user approval). Need to define the lifecycle, suspension mechanics, and execution strategy.

## Decision

**ToolCall state machine**:

```
Pending → Running → Succeeded / Failed
                  → Suspended → Resuming → Running (re-execute)
                               → Cancelled
```

Implemented as `StateKey`s with ToolCall scope (namespaced per `call_id`).

**Suspension is first-class**: Not an error path. A tool or `BeforeToolExecute` hook can suspend a call. The run transitions to `Waiting` only after the current step reaches quiescence with no runnable work left and at least one suspended call. Suspended state persists; external decisions arrive asynchronously; the agent loop replays them at the next wait boundary. There is no separate run-level `Running+Waiting` state today. Three resume modes: ReplayToolCall (re-execute with decision context), UseDecisionAsResult (decision payload becomes result), PassDecisionToTool (decision as new arguments).

Each suspended tool call persists both internal and external resume identity:

- `call_id` remains the internal runtime identifier
- `suspension_id` is the current external-facing suspension key
- `suspension_reason` describes why this suspension was created
- `resume_input` stores the latest applied decision payload

Resume requests may target either `call_id` or `suspension_id`; both resolve to
the same tool-call state.

**Three execution modes** (per-agent config):

| Mode | Behavior | Suspension handling |
|------|----------|-------------------|
| Sequential | One at a time | Immediate replay |
| Parallel | All concurrently | Batch: wait for all decisions |
| ParallelStreaming | All concurrently | Immediate per-decision |

## Consequences

- Suspension requires persistent ToolCall-scoped state (ADR-0008)
- Parallel modes require state merge strategy (ADR-0008)
- The server/client layer provides external decision transport
