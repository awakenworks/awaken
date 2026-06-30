# ADR-0012: SQLite and Postgres Durable Store Backends

- Status: Accepted
- Date: 2026-06-30
- Depends on: ADR-0006, ADR-0008, ADR-0009

## Context

The durable layer shipped Postgres-only: the commit coordinator (ADR-0008) and
the dispatch store (ADR-0009) both name `sqlx`. The corpus already anticipates
more: `requirements-coverage.md` lists "File, SQLite, PostgreSQL, NATS" as store
adapters that "satisfy the same contracts; backing-service choice does not change
domain behavior," and ADR-0008 noted the commit bundle is written in the
migrator's portable tokens so "the same bundle could target SQLite." This ADR
makes SQLite real, for both durable layers, without duplicating schema or
changing the neutral contracts.

## Decision

### D1: One portable schema, two runners

The commit schema moves to `awaken-store-schema`, a driver-free crate that owns
the portable `MigrationBundle` and names only the migrator. Both backends apply
the *same* bundle: the Postgres runner renders `{json}`→`JSONB`, the SQLite runner
renders `{json}`→`TEXT`. The dispatch schema is shared the same way, in one module
both dispatch stores in `awaken-run-ingress` use. No SQL spec string is written
twice.

### D2: Per-backend adapter crates behind the same neutral ports

`awaken-store-postgres` (sqlx) and `awaken-store-sqlite` (rusqlite) are sibling
adapters; each implements the identical neutral `CommitCoordinator` /
`RunStore` / `ThreadReader` ports and satisfies the same commit contract
(ADR-0006): committed fact log is the authority, `run_record` is a derived cache
(G32). The dispatch ports gain a `SqliteDispatchStore` next to
`PostgresDispatchStore`. The in-memory `MemoryDispatchStore` remains the
executable specification all backends must match.

### D3: A synchronous driver behind the async ports

`rusqlite` is synchronous, so each SQLite write runs on a blocking thread
(`spawn_blocking`) and the async write boundary is preserved. The sync
`RunStore`/`ThreadReader` reads are served from the same in-memory projection the
Postgres coordinator already uses. Commits are serialized by a write lock so the
monotonic fence is assigned without a race — matching SQLite's single-writer
model.

### D4: SQLite claims need no `SKIP LOCKED`

The Postgres dispatch claim uses `FOR UPDATE SKIP LOCKED`; SQLite has neither. It
does not need them: every claim runs in a `BEGIN IMMEDIATE` transaction that
takes the database write lock, so claims serialize and a run is owned by one
worker at a time — the same single-owner guarantee by a different mechanism.

## Consequences

- The whole durable stack — commit boundary and dispatch queue — runs on either
  Postgres or embedded SQLite, selected by which adapter the host wires in.
- SQLite is embedded (`bundled`), so its tests need no external server and always
  run; an in-memory database covers the fast path and a temp file covers restart.
- A backing-service choice changes no domain behavior: the same contracts, the
  same schema, the same claim policy, proven against the shared memory spec.
- Postgres remains the durable default; SQLite suits single-node, embedded, and
  test deployments. NATS and other backends remain future adapters.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G1, G13, G32 (the commit contract every
  backend satisfies).
- ADR-0008 — the Postgres commit backend and the portable bundle this shares.
- ADR-0009 — the dispatch store and worker this adds a SQLite backend to.
- [requirements-coverage.md](../requirements-coverage.md) — store adapters
  satisfy the same contracts regardless of backing service.
