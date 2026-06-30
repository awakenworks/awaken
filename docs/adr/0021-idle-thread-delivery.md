# ADR-0021: Idle-Thread Delivery via an Unbound Thread Inbox

- Status: Accepted
- Date: 2026-06-30
- Depends on: ADR-0010, ADR-0013, ADR-0017

## Context

`send_message` (ADR-0017) delivers to the run currently parked on a thread. A
message to a thread with **no** parked run fails closed: there is no ticket to
answer. But inter-agent messaging is asynchronous — agent A may message thread B
when B has no run in flight. The message must not be lost, and it must reach B's
next run as *new input* (not a resume of a ticket that does not exist).

## Decision

### D1: An idle-thread message is unbound pending input

Pending input is normally bound to a run and a ticket correlation (ADR-0010). A
message to an idle thread is the same durable `PendingInput` with an **empty
`run_id` and `correlation_id`** — *unbound*: pending whose run is not yet
determined. It is addressed only by `thread_id`. It rides the same outbox →
relay → pending path as a bound delivery (ADR-0013), so there is one delivery
mechanism, not two; `send_message` stages a bound input when a run is parked and
an unbound input when none is.

### D2: The next run on the thread binds and consumes it

`PendingInbox::list_thread` returns a thread's unbound input. When the worker
starts a **fresh** run on a thread, it prepends that input (in arrival order) to
the run's activation input as user messages, so the queued message becomes new
input to the next run. Claim never delivers unbound input to a resume (its
run-id filter excludes the empty id), so an unbound message can only ever feed a
fresh run, never masquerade as a resume.

### D3: Consume on settle, not on read — crash-safe

The worker reads the thread inbox but does not remove it; the consumed entries
are settled away with the run's other consumed input once the run commits
(ADR-0010's read-then-remove rule). A crash after reading but before commit
leaves the unbound input in place, so the next attempt re-delivers it — at-least
-once with an exactly-once committed effect, exactly as bound pending input.

### D4: Auto-activation stays deferred

This binds an idle-thread message to the *next* run someone starts on the thread.
Spawning a new run *immediately* from an idle-thread message needs the thread's
last executable snapshot (its agent/config), which the after-commit
`ThreadReader` does not expose. That continuation-activation path — and the
config seam it needs — remains a named, deferred extension.

## Consequences

- A message to an idle thread is durable and reaches that thread's next run as new
  input; nothing is lost and nothing is mis-delivered as a resume.
- No new table or commit field: an unbound `PendingInput` reuses the pending
  inbox, and the worker's fresh-run path drains it.
- Auto-activating a run from an idle-thread message remains deferred (needs the
  thread-snapshot/config seam).

## References

- [INVARIANTS.md](../INVARIANTS.md) — G5 (ingress delivery semantics).
- ADR-0010 — correlation-keyed pending input and read-then-remove.
- ADR-0013 — the outbox/relay path the delivery reuses.
- ADR-0017 — `send_message` to a parked run, which this completes for idle threads.
