# ADR-0025: The Dispatch Query Surface

- Status: Accepted
- Date: 2026-06-30
- Depends on: ADR-0009

## Context

`RunDispatch` exposed targeted operational reads — `dead_letters`, `superseded` —
but no general view of the queue. Monitoring and maintenance (how many runs are
parked, which are running, attempt counts) had no port. The design named a
`RunDispatch*` query/lifecycle role; this builds the read half.

## Decision

### D1: A read-only summary, no live handle

`list_dispatches()` returns a `DispatchSummary` per row — `run_id`, `thread_id`,
`status`, `attempt_count` — in enqueue order. It is committed queue state as plain
data, never a live handle or runtime truth (the run's *outcome* is the commit
coordinator's `RunFact`, not this; ADR-0009 keeps run-outcome truth out of the
dispatch aggregate). So a summary can be logged, rendered, and compared freely.

### D2: A public `DispatchStatus`, mapped from each backend

`DispatchStatus` is the public lifecycle enum (Pending, Running, Parked,
DeadLetter, Superseded). The memory store maps its internal status to it; the SQL
stores map their `status` text via `DispatchStatus::from_db`. The public enum is
the one vocabulary every backend reports, so an operator reads the same states
regardless of storage.

### D3: Query only; lifecycle mutation stays in the existing verbs

This adds reads, not new mutations: dead-letter, requeue, purge, cancel, and
supersede already cover the lifecycle. The "lifecycle" half of the named role is
those existing verbs; `list_dispatches` is the missing observability over them.

## Consequences

- Monitoring and maintenance have one backend-uniform view of the queue, proven
  across the three backends against one shared spec.
- The query carries no live handle and no run-outcome truth, so it cannot become a
  second source of run state.
- Richer filtered/paged queries can extend this surface without changing the
  lifecycle verbs.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G6 (dispatch is operational, not run truth).
- ADR-0009 — the dispatch aggregate this observes.
