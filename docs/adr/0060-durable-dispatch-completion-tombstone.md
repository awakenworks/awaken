# ADR-0060: Durable Dispatch Completion Log and Tombstone

- Status: Accepted
- Date: 2026-07-19
- Depends on: ADR-0009, ADR-0010, ADR-0012, ADR-0022

## Context

`DispatchOutcome::Done` currently deletes the dispatch row. That is sufficient
for an in-process worker, but it loses two durable facts at the process boundary:

1. a downstream host cannot observe that delivery finished after either process
   restarts; and
2. an at-least-once producer can enqueue the same `run_id` again after deletion,
   even though the queue contract promises an exactly-once effect per run id.

Reading private dispatch tables, adding a database trigger, or calling a
best-effort completion callback would cross the dispatch bounded context and
leave a crash window between delete and notification.

## Decision

### D1: Completion is a dispatch-domain fact, not run outcome truth

The durable-dispatch bounded context owns a `DispatchCompletion` value object:
a store-assigned monotonic `sequence` and the neutral `run_id`. It means only
that a current fenced claim applied `DispatchOutcome::Done`; the committed fact
log remains the authority for why or how the run ended (G1/G6/G31/G32).

`DispatchQueue::completion_events_after(cursor, limit)` is the existing port's
repository query. A consumer owns its cursor and may replay pages idempotently.
No tenant, hosting, billing, protocol, or provider vocabulary crosses this
boundary.

### D2: Applied Done atomically writes one completion tombstone

The store inserts the completion row in the same transaction that deletes the
dispatch and pending input. A fenced settle writes nothing. `run_id` is unique,
so retry or recovery cannot publish two completion facts.

The completion row is also the permanent run-id tombstone: `enqueue_with` and
both exact new-run claim commands are no-ops for a completed `run_id`. This
makes the documented run-id idempotency survive row deletion and process
restart.

### D3: Consumers checkpoint; producers retain

Completion rows are ordered and retained. The producer does not own consumer
acknowledgements or a shared cursor. A future retention policy may compact event
payloads only if it preserves a permanent completed-run tombstone; deleting both
would reintroduce run resurrection.

### D4: Owner, enforcer, and first vertical slice

- **Bounded context / owner:** durable dispatch / `DispatchQueue`.
- **Model elements:** `DispatchCompletion` value object and completion tombstone
  repository row.
- **Port:** `DispatchQueue::completion_events_after`.
- **Guardrail:** G35.
- **Enforcer:** memory state plus the portable V0018 SQLite/Postgres migration;
  the fenced `settle` transaction and admission queries.
- **First vertical slice:** enqueue → claim → applied `Done` → cursor query →
  replayed enqueue/exact-claim stays absent, executed by the shared backend
  conformance suite.

## Consequences

- A separate process can project terminal delivery after either side restarts,
  without coupling to a backend schema or adding a second run authority.
- The queue's stated run-id idempotency becomes durable across successful
  completion.
- One compact row is retained per completed run. That storage cost is the price
  of permanent identity deduplication; operational retention must preserve the
  tombstone invariant.
- Remote worker transports need not expose this server-local projection query;
  their default implementation fails explicitly.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G1, G6, G13, G31, G32, G35.
- ADR-0009 — dispatch owns delivery opportunity, not run outcome truth.
- ADR-0010 — idempotent pending consumption and crash recovery.
- ADR-0012 — one portable migration bundle for SQLite and Postgres.
- ADR-0022 — fencing and supersession semantics.
