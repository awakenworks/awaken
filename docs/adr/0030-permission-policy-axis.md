# ADR-0030: The Permission Policy Axis

- Status: Accepted
- Date: 2026-06-30
- Depends on: ADR-0016, ADR-0020

## Context

Permission is the *only* authorization path (G21): visibility, selection,
capability, and health narrow what is possible but never grant
(`permission-policy-axis.md`, G9). The ports existed — `PermissionPolicy`,
`PermissionContext`, `PermissionDecision`, `ToolGateHook` — but nothing
implemented them, so authorization was a test stub. This builds the axis: a
policy-backed gate, the ask→resume loop, and committed audit. The concrete rule
*policy* (patterns, modes) is an extension, not runtime core.

## Decision

### D1: A policy-backed gate maps the decision to a gate outcome

`PermissionGate` is the `ToolGateHook` the engine calls per protected tool call.
It asks an injected `Arc<dyn PermissionPolicy>` and maps the decision to a
`GateOutcome`:

| `PermissionDecision` | `GateOutcome` | Runtime behavior |
|---|---|---|
| `Allow` | `Allow` | execute the tool |
| `Deny { reason }` | `Block { reason }` | feed a typed denied result to the model |
| `Ask { ticket_id }` | `Suspend { ticket_id }` | await on a decision ticket |

The gate is the single authorization choke point; a tool never executes without a
decision passing through it (G21, no-bypass).

### D2: A DecisionTicket is a ResumeTicket, reused

An `ask` awaits on a `ResumeTicket` with `reason = ToolPermission` (ADR-0016's
machinery) — the design's *DecisionTicket* is that ticket, not a new type. The
operator's later allow/deny arrives as a `ResumeResult::Decision`, validated
against the committed ticket by the shared `ResumeValidator`; `allow` runs the
committed pending tool, `deny` does not. The ask→approve→resume loop is the
existing wait/resume capability, one await reason among several.

### D3: Three decisions, not five — set_result and require_scope fold in

`PermissionDecision` stays `Allow | Deny | Ask` — the irreducible outcomes
(proceed / refuse / suspend-for-external-input). The design's other two named
decisions are not separate control flow:
- **set_result** (policy supplies a substitute result) is the gate's existing
  `GateOutcome::SetResult` — a result-transport detail at the gate, not a policy
  control-flow variant.
- **require_scope** (a credential grant is needed) is an `ask` whose ticket waits
  for a grant rather than a yes/no, or a `deny` for fail-fast — the same
  await/resume, a different reason. No new variant.

Keeping the enum minimal avoids redundant authorization paths.

### D4: Audit is a committed event, never a side write

Each gate decision stages a `PermissionDecided` event (tool id, call id, decision)
into the attempt's checkpoint, committed through the one finish boundary with the
run's other facts (G1). Authorization is therefore explainable from committed
truth, not a durable write outside the commit path. No gate means no protected
call and no audit.

### D5: Approval and tool execution state share the ThreadCommit boundary

An approval wait is represented twice inside the same runtime-truth aggregate:

- `RunDisposition::Awaiting(ResumeTicket { reason: ToolPermission, ... })` owns
  the resumable Run lifecycle;
- the Run-scoped `ActiveToolBatch` state cell owns the corresponding tool call as
  `Awaiting { kind: Approval, correlation_id }`.

Both are state carried by `ThreadCommit`; there is no `ToolStore`, tool table, or
second repository. A resume accepts only `ResumeResult::Decision` or a correlated
`ToolResult`; free-form input cannot approve a tool. Before an approved tool is
entered, one commit atomically clears the ticket, moves the Run to `Running`, moves
the call to `Executing { attempt }`, and records `PermissionDecided=approved`.
Failure or fencing of that commit prevents executor entry. Denial completes the
call with a blocked result and records `PermissionDecided=denied`, without entering
the executor.

This is intentionally Run-scoped state stored in the thread commit log, not
Thread-scoped domain state: the embedded `run_id` prevents an older Run's active
batch from being rehydrated into a later Run on the same thread.

## Consequences

- Authorization is real and uniform: every protected tool call passes a
  policy-backed gate, and the decision is committed for review.
- The ask path reuses the resume-ticket/resume machinery unchanged.
- Approval is a typed lifecycle command, never an ordinary agent message; its
  decision and tool transition are durable before any external effect.
- Tool-call recovery adds no persistence authority: all durable state remains in
  the existing `ThreadCommit` aggregate.
- The decision enum is three-valued; richer needs map onto the gate's SetResult or
  the ask ticket, not new variants.
- The concrete rule policy (Claude-Code-style patterns and modes) lives in
  `awaken-ext-permission`; the runtime owns only the gate, the ticket reuse, and
  the audit.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G1, G5, G9, G21, G26.
- [permission-policy-axis.md](../design/permission-policy-axis.md) — the axis spec.
- ADR-0016 — the resume-ticket/resume a DecisionTicket reuses.
