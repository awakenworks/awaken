# ADR-0087: SQLite Through sqlx

- Status: Accepted
- Date: 2026-10-06
- Builds on: ADR-0081 contract layering and ratchet guardrails; ADR-0045 (the
  2026-09-06 amendment keeps database connections out of awaken-foundation's
  connection crates).
- Affects: awaken-foundation ADR-0005 (rusqlite range for
  `awaken-scoped-migration-sqlite`) and awaken-cloud, which links the same native
  SQLite library.

## Context

Embedded process stores reach SQLite through rusqlite, a synchronous driver, while
the rest of the code base is asynchronous. Twenty-five crates depend on rusqlite.
Twenty of them already contain a second implementation of the same store on
PostgreSQL through sqlx, so most stores are maintained twice in two different
styles.

Bridging the synchronous driver into async code is the job of
`awaken-sqlite-pool`. Each store holds one shared `rusqlite::Connection` behind a
fair one-permit semaphore, and every operation is moved onto a blocking thread.
The same crate also owns the embedded connection policy: a 30-second busy wait,
foreign keys enabled, and a process-serialized one-time switch to WAL. Schema
migrations go through awaken-foundation's `awaken-scoped-migration-sqlite`, which
takes a `&rusqlite::Connection`.

The synchronous driver leaks into port design. `WebhookStore` and
`DreamProcessStore` are synchronous ports because their SQLite adapters were
synchronous; their PostgreSQL adapters then need `block_on` bridges
(`awaken-sql-storage`) to implement them.

A Rust binary can link only one `libsqlite3-sys`. sqlx 0.8.6's SQLite driver
requires `libsqlite3-sys` 0.30.x, the version rusqlite 0.32 uses. This repository's
lockfile already resolves rusqlite 0.32.1, and awaken-cloud pins
`rusqlite = "=0.32.1"`.

## Decision

1. **All SQLite access goes through sqlx.** Store adapters open a `SqlitePool` and
   use async queries, so the SQLite and PostgreSQL implementations of a store have
   the same shape. No production or test code in this repository uses rusqlite
   once the migration below completes.

2. **Today's admission semantics are kept exactly.** Each store opens its pool with
   `max_connections(1)`. sqlx pool acquisition is first-come first-served, which
   matches the current single connection behind a fair one-permit semaphore.
   Transactions that today use `TransactionBehavior::Immediate` use
   `begin_with("BEGIN IMMEDIATE")`. Widening read concurrency under WAL is a later,
   separately measured change.

3. **One connection policy, one crate, one handle.** `awaken-sqlite-pool` is
   rewritten on sqlx and renamed `awaken-sqlite-pool`. It owns only the embedded
   policy: the busy wait, foreign keys, and the serialized WAL switch, expressed
   as one `SqliteConnectOptions` constructor and one pool opener. The
   blocking-thread bridge and the shared-connection type are deleted. It stays in
   the `storage` bounded context. Stores hold its `EmbeddedPool` handle, not a bare
   `SqlitePool`. The handle is an sqlx executor with `begin`, `begin_with` and
   `acquire`, so store code reads like plain sqlx, and it carries the two
   guarantees below that a bare pool cannot.

4. **Synchronous ports become asynchronous.** `WebhookStore`, `DreamProcessStore`
   and the synchronous functions of `awaken-store-sqlite` become async. Their
   PostgreSQL adapters drop the matching `block_on` bridges from
   `awaken-sql-storage`. The coordinator's `runtime_authority` use of
   `block_on_ambient_runtime` is a deliberate cancellation-safety device and is
   outside this decision.

5. **Migrations move to foundation's sqlx backend.** awaken-foundation's
   `awaken-scoped-migration` gains a `sqlite` feature beside its existing
   `postgres` feature, with the same validation tests and a cross-driver test
   showing that a ledger written by either SQLite runner is accepted by the other
   (awaken-foundation ADR-0007, which partly reverses its ADR-0005 removal of the
   sqlx SQLite shell now that a consumer exists). Each store migrates through its
   own pool; there is no second driver or second connection to the same file.

6. **Version consequence, accepted.** While sqlx 0.8 is in use, any build graph
   that contains awaken's SQLite stores resolves `libsqlite3-sys` 0.30 and,
   therefore, rusqlite at most 0.32. awaken-cloud already pins 0.32.1.
   awaken-foundation's ADR-0005 range stays valid for graphs without awaken's
   stores. Moving past 0.30 waits for sqlx 0.9.

7. **No exceptions.** `awaken-eval` and the memoryd dev-dependency also move to
   sqlx. A single remaining rusqlite user would keep the version coupling of
   item 6 and a second connection style.

8. **Order.**
   - **Pool crate first.** `awaken-sqlite-pool` comes first, together with one small
     pilot store that fixes the adapter pattern and the conformance setup.
   - **Then the stores, smallest first.** Each crate is one commit with its
     existing SQLite tests and the shared `awaken-store-conformance` suites green.
   - **Dispatch and session last.** The dispatch and session stores
     (`awaken-dispatch`, `awaken-work-store`, `awaken-session-store`) come
     last, because their claim and lease semantics carry the most behavior.
   - **Then the foundation switch.** Finally, migrations move to the foundation
     `sqlite` feature, and rusqlite and the `awaken-sql-storage` bridges are
     removed.

9. **A paused test clock stands still while SQLite works.** sqlx runs SQLite on
   its own thread, which a paused Tokio clock cannot see: the clock jumps to the
   next timer whenever the runtime looks idle, including sqlx's own 24-hour
   admission bound. The bridge's blocking task hid this, because a running
   blocking task stops that jump. `EmbeddedPool` keeps the guarantee by holding
   a blocking task for the span of each query, transaction and acquired
   connection, measured from the request. The span therefore also covers the
   wait for the one connection, which includes the moment sqlx spends returning
   the previous caller's connection. A hold per lent-out connection, from pool
   hooks, leaves that moment open and also leaks at runtime shutdown, so it was
   rejected. Migrations run on the underlying pool inside `held`. Only a
   current-thread runtime can pause its clock, so other runtimes skip the hold.

10. **A dropped store's file settles before it is inspected.** Dropping a
    synchronous connection closed it at once. sqlx closes a dropped pool's
    connection later, on its own thread, and the last close checkpoints the WAL
    into the main file. `EmbeddedSqlite::settled` waits until every pool this
    process dropped on a file has finished closing; pools still in use do not
    delay it. A guard kept in a collation tells the pool's connections apart from
    its handles, because SQLite releases a collation's state exactly when it
    closes the connection. Read-only inspections of a file, such as the Session
    store's existing-database probe, settle first.

## Consequences of moving off the bridge

- Cancelling a write before `COMMIT` now rolls it back. The bridge's detached
  blocking task finished the commit anyway.
- A panic no longer poisons the connection. The transaction is rolled back and
  the next caller gets a clean connection.
- rusqlite stays in the build graph through awaken-iam until that repository
  moves to the same sqlx migration runner. Its `libsqlite3-sys` is the one sqlx
  links (item 6).

## Alternatives

- **Keep rusqlite and the bridge.** Two implementation styles per store stay. The
  sync-in-async bridge keeps its own scheduling code. Ports keep being shaped by one
  driver.
- **`sqlx::Any` for one query set across both databases.** The SQL dialects differ
  in placeholders, upsert syntax, locking and time functions. A lowest-common-
  denominator layer would hide those differences instead of owning them. The two
  adapters stay separate but share shape.
- **Keep rusqlite only for migrations permanently.** This leaves the version
  coupling and a second connection stack for the same file. It is acceptable only
  as the interim state of item 5.
- **Raise the pool size immediately.** This changes admission order and lock
  contention at the same time as the driver. Keeping one connection per store makes
  the driver change behavior-neutral and measurable.

## Consequences

- The PostgreSQL and SQLite adapters of each store have the same async shape, so
  shared conformance tests exercise both through one interface.
- `awaken-sqlite-pool` shrinks to a connection-policy crate. Its blocking bridge,
  the synchronous ports and the matching `block_on` bridges disappear.
- The plan's "delete `awaken-sqlite-pool`" becomes "rename to
  `awaken-sqlite-pool`, policy only".
- Builds that include awaken's SQLite stores are capped at `libsqlite3-sys` 0.30
  until sqlx 0.9.
- The migration touches about 25 crates and about 30 000 lines that use SQLite. It
  runs crate by crate, and each step leaves the repository consistent.
- Removing rusqlite completely requires publishing a new awaken-foundation
  revision.
