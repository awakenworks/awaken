# ADR-0030: The Permission Policy Axis

- Status: Accepted
- Date: 2026-06-30
- Depends on: ADR-0016, ADR-0020

## Context

Permission is the *only* authorization path (G21): visibility, selection,
capability, and health narrow what is possible but never grant
(`permission-policy-axis.md`, G9). The ports existed — `ToolPermissionPolicy`,
`ToolPermissionVerdict`, and `ToolGateHook` — but nothing implemented them, so
authorization was a test stub. This builds the axis: a policy-backed gate, the
confirmation→resume loop, and committed audit. The concrete rule *policy*
(patterns and their unmatched default) is an extension, not runtime core.

## Decision

### D1: A policy-backed gate maps the decision to a gate outcome

`PermissionGate` is the `ToolGateHook` the engine calls per protected tool call.
It asks an injected `Arc<dyn ToolPermissionPolicy>` and maps the policy verdict
to a `GateOutcome`:

| `ToolPermissionVerdict` | `GateOutcome` | Runtime behavior |
|---|---|---|
| `Allow` | `Allow` | execute the tool |
| `Deny { reason }` | `Block { reason }` | feed a typed denied result to the model |
| `RequireConfirmation { correlation_id }` | `RequireConfirmation { correlation_id }` | await on a confirmation ticket |

The gate is the single authorization choke point; a tool never executes without a
decision passing through it (G21, no-bypass).

### D2: A confirmation wait reuses ResumeTicket

A `RequireConfirmation` verdict awaits on a `ResumeTicket` whose closed target is
`AwaitTarget::ToolCall { reason: ToolAwaitReason::Permission, call_id, tool }`
(ADR-0016's machinery), not a new ticket type. The operator's later allow/deny
arrives as
`ResumeResult::Permission(PermissionDecision::Allow | PermissionDecision::Deny)`,
validated against the committed ticket by the shared `ResumeValidator`; `Allow`
runs the committed pending tool, `Deny` does not. The
confirmation→approve→resume loop is the existing wait/resume capability, one
await reason among several.

### D3: Three policy verdicts, not five — set_result and require_scope fold in

`ToolPermissionVerdict` stays `Allow | Deny | RequireConfirmation` — the
irreducible policy outcomes (proceed / refuse / suspend-for-external-input). The
design's other two named decisions are not separate control flow:
- **set_result** (policy supplies a substitute result) is the gate's existing
  `GateOutcome::SetResult` — a result-transport detail at the gate, not a policy
  control-flow variant.
- **require_scope** (a credential grant is needed) is a `RequireConfirmation`
  verdict whose ticket waits for a grant rather than a yes/no, or a `Deny` for
  fail-fast — the same await/resume path, not a new variant.

Keeping the enum minimal avoids redundant authorization paths.

### D4: Audit is a committed event, never a side write

Each gate decision stages a `PermissionDecided` event (tool id, call id, decision,
`decided_by` — who decided — and, when an authored policy decided, the closed
`policy` it came from: `always_allow` | `always_ask` | `auto` | `rule`, plus for
`auto` the `reason_code`: `high_risk` on a refusal, `indeterminate` on an ask,
none on an allow) into the attempt's checkpoint, committed through the one finish
boundary with the run's other facts (G1).

`decided_by` is a closed vocabulary, one label per real decider, and every row
written now carries it:

| `decided_by` | who decided | `policy` / `reason_code` |
|---|---|---|
| `policy` | the authored permission policy (a disabled tool or a matched deny rule is still the authored `rule`) | present / `auto` grounds only |
| `run_narrowing` | a per-Run narrowing of the configured authority (a tool-free Run, a per-Run `DenyAllTools`) | absent |
| `plugin_gate` | a tool gate that is not an authored policy (a state-machine precondition, the skill `allowed_tools` intersection) | absent |

The producer side is one closed `GateAuthority` (`Policy(PermissionBasis)` |
`RunNarrowing` | `PluginGate`) on every `GateDecision`, and a
`RulingAuthority` (`Policy` | `RunNarrowing`) on every `ToolPermissionRuling`;
there is no unattributed constructor, and all three audit fields are lowered
from that one value, so they cannot disagree. An allow a later gate merely
passes through keeps the earlier authority. There is no "absent gate" label:
with neither a host gate nor a per-Run policy installed nothing is audited.

`reason_code` is an open registry, as the SDK declares it: a reader keeps a code
it has no name for rather than failing or dropping it. `decided_by`, `policy` and
`reason_code` are additive: rows written before they existed still decode (a row
without `decided_by` is read as unattributed), and an unknown `decided_by` or
`policy` label is refused by the typed decoder rather than read as another
member. Managed projects `policy`/`reason_code` as the SDK's per-call
`evaluation` beside the flat `evaluated_permission`; the SDK object has only
policy vocabulary, so a row whose `decided_by` is not `policy` projects the flat
decision alone and no wire field is invented for its decider. Authorization is therefore explainable from committed
truth, not a durable write outside the commit path. No gate means no protected
call and no audit.

### D5: Approval and tool execution state share the ThreadCommit boundary

An approval wait is represented twice inside the same runtime-truth aggregate:

- `RunDisposition::Awaiting(ResumeTicket)` with an
  `AwaitTarget::ToolCall { reason: ToolAwaitReason::Permission, ... }` owns the
  resumable Run lifecycle;
- the Run-scoped `ActiveToolBatch` state cell owns the corresponding tool call as
  `ToolCallPhase::Awaiting { wait: ToolWait { kind:
  ToolWaitKind::ToolPermission, correlation_id } }`.

Both are state carried by `ThreadCommit`; there is no `ToolStore`, tool table, or
second repository. A permission ticket accepts only
`ResumeResult::Permission(PermissionDecision)`; free-form input or a tool result
cannot approve a tool. Before an approved tool is entered, one commit atomically
clears the ticket, moves the Run to `Running`, moves the call to
`Executing { attempt }`, and records `PermissionConfirmed { result: allow }`.
Failure or fencing of that commit prevents executor entry. Denial completes the
call with a blocked result and records `PermissionConfirmed { result: deny }`,
without entering the executor. The person's answer is its own fact, not a second
`PermissionDecided`: the decision states what the gate concluded on which basis
(D4), and the confirmation states how the wait that decision opened was answered.

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
- The policy-verdict enum is three-valued; richer needs map onto the gate's
  `SetResult` or confirmation ticket, not new variants.
- The concrete rule policy (Claude-Code-style patterns) lives in
  `awaken-ext-permission`; the runtime owns only the gate, the ticket reuse, and
  the audit.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G1, G5, G9, G21, G26.
- [permission-policy-axis.md](../design/permission-policy-axis.md) — the axis spec.
- ADR-0016 — the resume-ticket/resume machinery this confirmation wait reuses.
