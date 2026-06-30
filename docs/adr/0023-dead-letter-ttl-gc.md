# ADR-0023: Time-Windowed Dead-Letter GC

- Status: Accepted
- Date: 2026-06-30
- Depends on: ADR-0015, ADR-0018

## Context

Dead-lettered dispatches are retained for visibility and requeue (ADR-0015), and
`purge_dead_letters` (ADR-0018) lets an operator reclaim them all at once. A
long-running fleet wants the daemon to reclaim *aged* dead-letters automatically,
while keeping recent ones around long enough for an operator to notice — a
time-windowed GC, the auto-GC named deferred in ADR-0018.

## Decision

### D1: Reap stamps the dead-letter time in epoch ms

`reap` already runs against the injected clock; when it dead-letters a run it now
records `dead_lettered_at = now_ms`. Epoch ms, not a wall-clock timestamp, keeps
the column comparable to the daemon's `Clock` and uniform across backends — the
same machine-time convention as the lease and schedule columns (ADR-0014).

### D2: GC is by dead-letter time, not enqueue time

`purge_dead_letters_before(cutoff_ms)` removes dead-letters whose
`dead_lettered_at <= cutoff_ms` (and their pending input). The window is measured
from when the run was dead-lettered, not when it was enqueued, so a run that
retried for a long time before dying still gets its full grace period. A row with
no `dead_lettered_at` is never aged out — only the unconditional
`purge_dead_letters` removes those.

### D3: The daemon GCs on its existing cadence, opt-in

`DispatchServiceConfig.dead_letter_ttl` is `None` by default (retain until an
operator purges). When set, each daemon tick computes
`cutoff = now - ttl` and calls `purge_dead_letters_before(cutoff)` — no new timer
or task, just one more step in the existing reap/relay/drain loop. A store error
is swallowed like the loop's other steps; the next tick retries.

## Consequences

- A fleet reclaims aged dead-letter storage automatically, with a configurable
  grace window; recent failures stay visible for triage.
- Opt-in: the default daemon behaviour (retain dead-letters) is unchanged.
- Proven across the three backends against one shared spec (younger spared, aged
  purged).
- Auto-GC of other terminal states is unneeded: done dispatches are removed on
  settle; only dead-letters are retained.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G5/G6 (durable ingress over runtime control).
- ADR-0014 — the epoch-ms machine-time convention.
- ADR-0015 — the dead-letter state this GC ages out.
- ADR-0018 — the operator `purge_dead_letters` this complements.
