# ADR-0020: ScheduledAction — Committed In-Run Deferred Work

- Status: Accepted
- Date: 2026-06-30
- Depends on: ADR-0003, ADR-0005, ADR-0009

## Context

ADR-0003 names three deferred-work mechanisms. Two are built: the waiting
ticket / resume (mechanism #2) and durable run dispatch (mechanism #3).
`ScheduledAction` (mechanism #1) — defer an action to a later phase *within the
same run*, recovered from committed state for consistency — was unimplemented:
no type, no engine path. It is not a timer; a `deadline` is one of its fields,
but its essence is a **committed request** that guarantees the deferred action is
performed exactly once and never resumed from an uncommitted candidate
(RS-SCH-003/007).

## Decision

### D1: A ScheduledAction is a committed `ResumeTicket`, by reason

A scheduled action and an aawaiting run already share the same committed identity —
correlation/idempotency key, run/thread binding, snapshot + catalog fingerprint,
deadline — and `ResumeTicket` already carries a `pending_tool` (an action with
arguments). So a ScheduledAction is a `ResumeTicket` with
`reason = AwaitReason::ScheduledAction` whose `pending_tool` is the deferred
action. It commits in `RunDisposition::Awaiting` and awaits the run in `RunState::Awaiting`
through the one finish boundary (G31), exactly like any other await. No new commit
field or state machine is added — the difference is the *reason* (who performs the
result) and nothing else.

### D2: A gate stages it; commit-before-wake makes it recoverable

A hook/gate decides a tool call should be *scheduled* rather than run now, via
`GateOutcome::Schedule { correlation_id }` (the sibling of `Suspend`). The engine
awaits with a ScheduledAction ticket carrying that call. The request is durable
only once `ThreadCommit` succeeds: an uncommitted scheduled-action candidate is
never wakeable and never recovered (RS-SCH-003/007), because recovery reads the
committed ticket, never an in-flight one.

### D3: Performing it is an allow-resume of the committed action

The difference between `ToolPermission` and `ScheduledAction` is who supplies the
result: a human *decides* a permission; the system *performs* a scheduled action.
Performing is therefore an `allow` resume of the committed pending action —
`Runtime::perform_scheduled_action` reads the committed ticket, and if its reason
is `ScheduledAction`, runs the pending action and commits the resumed outcome,
validated against the committed request (correlation, run/thread, snapshot,
fingerprint, deadline) and idempotent (RS-SCH-001/004). A result for an unknown
or mismatched correlation is rejected before it touches committed facts
(RS-SCH-004). Cancel/stop make the run terminal and clear the ticket, so a late
scheduled result is rejected (`NotWaiting`) without mutating facts (RS-CTRL-001/002).

### D4: In-process by default, dispatch-driven for durability

The normal path is in-process: the runtime performs the scheduled action and
continues. For crash recovery, the committed ScheduledAction ticket is wakeable
work the dispatch (ADR-0009) drives — an aawaiting run whose ticket reason is
`ScheduledAction` is performed by the daemon, not waited on for external input.
Action-kind capability bounds (RS-SCH-005) ride on the pending action's resolved
tool, already bound by the run's `ResolvedExecutionEnv` (ADR-0004); a richer
plugin-owned action-kind axis is deferred.

## Consequences

- The third deferred-work mechanism exists, completing the ADR-0003 triad, with no
  new commit field or `BackgroundTask` umbrella — it is a `ResumeTicket` reason.
- Consistency holds: the action is performed only from a committed request, never
  a candidate; a late result after cancel is rejected.
- Scenarios RS-SCH-001/003/004 and RS-CTRL-001 are covered by runtime tests; the
  daemon driver covers the recovery path.
- A distinct plugin-owned action-kind type with its own capability axis, and a
  non-tool scheduled action, remain deferred.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G5, G13, G28, G31.
- ADR-0003 — the three deferred-work mechanisms; this is #1.
- ADR-0005 — the one finish boundary a scheduled await commits through.
- [runtime-scenario-validation.md](../design/runtime-scenario-validation.md) —
  RS-SCH-001..007, RS-CTRL-001/002.
