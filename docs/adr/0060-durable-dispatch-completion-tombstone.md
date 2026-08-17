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
- **Enforcer:** memory state plus the portable V0016 SQLite/Postgres migration;
  the fenced `settle` transaction and admission queries.
- **First vertical slice:** enqueue → claim → applied `Done` → cursor query →
  replayed enqueue/exact-claim stays absent, executed by the shared backend
  conformance suite.

### D5: Commit/settle gaps repair through the same fenced Done path

A quiescent `awaiting` dispatch can survive a historical crash or defect even
after the matching committed Run is terminal. The same gap can recur if the
reconciler crashes after claiming that row but before its `Done` settlement,
leaving an expired `running` lease. The store-owning Runtime Host periodically
reads both shapes through its own Thread commit boundary. A Host with a local
execution pool uses that pool's maintenance loop; a coordinator-only Host starts
one mutually exclusive reconciliation daemon. Database-less Workers do neither.
Only an exact committed `RunState::Ended` proof permits a special claim of an
unleased `awaiting` row or strictly expired `running` lease; a live lease is never
touched. The claim skips execution placement and credentials because it cannot
execute. It then reuses the ordinary epoch-fenced `Done` settlement, including
terminal observer redelivery and the D2 completion tombstone.

The dispatch store never accepts or derives Run outcome truth. It only exposes
the narrowly scoped claim command, preserves the one-running-dispatch-per-Thread
constraint, and rejects pending, live-leased, dead-lettered, or superseded rows.
Reclaiming an expired lease advances the existing epoch and fences the crashed
owner. The worker checks committed truth both before and after the claim; a stale
proof restores `Awaiting`. This adds no cleanup table, outcome status, or direct
row-deletion path.

### D6: A caller-owned Run id identifies one canonical dispatch

Every admission path compares identity before placement, scheduling, or
supersession eligibility. A live row is replayable only when its complete
`RunDispatch` has byte-equal canonical JSON after excluding `traceparent`; any
other execution-bearing difference is rejected. Once `Done` compacts the live row,
the same transaction stores that canonical dispatch identity as a SHA-256
fingerprint on the existing completion tombstone. A retry with the same Run id
and canonical dispatch is a no-op; a different or unverifiable payload is
rejected. This ordering also applies to concurrent first admission, so an
incompatible placement cannot mask a collision and two contenders cannot both
authorize different payloads.

`SubmitOptions` are delivery policy rather than Run identity. The first
accepted options remain authoritative on an exact Run-id replay; retry options
cannot mutate or supersede the existing dispatch. A live `dedupe_key` collision
remains the separate caller-key no-op defined by ADR-0018.

Migration V0024 adds the nullable fingerprint to the existing tombstone rather
than creating a collision registry. `NULL` is reserved for historical rows
whose dispatch payload is no longer provable; those Run ids fail closed on
replay instead of treating absence of evidence as identity equality.

## Consequences

- A separate process can project terminal delivery after either side restarts,
  without coupling to a backend schema or adding a second run authority.
- The queue's stated run-id idempotency becomes durable across successful
  completion and rejects caller-owned Run-id reuse with another payload.
- One compact row is retained per completed run. That storage cost is the price
  of permanent identity deduplication; operational retention must preserve the
  tombstone invariant.
- Remote worker transports need not expose this server-local projection query;
  their default implementation fails explicitly.
- Legacy commit/settle and reconciliation-claim crash gaps converge without
  replaying execution or provisioning a Session environment; nonterminal rows
  and terminal rows with live leases remain untouched.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G1, G6, G13, G31, G32, G35.
- ADR-0009 — dispatch owns delivery opportunity, not run outcome truth.
- ADR-0010 — idempotent pending consumption and crash recovery.
- ADR-0012 — one portable migration bundle for SQLite and Postgres.
- ADR-0022 — fencing and supersession semantics.
