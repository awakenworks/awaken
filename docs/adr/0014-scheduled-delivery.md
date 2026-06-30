# ADR-0014: Scheduled Delivery via available_at

- Status: Accepted
- Date: 2026-06-30
- Depends on: ADR-0011, ADR-0013

## Context

`RunIngressCapabilities.scheduled_wake` was false: the durable queue stored no
timer, so a parked run could only be woken by an immediately-deliverable input.
The run-ingress design and the reference both call for delayed/scheduled
delivery (`available_at` in the reference).

## Decision

### D1: One nullable `available_at` on the pending input

`PendingInput` gains `available_at_ms: Option<u64>` — the earliest delivery
instant. `None` is deliverable immediately; a future value schedules the wake.
`claim` skips a pending input that is not yet due (`available_at IS NULL OR
available_at <= now`), both in its wake-eligibility test and in the input it
hands the worker. The daemon's poll re-checks as its clock advances, so the
delivery fires when due. With this, `scheduled_wake` is true.

### D2: The schedule is compared against the injected clock, not the database

`available_at_ms` is epoch milliseconds, compared against the same `now_ms` the
worker and daemon already carry (the `Clock` port, ADR-0011) — never against a
database `now()`. So the column is `BIGINT` epoch-millis, not a timezone-aware
timestamp: epoch-millis is the `Clock`'s native representation, is a
timezone-unambiguous absolute instant, and compares identically on Postgres
(`BIGINT`) and SQLite (`INTEGER`) — whereas a `{timestamptz}` renders to `TEXT`
on SQLite, where due-comparison would depend on string formatting. (The audit
`created_at` stays `{timestamptz}`: it is DB-managed, human-facing, and never
compared against the injected clock — the two kinds of time take two types.)

### D3: One field covers delayed direct and delayed cross-thread delivery

Because the outbox (ADR-0013) stages a `PendingInput`, a delayed cross-thread
send is just a staged input with a future `available_at`: relay moves it to the
target's pending input immediately, and the due-time gate at `claim` defers the
wake. No separate scheduling path is needed.

## Consequences

- A parked run can be woken on a durable schedule; the daemon fires a delayed
  delivery when its clock reaches the time, proven deterministically with a
  `ManualClock`.
- This is *delayed delivery of input* — the run-ingress waiting-ticket mechanism
  (ADR-0003 mechanism #2) plus a due-time gate. It is **not** `ScheduledAction`
  (ADR-0003 mechanism #1): a `ScheduledAction` is a committed in-run request to
  perform deferred work, recovered from committed state for consistency, not a
  queue column with a time. That mechanism remains unbuilt and stays a
  runtime-core concern, distinct from this dispatch-layer schedule.
- The deterministic `Clock` plus an epoch-millis column keeps scheduling testable
  and backend-uniform.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G5 (`RunIngressCapabilities`).
- ADR-0011 — the `Clock` port the schedule is compared against.
- ADR-0013 — the outbox a delayed cross-thread delivery reuses.
