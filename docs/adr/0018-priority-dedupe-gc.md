# ADR-0018: Dispatch Priority, Dedupe Key, and Dead-Letter GC

- Status: Accepted
- Date: 2026-06-30
- Depends on: ADR-0009, ADR-0015

## Context

Three operational gaps remained in the dispatch queue: all fresh work was claimed
in enqueue order (no priority), a caller had no idempotency beyond the run id (no
dedupe), and dead-lettered rows accumulated with no way to reclaim their storage
(no GC). The reference carries `priority`, `dedupe_key`, and a `purge_terminal`
GC; this adds the same, minimally.

## Decision

### D1: One options value, with a default-preserving `enqueue`

`enqueue_with(request, SubmitOptions { priority, dedupe_key })` carries the new
metadata; `enqueue(request)` stays as a provided trait method that calls
`enqueue_with` with defaults, so every existing caller is unchanged. `priority`
defaults to 0 and `dedupe_key` to none.

### D2: Priority orders only fresh work; recovery and wake do not change

The fresh band claims the highest-priority pending run first (ties stay FIFO by
enqueue order). Recovery (expired lease) and wake (pending input due) keep their
own ordering — failure and readiness, not caller priority — because reordering
them would starve crashed or awaiting runs.

### D3: Dedupe is a live-key no-op

A `dedupe_key` makes `enqueue_with` a no-op while a non-dead-letter dispatch
already carries that key — idempotency for an at-least-once producer that may
submit the same logical work under different run ids. A `NULL` key never matches.
The key is cleared when the run finishes (the row is removed), so dedupe scopes to
in-flight work, not history.

### D4: GC is an explicit operator action, not silent

`purge_dead_letters()` removes every dead-lettered dispatch and its pending input
and returns the count. It is operator/maintenance-triggered, not an automatic
sweep, so a poison run stays visible (`dead_letters`/`requeue`, ADR-0015) until an
operator decides to reclaim it. Done dispatches are already deleted on settle, so
they need no GC.

## Consequences

- High-priority runs start ahead of a backlog; equal priorities stay fair (FIFO).
- A producer can dedupe concurrent submissions with a stable key.
- Dead-letter storage is reclaimable on demand, proven across the three backends
  against one shared spec.
- Time-windowed (ttl) auto-GC and per-run lease renewal remain deferred.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G5/G6 (durable ingress over runtime control).
- ADR-0009 — the dispatch queue and claim bands.
- ADR-0015 — the dead-letter state this GC reclaims.
