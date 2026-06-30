# ADR-0013: Pending Lifecycle Operations and a Cross-Thread Outbox

- Status: Accepted
- Date: 2026-06-30
- Depends on: ADR-0009, ADR-0010

## Context

The durable ingress slice (ADR-0009) shipped pending input as append-only,
delivered to a parked run's ticket correlation (ADR-0010). Two pieces of the
run-ingress design remained: the thread-message *operations* over pending input
(edit, retract, reorder under a revision check) and *cross-thread* message
delivery (a sender on one thread delivering input to another thread, via outbox
plus idempotent target append, never two-phase commit).

The reference (`awaken-worktrees/goal`) realizes these with a `PendingInputStore`
(load/append/update/retract/reorder) carrying a `delivery_mode` (boundary,
granularity, targeted_run, barrier) and a `durable_message_sink` for cross-thread
delivery.

## Decision

### D1: Revision-guarded edit and retract; no reorder, no delivery_mode

Pending records carry a store-assigned `revision`. `PendingInbox` gains `list`,
`retract(expected_revision)`, and `edit(expected_revision, result)` — optimistic
concurrency that fails closed on a stale revision, so a concurrent change is
never silently overwritten. These are the thread-message operations surface, not
`RunIngress` routes.

**Reorder and `delivery_mode` are deliberately not copied.** Delivery here is
keyed by ticket correlation (ADR-0010), not by arrival order or a boundary/
granularity/targeted-run axis: the worker delivers the input whose correlation
matches the committed ticket. Under correlation-keyed delivery, reordering
pending input cannot change the outcome, and `delivery_mode` is subsumed by the
correlation. Adding them would be cargo-culting a model we do not use.

### D2: A transactional outbox with no `delivered` flag and no 2PC

`MessageOutbox` stages a cross-thread delivery (`stage`) and relays it (`relay`).
The relay needs no `delivered` column: in one store transaction it appends the
payload to the target thread's pending input (idempotent by `message_id`) and
deletes the outbox row. A crash between the two leaves the outbox row, so the
next relay re-appends (a no-op) and deletes — at-least-once delivery with an
exactly-once effect. In a single-process host the outbox and pending tables are
in one store, so the relay is a local transaction; the same shape extends to a
distributed sender-outbox/target-append without two-phase commit.

The daemon relays each tick before draining; `DurableRunIngress` exposes
`stage_cross_thread` and `relay_outbox`, and `DispatchService` a `send`. Wiring a
runtime `send_message` tool/effect to these is deferred (a runtime-side concern).

## Consequences

- Pending input is mutable before consumption under optimistic concurrency;
  edit/retract are proven across the memory, Postgres, and SQLite backends
  against one shared spec.
- Cross-thread delivery is durable and idempotent without a delivered flag or
  2PC; the same outbox carries a scheduled delivery (ADR-0014) for free.
- The dispatch surface stays smaller than the reference by dropping reorder and
  `delivery_mode`, which correlation-keyed delivery makes redundant.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G1, G13 (committed truth; single-source).
- [run-ingress-message-delivery.md](../design/run-ingress-message-delivery.md) —
  the pending lifecycle and cross-thread outbox/idempotent-append rules.
- ADR-0010 — correlation-keyed delivery this builds the operations surface on.
