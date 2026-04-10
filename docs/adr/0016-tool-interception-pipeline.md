# ADR-0016: Tool Interception Pipeline

- **Status**: Accepted
- **Date**: 2026-03-27
- **Depends on**: ADR-0006, ADR-0007

## Context

Tool execution needs extension points for permission checks, frontend tool delegation, and human-in-the-loop workflows. The original implementation modeled interception as a `BeforeToolExecute` scheduled action, but that blurred two responsibilities:

- pure "may this call proceed?" decisions
- one-shot execution-time hooks that should only run when a tool really executes

The runtime now separates these responsibilities. `ToolGateHook` provides the pure interception layer, while `BeforeToolExecute` is reserved for execution-time side effects.

## Decision

### D1: `ToolGateHook` with strict priority

Tool execution is intercepted during the `ToolGate` phase via `ToolGateHook`, which returns an optional `ToolInterceptPayload`. Three intercept payloads exist, each with a fixed priority:

| Action | Priority | Behavior |
|--------|----------|----------|
| Block | 3 (highest) | Terminates the run with a reason. Used for security denials. |
| Suspend | 2 | Pauses execution pending an external decision (permission prompt, frontend). |
| SetResult | 1 (lowest) | Skips tool execution and uses the provided result directly. |

### D2: Same-priority conflict resolution

When multiple gate hooks produce payloads at the same priority level, the first payload is kept and an error is logged. This is a conflict that indicates misconfiguration — two plugins should not both attempt to Block the same tool call.

### D3: Higher priority always wins

A higher-priority payload always overrides a lower-priority one. A Block from a permission plugin cannot be overridden by a SetResult from a frontend tool plugin. This ensures security-critical intercepts are never bypassed.

### D4: Resume semantics

Suspended tool calls are resumed via `ToolCallResume` carrying either a `Resume` or `Cancel` action. On replay, the runtime re-enters the normal tool pipeline with `resume_input` injected into the `ToolGate` / tool context. Gate hooks can use that resume context to proceed, block, or produce a result directly (for example, frontend tools return `SetResult`, while permission `Ask` resumes without re-suspending). On `Cancel`, the tool call is abandoned.

## Consequences

- Permission plugins use Block/Suspend; frontend tool plugins use SetResult. The priority system prevents lower-priority intercepts from overriding security decisions.
- Same-priority conflicts are logged as errors, surfacing misconfiguration without silent behavior changes.
- `ToolGate` remains pure and replay-safe, so the runtime can re-evaluate it after earlier tool calls commit state in the same step.
- `BeforeToolExecute` no longer participates in interception and keeps its run-once execution semantics.
- The pipeline is extensible: new intercept priorities can be added if needed, though three levels have proven sufficient.
