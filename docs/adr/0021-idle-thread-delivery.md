# ADR-0021: Idle-Thread Delivery via an Unbound Thread Inbox

- Status: Accepted
- Date: 2026-06-30
- Amended: 2026-08-31
- Depends on: ADR-0010, ADR-0013, ADR-0017

## Context

Product/protocol adapters may receive input for a Thread with no Run in flight.
The input must not be lost, and it must reach that Thread's next Run as *new
input* rather than masquerading as a reply to a ticket that does not exist.
Internal Agent messaging is separate: Managed coordination already derives a
deterministic fresh Run and follows ADR-0017's Thread-owned command path rather
than writing pending input.

## Decision

### D1: An idle-thread message is unbound pending input

Pending input is normally bound to a run and a ticket correlation (ADR-0010). A
input to an idle Thread is the same durable `PendingInput` with an **empty
`run_id` and `correlation_id`** — *unbound*: pending whose run is not yet
determined. It is addressed only by `thread_id`. It rides the same outbox →
relay → pending path as a bound delivery (ADR-0013), so there is one delivery
mechanism for external ingress, not two. Only an authenticated ingress adapter
that intentionally chooses next-Run semantics should append this unbound form;
it is not an Agent-to-Agent message queue.

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

### D4: Generic auto-activation stays deferred; explicit continuation is exact

This binds accepted idle-thread input to the *next* run someone starts on the thread.
Spawning a new run *immediately* from an idle-thread message needs the thread's
last executable snapshot (its agent/config), which the after-commit
`ThreadReader` does not expose. That generic continuation-activation path — and
the config seam it needs — remains a named, deferred extension for external
ingress.

Managed Agent coordination does not use this deferred external-ingress path. Its
Session owner already holds a deterministic `RunDispatch` and freezes the
message directly in `RunActivation.input` under ADR-0017, so concurrent
follow-ups cannot consume one another's input.

## Consequences

- A message to an idle thread is durable and reaches that thread's next run as new
  input; nothing is lost and nothing is mis-delivered as a resume.
- No new table or commit field: an unbound `PendingInput` reuses the pending
  inbox, and the worker's fresh-run path drains it.
- Inferring and auto-activating a run from generic idle-thread input remains
  deferred (needs the thread-snapshot/config seam); internal Agent coordination
  does not infer it and instead supplies an explicit frozen activation.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G5 (ingress delivery semantics).
- ADR-0010 — correlation-keyed pending input and read-then-remove.
- ADR-0013 — the outbox/relay path the delivery reuses.
- ADR-0017 — Thread-owned Managed messaging and deterministic fresh-Run admission.
