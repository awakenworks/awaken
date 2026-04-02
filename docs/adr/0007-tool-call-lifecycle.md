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
                  → Suspended → Resumed → Running (re-execute)
                               → Cancelled
```

Implemented as `StateKey`s with ToolCall scope (namespaced per `call_id`).

**Suspension is first-class**: Not an error path. A tool or `BeforeToolExecute` hook can suspend a call. Run transitions to Waiting; suspended state persists; external decision arrives asynchronously; agent loop replays at next step boundary. Three resume modes: ReplayToolCall (re-execute with decision context), UseDecisionAsResult (decision payload becomes result), PassDecisionToTool (decision as new arguments).

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
