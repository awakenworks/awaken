# ADR-0008: A Durable Postgres Commit Backend

- Status: Accepted
- Date: 2026-06-30
- Depends on: ADR-0005, ADR-0006
- Amended by: ADR-0029 (a built-in `runtime` namespace replaces the D3 table prefix), ADR-0087 (D1: `sqlx` is named by every SQL adapter, not only `awaken-store-postgres`)
- Amended by: ADR-0039 (D4: reads come from committed facts, not an in-memory read projection)
- Amended: 2026-10-09 (reconciled with ADR-0087: D1 sqlx ownership)

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

The cross-chunk benchmark uses 32 independent commits per phase, each with 257
Messages, 257 StateCommands and 257 supplied Events. In three alternating
local runs against one PostgreSQL container, the pre-batch implementation at
`a944a6c9e3` measured 12.8–15.0 commit/s at 1/8/32 client concurrency; the
current implementation measured 73.2–96.3 commit/s. Minimal commits were
roughly flat, and eight-fact results varied across runs. These figures isolate
the local store path, not Worker, synchronous standby or Cell throughput; the
global commit lock remains the cross-Thread serial point.

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

## PostgreSQL checkout and cancellation (2026-10-07)

Runtime PostgreSQL connection lifecycle has one owner:
`awaken-sql-storage::postgres_pool_options`. Composition roots choose
connection capacity and share the authority pool across guarded mutations.
The [claim-guard recovery amendment](#claim-guard-recovery-dependency-pools-2026-11-12)
selects the pool for dependent observations. Standalone commit, dispatch,
checkpoint and Worker-directory constructors use the canonical checkout policy.
A composition that injects a `PgPool` must construct it with this policy.
Domain repositories retain ownership of their SQL, transactions and facts.

SQLx 0.8.6 can cancel `BEGIN` after sending it but before recording transaction
depth. Its drop guard then queues no rollback, and its protocol ping accepts the
server's open transaction. A later recovery read can inherit its snapshot and
fail when setting isolation, while other borrowers can execute in the wrong
transaction. Increasing replicas or retrying the projection does not repair that
connection. SQLx's upstream [PostgreSQL cancellation fix](https://github.com/transact-rs/sqlx/pull/4394)
also reports this defect in 0.9.0; upgrading to that release alone is insufficient.

The existing bounded checkout health check now proves clean server state as well
as transport liveness. It executes one simple-protocol, no-data anonymous block,
`DO $awaken_pool$ BEGIN ROLLBACK; END $awaken_pool$`. PostgreSQL permits its
transaction control only outside an explicit transaction block. A healthy
automatic transaction succeeds; an open or aborted transaction rejects the
check, and SQLx discards the connection before the next borrower can use it.
The block writes no domain data and creates no persistent function or schema
object. Fresh connections run the same check so unsupported database
permissions fail at construction rather than causing silent connection churn.

The database must have its standard `plpgsql` language and grant the connection
role `USAGE` on it. This does not require schema `CREATE` permission. Each check
retains the existing 500 ms deadline; total acquisition and connection capacity
remain composition policy. The check adds SQL execution cost and an automatic
transaction rollback to PostgreSQL statistics, so application commit throughput
and failed business transactions must not be inferred from total database
transaction counts. Scoped local throughput observations must quantify its cost.

Cancellation while opening a transaction now leads to connection retirement,
then a fresh canonical recovery transaction. Domain transaction rollback,
receipt replay, dense global order, claim fencing and the Managed committed-prefix
validation remain unchanged. An ambiguous commit still requires querying its
existing receipt; a clean connection does not prove that an external effect ran
exactly once. The cause/effect rules in `awaken-store-runtime`'s live PostgreSQL
tests own the cancellation, reuse, permission and durability coverage; the
production-role k3d scenario owns failure and completed-turn acceptance.

PostgreSQL documents the [anonymous block's transaction-control restriction](https://www.postgresql.org/docs/16/sql-do.html)
and [top-level transaction management](https://www.postgresql.org/docs/16/plpgsql-transactions.html).

## References

- [INVARIANTS.md](../INVARIANTS.md) — G1, G13, G31, G32 (the commit contract).
- ADR-0006 — the commit contract any coordinator must satisfy.
- [runtime-behavior.md](../design/runtime-behavior.md) — `ThreadCommit`,
  `RunRecord`, and the commit boundary.

## Amendment 2026-10-09: reconciled with ADR-0087

- D1 ("`awaken-store-postgres` is the only crate allowed to name `sqlx`") no longer
  holds. The Postgres dispatch adapter lives in `awaken-run-ingress`
  ([ADR-0009](0009-durable-run-ingress-slice.md) D1, `crates/server/awaken-run-ingress/Cargo.toml:38`), and
  [ADR-0087](0087-sqlite-through-sqlx.md) sends all SQLite access through sqlx
  (`crates/platform/committed-truth/awaken-store-sqlite/Cargo.toml:24`). What still holds: the
  runtime core never names a SQL driver; only storage adapters do.
- D3: the table prefix is a built-in constant, not a parameter ([ADR-0029](0029-built-in-runtime-namespace.md);
  `crates/platform/committed-truth/awaken-store-postgres/src/lib.rs:190`).
- Names: `RunStore` / `ThreadReader` are now `CommittedThreadView` (+
  `CheckpointReader` for events), per [ADR-0039](0039-runtime-persistence-port-convergence-and-store-naming.md). `DurableRunIngress` no
  longer exists (removed by the 2026-09-01 amendment of [ADR-0009](0009-durable-run-ingress-slice.md)).


## Claim-guard recovery dependency pools (2026-11-12)

### Cause and decision

A legal claim-guard operation can retain a PostgreSQL transaction while awaiting
three existing recovery dependencies. Worker selection lists registrations,
predecessor recovery reads the prior Worker incarnation, and branch recovery
loads the committed Thread transcript and recovery prefix. When every checkout
in their shared pool is held by such a guard, the dependency waits for a
connection whose release requires that same dependency to finish. One retained
guard and a one-connection pool are sufficient to reproduce the cycle.

The Coordinator composition constructs one bounded authority pool and one
independent bounded dependency pool over the same PostgreSQL primary and schema.
Dispatch guards, Registry mutations and commit writes acquire the authority pool.
Worker-directory observations and committed Thread/message/event/lifecycle and
recovery operations acquire the shared dependency pool. Thread-scoped execution
projections retain both pool handles and acquire connections for their operations.
Recovery reconstructs its existing derived execution view from committed facts.
The dependency pool carries these repository operations through their original
ports and validation.

This allocation follows the Dispatch and Committed Truth bounded contexts.
`CoordinatorPersistence` owns physical connection composition, the Registry owns
Worker facts, and `PostgresCommitCoordinator` owns committed facts and replay.
The live `CommitEpochGuard` keeps its original transaction and fence throughout
the dependent work. Successful recovery still requires the original Worker,
incarnation, claim and committed-prefix checks before an authoritative write.

Increasing the original pool size delays exhaustion to a larger set of guarded
callers. Releasing the guard before dependency evaluation loses its protected
admission interval. A pool created per request makes connection capacity grow
with callers. The selected shared dependency pool provides a separately bounded
checkout budget while preserving the original admission and commit boundaries.

### Construction and connection budget

Both `PostgresWorkerDirectory` and `PostgresCommitCoordinator` expose async
`with_read_pool(authority_pool, read_pool)` and
`with_existing_read_pool(authority_pool, read_pool)`. The first applies the
existing migration bundles through `authority_pool`, and the second verifies the
externally managed bundles through that pool. Each caller supplies independent
pool instances configured for one primary, database, role and schema/search path.
The constructors rely on this composition contract. Their inputs are ordinary
`PgPool` handles and carry no automatic cross-pool database-identity comparison.

The existing `with_pool`, `with_existing_pool`, `connect` and `connect_existing`
constructors alias dependency operations to their authority pool. They retain
single-pool checkout behavior under a retained guard. Callers requiring the
independent dependency budget select the explicit two-pool constructors.

Production `CoordinatorPersistence::open` and `open_existing`, together with the
`DurableRuntimeAuthority` scenario composition, create the pair once from the
same URL and `awaken-sql-storage::postgres_pool_options`. The private
`postgres_pools` component owns their shared backend selection, database-URL
validation and bounded pool construction. Both composition owners depend on
this component, whose dependencies point to configuration and SQL storage.
The existing backend-selection decision table and module-cycle ratchet enforce
this dependency direction. With
`N = CoordinatorStorage.postgres_max_connections`, each pool has maximum `N`.
The pair contributes up to `2N` connections per Coordinator composition instance.
Operator connection budgeting includes both maxima and every replica. Other
component pools contribute their own configured budgets.

`CoordinatorPersistence.postgres_pool` exposes the authority pool used by guarded
writes and hosted adapters. The directory and commit adapters retain the shared
dependency pool. The operational migration path uses its single bounded pool of
maximum `N`, applying bundles without starting the runtime recovery composition.
Acquisition cancellation and timeout use the canonical checkout policy. A
saturated dependency pool surfaces the existing unavailable/acquisition error.

## PostgreSQL Role Catalog

| Name | Kind | Owns | Uses | Must Not Own | Failure Mode | Guardrail/Test |
|---|---|---|---|---|---|---|
| `CoordinatorPersistence` | Application composition | One process-owned authority/dependency pool pair and its explicit connection maxima | `CoordinatorStorage`, private `postgres_pools` construction, Registry and Runtime authority constructors | Repository SQL and domain admission decisions | Every retained guard consumes the capacity needed by its own dependent operation | G67, production constructor and exact-claim PostgreSQL regression |
| `PostgresWorkerDirectory` | Persistence adapter | Worker registration/incarnation observations and original Registry mutation transactions | Authority pool for guards/mutations, shared dependency pool for observations | Dispatch claim mutation and Thread commit authority | Worker selection or predecessor observation waits on the held guard pool | G67, Registry dependency-read conformance |
| `PostgresCommitCoordinator` | Persistence adapter | Atomic Thread commits and authoritative committed-prefix recovery | Authority pool for commit writes, shared dependency pool for observations/recovery, bounded Thread execution views | Worker selection and Dispatch admission authority | Transcript or branch recovery exhausts the guard's checkout pool | G67, store recovery and production claim-composition regressions |

### Test design and qualification boundary

G67 is a Target guardrail while the changed cohort awaits qualification. The
following cases name intended assertions. The table records test design and
existing owners without claiming their execution passed.

Its promotion profile indexes the existing schema/adapter, claim/delivery and
atomic-commit safety requirements. The separate
`store.backends/guard_dependency_progress` liveness requirement registers the
three new PostgreSQL test obligations. Promotion of G67 additionally requires
the actual exhausted-pool/held-guard/recovery cases and composing-product HTTP
resume evidence on the final source. Safety-profile results establish their
indexed authority properties. The separate executed cases establish dependency
progress for their owned pool and recovery conditions.

| Entry or cause | Required observation | Existing validation owner |
|---|---|---|
| Worker-directory authority pool has one connection held by a live guard | `list`, current registration and incarnation observations complete through the independent bounded pool, retaining the expected Worker facts | `postgres_dependency_reads_do_not_borrow_the_authority_pool` in `awaken-worker-registry` conformance |
| Commit authority pool is exhausted | Committed messages, branch transcript, recovery prefix, events/lifecycle and a Thread execution view remain observable through the dependency pool, and that view can commit through the original authority after the dependency pool closes | `dependency_reads_survive_an_exhausted_authority_pool` in `awaken-store-postgres` `postgres_live` |
| Production migration/verification composition exhausts all authority slots at maximum one and two | Real `CommitEpochGuard` values stay live during Registry observation, cold recovery and Thread-view opening, malformed prefixes refuse, and explicit release or guard drop returns capacity with the claim still current | `postgres_process_dependency_reads_keep_the_original_claim_authority` in the Coordinator `claimed_commit` tests |
| A current guarded owner writes or a receipt is replayed | Original claim-fenced commit succeeds once, replay returns its original receipt and stale owners cannot append | Existing Coordinator `claimed_commit` conformance plus the composing product's PostgreSQL terminal-writer recovery tests |
| Worker, incarnation or claim becomes stale, cancelled or lease-expired | Original admission/fence checks refuse authoritative effects and preserve committed truth | Existing Dispatch and signed Worker-transport stale-owner, cancellation and claim conformance |
| Dependency checkout is exhausted or its awaiting task is cancelled | Existing unavailable/timeout behavior, bounded pool sizes, released cancelled work and clean subsequent authority checkout | Live PostgreSQL acquisition/cancellation conformance plus the affected dependency-read cases |
| Thread-scoped projections are cloned or reopened | Both pool handles are propagated, ordinary handles retain no checked-out connection, and exact committed-prefix validation still applies | Store Thread projection/recovery tests and Coordinator claim composition |
| Migrate versus externally managed startup | Both pools share the owned schema, migration and verification select their original authority entrypoints, and the operational migration path uses its single configured pool | Existing scoped-migration and Coordinator persistence tests |
| Public constructor compatibility | Existing single-pool calls compile and use their original aliasing path, explicit pair constructors appear in reviewed API snapshots | Affected Registry/store/Coordinator compilation and `check_public_api.sh` owners |

Real HTTP recovery of the three original branches belongs to the composing
product's existing PostgreSQL resume tests. It qualifies the exact published
Native revision consumed by that product, including final claim release and
terminal outcome. Adapter-level progress, documentation checks and a selected
API snapshot each establish their own scope. Qualification results remain bound to their actual source and declared scope.
