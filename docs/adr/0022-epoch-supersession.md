# ADR-0022: Epoch-Based Supersession on a Thread

- Status: Accepted
- Date: 2026-06-30
- Depends on: ADR-0009, ADR-0016

## Context

A thread is a conversation; sometimes a newer turn should *supersede* an older
one still queued or parked — the latest submission wins, and the stale one must
not run or resume. Durable cancel (ADR-0016) stops one named run; supersession is
the "newest-on-the-thread wins" generalization, named deferred there. The
reference keys this on a dispatch epoch.

## Decision

### D1: Supersession is opt-in, per submission, keyed on the thread

`SubmitOptions.supersede` (default false) asks the store to supersede prior live
work on the *same thread*. A normal submit never supersedes, so concurrent runs
on one thread remain legal; only a submission that asks for it wins over the
others. The thread is the supersession key because it is the stable conversational
unit (the same reason `send_message` addresses threads, ADR-0017).

### D2: A monotonic per-thread epoch orders submissions

Each dispatch carries an `epoch`. A superseding submit takes `max(epoch on the
thread) + 1`, so the newest submission always holds the highest epoch — a durable,
recoverable record of "which is newest," not a wall-clock guess.

### D3: Superseded is a terminal dispatch status, excluded from claim

Superseding marks the thread's prior **pending and parked** dispatches
`Superseded` — a terminal dispatch status, excluded from claim exactly like
dead-letter (ADR-0015). A superseded run is therefore never claimed, never woken,
never resumed; its queued or parked work is abandoned. `superseded()` lists them
for operations, the mirror of `dead_letters()`.

### D4: An in-flight running run is not force-superseded

Superseding touches pending and parked dispatches, not a *running* one: a run
mid-execution is stopped cooperatively through live cancel (ADR-0016), not by a
queue status flip under its feet. The superseded dispatch is abandoned (excluded
from claim) and visible via `superseded()`; committing a terminal `Cancelled`
fact for it through the durable-cancel boundary — so committed truth, not just the
queue, reflects it — and force-superseding a running run both remain deferred. An
operator can `cancel` a superseded run to make it committed-terminal today.

## Consequences

- A newer submission can abandon a thread's stale queued/parked work, by epoch,
  proven across the three backends against one shared spec.
- Supersession stays opt-in and thread-keyed; default submits are unaffected.
- Superseded dispatches are visible (`superseded()`) and excluded from claim, like
  dead-letters.
- Force-superseding an in-flight running run stays deferred (cooperative cancel).

## References

- [INVARIANTS.md](../INVARIANTS.md) — G5/G6 (durable ingress over runtime control).
- ADR-0009 — the dispatch claim bands a superseded row is excluded from.
- ADR-0016 — durable cancel, which this generalizes to "newest wins."
