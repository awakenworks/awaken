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
run-fact state authority and the monotonic fence, G31/G32), the message
transcript, the state-command log, committed events, a `run_record` cache (a
projection of the latest fact, G32), and active resume tickets. There is no
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

- A run's facts, transcript, state, events, and awaiting state survive a restart;
  a fresh coordinator on the same database rehydrates them.
- The durable `CommitCoordinator` backend that ADR-0006 D4 anticipated now exists,
  so the durable-ingress work it gated (a `DurableRunIngress` with a queue/worker)
  has something to persist to. That ingress remains future work.
- Tests run against a real Postgres; with no database reachable they skip, so the
  suite still passes without one.

## Lifecycle predecessor access (2026-09-13)

The existing lifecycle feed must classify `Running` after `Awaiting` using the
preceding committed state of that same Run. Its SQL query looks backward by
`run_id` and exclusive `sequence`. The original event primary key covered only
sequence, so repeated full-history observations searched unrelated Runs for
each event. A retained container test database reproduced this access cost.

Portable commit migration v11 adds the one `(run_id, sequence)` index in
`awaken-store-schema`, shared by SQLite and PostgreSQL. It changes physical
lookup only: the classifier, feed cursor, historical event bytes, and atomic
commit boundary remain the original authorities. No new feed, cached lifecycle
truth, or schema bootstrap path is introduced. The migration runs through the
existing checksum-guarded startup transaction; populated v10 upgrade, all
historical prefixes, and repeated/concurrent startup use the existing migration
suite. The v11 case comments own data-preservation and query-plan rules.

In the retained PostgreSQL experiment, the same 256-event lifecycle query took
7.904 ms without the index and 2.119 ms with a transaction-local index, then the
experiment rolled back. This is a query observation, not the completed-turn
latency gate: the full container campaign must still meet its original bound.
The migration adds no state transition requiring another abstract model;
backend migration tests and actual query plans cover the new physical access
path. Existing formal commit and migration invariants retain their prior proof
boundaries and do not establish a performance guarantee.

## Bounded fact inserts under the existing commit authority (2026-09-25)

The PostgreSQL adapter now inserts one commit's Messages, StateCommands and
Events in ordered chunks of at most 256 rows per statement. Message and state
row ids continue to follow their source order through `WITH ORDINALITY`; event
sequences remain the existing explicit lifecycle cursors. The transaction,
global dense-sequence allocation, terminal check and rollback boundary are
unchanged. A failure in a later chunk rolls back every earlier chunk and leaves
the global successor available to the next successful commit.

This reduces SQL round trips while the existing global commit-table lock is
held. It does not remove that cross-Thread serial point or the full-history
startup projection. The live PostgreSQL tests cover ordered replay across a
chunk boundary, later-chunk rollback, reconnect and concurrent dense commits;
the opt-in benchmark records throughput for minimal and multi-fact commits.

## Bounded startup and asynchronous committed reads (2026-09-25)

The original D4 full-database projection is retired for PostgreSQL. Startup now
verifies the schema and reads only the indexed global commit fence. Coordinator
opens a process-local execution view for an active Thread from the existing
repeatable-read `RunRecoverySource` snapshot; it reuses the same single-flight
registry and prunes inactive views with bounded work per new opening. The synchronous
view remains an execution convenience, not a durable read authority. Scoped
views reject a commit addressed to another Thread before SQL is written.

`CheckpointReader::list_events` is now asynchronous and queries committed SQL;
storage failure is an error rather than an empty event page. The portable
single-process lifecycle reader retains its execution-view Run lookup;
PostgreSQL uses its native SQL lifecycle feed. Cross-replica policy and public
history continue to use the existing authoritative PostgreSQL queries. The
fact log, dense commit sequence and receipt transaction are unchanged. A large
single Thread still costs proportional time and memory when *that Thread* is
opened; this change removes the whole-Cell startup dependency, not the need
for bounded or incremental long-Thread recovery.

The opt-in hot-Thread observation makes that remaining cost visible without
changing the read authority. On one local PostgreSQL primary, opening a Thread
with 100,001 historical Messages took 632 ms; 1,000,001 took 4,222 ms. Opening
an unrelated Thread in those runs took 4–5 ms. These are single samples, not
latency targets or production capacity evidence; a long-Thread acceptance SLO
and an incremental recovery design remain separate work.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G1, G13, G31, G32 (the commit contract).
- ADR-0006 — the commit contract any coordinator must satisfy.
- [runtime-behavior.md](../design/runtime-behavior.md) — `ThreadCommit`,
  `RunRecord`, and the commit boundary.
