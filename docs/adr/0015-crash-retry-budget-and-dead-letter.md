# ADR-0015: A Crash-Retry Budget and Dead-Letter

- Status: Accepted
- Date: 2026-06-30
- Depends on: ADR-0009, ADR-0011

## Context

The daemon recovers a crashed run by reclaiming its expired lease and re-running
it (ADR-0011). Nothing bounded that: a *poison* run — one whose execution kills
the process every attempt — would be reclaimed on every poll, forever, across
restarts. That is the one unbounded loop left after crash-recovery and idempotent
consumption landed. The reference (`awaken-worktrees/goal`) bounds it with
`attempt_count` / `max_attempts` and a `DeadLetter` state.

## Decision

### D1: The budget counts only crash-recoveries, and a checkpoint refreshes it

The dispatch row carries `attempt_count`. It increments **only** on a recovery
re-claim (an expired-lease running row), never on a fresh claim or a normal
park/wake — so a long-lived run that legitimately parks and wakes many times
never spends the budget. A successful `settle(Parked)` resets it to zero: a run
that reaches a checkpoint earned a fresh budget. So `attempt_count` is the count
of *consecutive crashes without progress*, which is exactly what a retry budget
should bound.

### D2: A separate reap step dead-letters, kept out of claim

A `reap(max_attempts, now)` operation moves every expired-lease running row with
`attempt_count >= max_attempts` to a `DeadLetter` status. It is one statement per
backend, run by the daemon each tick *before* draining — so claim never sees a
dead-lettered row and never needs over-budget logic in its hot path. A
dead-lettered run is retained (not deleted) for operations: `dead_letters()`
lists them and `requeue()` returns one to the queue at a fresh budget.

## Consequences

- A poison run is dead-lettered after `max_attempts` crash-recoveries instead of
  looping forever; `DispatchServiceConfig.max_attempts` (default 5) tunes it.
- The budget is spent only by crashes, not by ordinary parking, so HITL or
  long-running runs are not penalised.
- Dead-letter is a held state with `dead_letters`/`requeue` ops, proven across
  the memory, Postgres, and SQLite backends against one shared spec.
- A run that *returns* a terminal error still settles `Done` immediately (no
  retry): the budget is for crashes that leave the dispatch unsettled, not for
  runs that fail cleanly.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G5/G6 (durable ingress over runtime control).
- ADR-0011 — the recovery this bounds.
- [run-ingress-message-delivery.md](../design/run-ingress-message-delivery.md) —
  the dispatch failure/recovery boundary.
