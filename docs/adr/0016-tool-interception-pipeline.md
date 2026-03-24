# ADR-0016: Tool Interception Pipeline

- **Status**: Accepted
- **Date**: 2026-03-27
- **Depends on**: ADR-0006, ADR-0007

## Context

Tool execution needs extension points for permission checks, frontend tool delegation, and human-in-the-loop workflows. Hooks in the `BeforeToolExecute` phase can intercept tool calls, but multiple interceptors may conflict. A deterministic priority system is needed to resolve conflicts without ambiguity.

## Decision

### D1: ToolInterceptAction with strict priority

Tool execution can be intercepted during the `BeforeToolExecute` phase via `ToolInterceptAction`. Three intercept types exist, each with a fixed priority:

| Action | Priority | Behavior |
|--------|----------|----------|
| Block | 3 (highest) | Terminates the run with a reason. Used for security denials. |
| Suspend | 2 | Pauses execution pending an external decision (permission prompt, frontend). |
| SetResult | 1 (lowest) | Skips tool execution and uses the provided result directly. |

### D2: Same-priority conflict resolution

When multiple interceptors produce actions at the same priority level, the first interceptor's action is kept and an error is logged. This is a conflict that indicates misconfiguration — two plugins should not both attempt to Block the same tool call.

### D3: Higher priority always wins

A higher-priority action always overrides a lower-priority one. A Block from a permission plugin cannot be overridden by a SetResult from a frontend tool plugin. This ensures security-critical intercepts are never bypassed.

### D4: Resume semantics

Suspended tool calls are resumed via `ToolCallResume` carrying either a `Resume` or `Cancel` action. On `Resume`, the permission hook that originally produced the Suspend skips re-evaluation — the external decision is authoritative. On `Cancel`, the tool call is abandoned.

## Consequences

- Permission plugins use Block/Suspend; frontend tool plugins use SetResult. The priority system prevents lower-priority intercepts from overriding security decisions.
- Same-priority conflicts are logged as errors, surfacing misconfiguration without silent behavior changes.
- Resume flow avoids redundant permission re-evaluation, preventing infinite suspend loops.
- The pipeline is extensible: new intercept priorities can be added if needed, though three levels have proven sufficient.
