# ADR-0016: Durable Cancel of a Not-Running Run

- Status: Accepted
- Date: 2026-06-30
- Depends on: ADR-0005, ADR-0009

## Context

`RunIngress.cancel` cancels an *in-flight* run cooperatively through
`LiveRunControl` — it signals the run's cancellation token, which the loop
observes at a step boundary and commits `Cancelled`. But a *queued* (pending) or
*parked* (waiting) run is not executing, so it holds no live token: cancelling it
needs a durable path. Without one, a parked run's committed `Waiting` fact would
dangle forever after its dispatch was dropped.

## Decision

### D1: The dispatch layer removes; the runtime commits the terminal

`RunDispatch.cancel(run_id)` removes a *pending or parked* dispatch and its
pending input, and returns the run's thread id; a *running* dispatch is left
alone (returns `None` — use live cancel) and so is a dead-lettered or unknown
run. The dispatch store owns only delivery state, so it does not commit run
truth.

The terminal `Cancelled` fact is committed by the runtime, through a new
`Runtime::cancel_run(run_id, thread_id, context)` that funnels through the single
`finish` boundary (G31) with a cancelled checkpoint — the same boundary every run
end uses. It clears any waiting ticket, so a parked run can no longer be resumed.

`DurableRunIngress::cancel_durable` composes the two: remove the dispatch, then
commit `Cancelled`. A queued run that never executed still gets a committed
`Cancelled` (its only fact), so its phase is always defined after a cancel.

### D2: Supersession is cancel-plus-resubmit, not a new epoch axis

The reference adds a dispatch *epoch* to supersede stale queued work for a
thread. That is deferred: durable cancel already expresses "stop this run," and
re-submitting under a new run id replaces it. An epoch/version axis is only
warranted once interrupt-all-older semantics are a real requirement.

## Consequences

- A queued or parked run can be cancelled durably, with its committed phase
  ending `Cancelled` and any waiting ticket cleared — no dangling parked run.
- In-flight cancel is unchanged (live control); the two paths are cleanly split
  by whether the run is currently executing.
- `Runtime::cancel_run` reuses the one finish boundary, so a cancelled run is
  indistinguishable from any other terminal in committed truth (ADR-0005).
- Epoch-based supersession remains a named, deferred item.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G31 (one finish boundary; one stored end).
- ADR-0005 — the committed `Phase`/`EndCause` terminal authority.
- ADR-0009 — the dispatch store and live-vs-durable ingress split.
