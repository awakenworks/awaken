# ADR-0010: Idempotent Pending Consumption via Committed Correlation

- Status: Accepted
- Date: 2026-06-30
- Depends on: ADR-0006, ADR-0009

## Context

ADR-0009 shipped durable resume but named one deferred gap: a crash *strictly
between* the resume commit and the dispatch settle could drop or double-apply a
pending input, because consumption state lived in the dispatch queue while run
truth lived in the committed fact log — two stores, no shared transaction.

The reference (`awaken-worktrees/goal`) closes this with a
`PendingConsumptionStore` that freezes pending records inside the same database
transaction as the run record (atomic append+freeze). That couples the dispatch
store into the commit transaction and adds a `frozen` lifecycle column.

## Decision

Do not couple the stores. Make consumption *idempotent by deriving it from
committed truth*, which is exactly the authority G1/G13/G32 already establish.

### D1: Pending input is keyed to the ticket correlation it answers

`PendingInput` carries a `correlation_id` — the `WaitingTicket` correlation it
answers. The worker delivers an input only while the committed ticket still
carries that same correlation. A resume that committed advanced the run, so the
active ticket is gone (run ended) or carries a new correlation (a fresh park);
either way the old input no longer matches and is never re-applied. Input for a
superseded ticket is likewise dropped without delivery.

### D2: The queue stores no consumption flag; settle consumes by id

The `frozen` column is removed. `claim` hands the worker the run's current
pending input without mutating it; the worker runs the resume, then tells
`settle` exactly which `message_id`s it consumed. A crash before settle leaves
the input in place, and recovery re-derives the correct action from the
committed ticket: re-deliver if the resume never committed, drop if it did. No
cross-store transaction, no dual-write, no freeze lifecycle.

## Consequences

- Closes the crash-window gap named in ADR-0009 D4 without an atomic
  append+freeze: a committed resume is never re-applied, and an uncommitted one
  is safely re-delivered.
- Fixes a latent bug in the ADR-0009 slice: input was applied to whatever ticket
  was current, so a stale answer could resolve the wrong park. Correlation
  matching makes that unrepresentable.
- The dispatch queue holds strictly less state (no `frozen`), and the property is
  proven by a crash-injection test plus a stale-correlation test.
- Multi-writer exactly-once still relies on the single-owner lease; the
  correlation key is the crash-recovery guard, not a concurrency guard.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G1, G13, G32 (committed truth is the one
  authority).
- ADR-0006 — the committed fact log and waiting ticket this derives from.
- ADR-0009 — the durable ingress slice this hardens.
