# ADR-0008: A Durable Postgres Commit Backend

- Status: Accepted
- Date: 2026-06-30
- Depends on: ADR-0005, ADR-0006

## Context

ADR-0006 fixed the commit contract — the committed fact log is the read
authority, the `RunRecord` is a derived cache equal to the latest fact, and the
append fence is the committed fact count — and stated those rules so they hold
"one backend or many" before a durable backend existed. The in-memory
`MemoryCommitCoordinator` was the only implementation; nothing survived a restart.

This ADR introduces the first durable backend: a Postgres implementation of the
same `Coordinator` write boundary and the `RunStore` / `ThreadReader` read ports.

## Decision

### D1: A standalone adapter crate owns the SQL driver

`awaken-store-postgres` is the only crate allowed to name the SQL driver
(`sqlx`) and the migration ledger (`awaken-scoped-migration`), the same way
`awaken-provider-genai` is the only crate that names the model SDK (ADR-0007
boundary discipline). It depends on the neutral contracts and implements their
ports; the runtime core never names Postgres.

### D2: The schema is a faithful, minimal projection of the commit value

One table per field of the staged `ThreadCommit`: an append-only commit log (the
run-fact phase authority and the monotonic fence, G31/G32), the message
transcript, the state-command log, committed events, a `run_record` cache (a
projection of the latest fact, G32), and active waiting tickets. There is no
outbox, scope index, or idempotency table — those belong to a server layer above
and are out of scope for the runtime commit contract. Each `commit` writes all of
it in one SQL transaction, so the checkpoint is atomic (G1/G13).

### D3: Schema lives in a scoped migration bundle, not lazy DDL

The schema is one `awaken-scoped-migration` `MigrationBundle` (`awaken.runtime_commit`),
applied with a fail-closed, checksum-verified runner under a table prefix so the
runtime's tables can share a database with other components without collision.
Migrations are explicit and versioned rather than implicit `CREATE TABLE IF NOT
EXISTS`, so schema evolution is reviewable.

### D4: An in-memory projection serves the synchronous read ports

`RunStore::get` and `ThreadReader` are synchronous, but Postgres reads are async.
The coordinator keeps an in-memory projection of committed truth — rebuilt from
the log on construction (durable across restart) and advanced in lockstep with
each commit. The projection is never an independent authority: it always equals
what replay derives from the log (G32). This keeps the sync ports honest without
blocking an async runtime inside them.

## Consequences

- A run's facts, transcript, state, events, and waiting state survive a restart;
  a fresh coordinator on the same database rehydrates them.
- The durable `CommitCoordinator` backend that ADR-0006 D4 anticipated now exists,
  so the durable-ingress work it gated (a `DurableRunIngress` with a queue/worker)
  has something to persist to. That ingress remains future work.
- Tests run against a real Postgres; with no database reachable they skip, so the
  suite still passes without one.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G1, G13, G31, G32 (the commit contract).
- ADR-0006 — the commit contract any coordinator must satisfy.
- [runtime-behavior.md](../design/runtime-behavior.md) — `ThreadCommit`,
  `RunRecord`, and the commit boundary.
