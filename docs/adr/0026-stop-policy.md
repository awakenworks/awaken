# ADR-0026: Stop Policy — A Terminal Stop Reason

- Status: Accepted
- Date: 2026-06-30
- Depends on: ADR-0005, ADR-0016

## Context

A host may need to stop a run for a policy reason — a token/time budget, a step
ceiling, an operator halt — distinct from an external *cancel*. The deferred-work
scenarios require that once such a stop is committed, a later resume or scheduled
result for that run fails closed (RS-CTRL-002), the same closure cancel provides
(RS-CTRL-001), but recorded as a *stop*, not a cancel.

## Decision

### D1: `Stopped(reason)` is a distinct terminal cause

`EndCause::Stopped(String)` records a policy stop with its reason, separate from
`Cancelled` (external) and `Error` (fault). The reason is committed data, so an
operator or audit can see *why* the run stopped (e.g. "budget exhausted"), not
just that it did.

### D2: Stop commits through the one finish boundary

`Runtime::stop_run(run_id, thread_id, reason, context)` commits a terminal
`Stopped` fact through the single finish boundary (G31), exactly as `cancel_run`
commits `Cancelled` (ADR-0016). It clears any waiting ticket, so the run is no
longer resumable. No second termination path is added — a stop is the same
boundary with a different cause.

### D3: A late result after a stop fails closed

Because the stop clears the waiting ticket, a resume or `perform_scheduled_action`
arriving afterward finds no ticket and is rejected (`not waiting`) before it can
run an action or commit a message (RS-CTRL-002). The terminal stop is the single
authority; a stale deferred result never resurrects the run.

### D4: The policy itself is the host's, not the runtime's

`stop_run` is the *mechanism*; deciding *when* to stop (which budget, which
ceiling) is a host policy that calls it. The runtime owns the terminal-commit and
fail-closed semantics, not the policy thresholds — the same separation as the
permission gate (the runtime enforces the decision; the host supplies the policy).

## Consequences

- A host can stop a run for a recorded policy reason, and a late deferred result
  for it is rejected without mutating committed facts.
- One more terminal cause, one more commit through the existing finish boundary —
  no new termination machinery.
- The MaxSteps cause already existed for the loop's own ceiling; `Stopped` is for
  a host-imposed policy stop, decided outside the loop.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G13/G31 (single-source commit, one finish).
- ADR-0005 — the one finish boundary a stop commits through.
- ADR-0016 — durable cancel, the sibling terminal-commit this mirrors.
