# ADR-0009: A Minimal Durable Run Ingress Slice

- Status: Accepted
- Date: 2026-06-30
- Depends on: ADR-0006, ADR-0008

## Context

`RunIngress` has exactly two delivery semantics (G5): direct, shipped by the
runtime as `DirectRunIngress`, and durable, `DurableRunIngress`. Until now only
the direct half existed; `submit_background` failed closed on it and there was no
durable half at all. ADR-0008 then landed `PostgresCommitCoordinator`, the first
durable commit backend, so — as its consequences noted — durable ingress finally
"has something to persist to," but its queue and worker remained future work.

The reference design planned this host: `key-design-decisions.md` D11 names an
`awaken-run-ingress` "durable run host" crate (and a separate
`awaken-run-ingress-contract`), and `deny.toml` already reserves the
`awaken-run-ingress` layer. The risk is the opposite of missing it: a full
durable dispatch subsystem (lease renewal, scheduled wake, supersession,
dead-letter, cross-thread outbox, dispatch query/maintenance, a per-thread worker
state machine) is a large surface. The repo's own rule rejects speculative
breadth: build the first tested slice, name the rest as deferred.

## Decision

### D1: One host crate, not yet split into contract/impl/stores

`awaken-run-ingress` holds the whole durable-ingress aggregate — the dispatch
ports, an in-memory reference store, the Postgres adapter, the worker, and
`DurableRunIngress` — in one crate. The planned `awaken-run-ingress-contract`
split (ports in their own crate, backends in `awaken-stores`) is **deferred**
until a second backend or an out-of-crate consumer needs it; splitting now would
be a speculative crate list (`key-design-decisions.md`, "no speculative crate
lists"). The crate sits in the layer `deny.toml` already reserved: it depends on
the runtime kernel and the store adapter; nothing below depends on it (G2/G29).

It lives in `awaken-run-ingress`, not `awaken-store-postgres`, because the
layering fitness function forbids the store adapter from depending upward on the
runtime kernel that the worker drives. Durable-ingress internals are a host
concern, not a runtime-core seam (G6).

### D2: Two aggregates, faithful to the run-ingress DDD split

`RunDispatch` owns delivery opportunity — enqueue, single-owner claim, lease, and
lease-expiry recovery. `PendingInbox` owns the thread's pending input —
idempotent append, frozen once at a run boundary. Neither owns a run's outcome.
One concrete store implements both so a wake can freeze pending input inside the
claim transaction, but the traits stay split so neither aggregate reaches into
the other's invariants.

### D3: The worker reads committed truth; the queue is never a second authority

The worker decides execute-versus-resume from committed facts — the waiting
ticket and the `RunRecord` read back through the same commit handle the runtime
writes through (G6 same-source wiring) — not from a status duplicated in the
queue. A reclaimed run that already committed a terminal state is settled without
re-running. So the dispatch row carries delivery state only; run truth stays in
the fact log (G1/G13/G32), and recovery cannot double-execute a finished run.

### D4: Minimal mechanism; the rest is named, not built

The slice ships idempotent enqueue, claim with a single-owner lease,
`FOR UPDATE SKIP LOCKED` concurrency, lease-expiry recovery, and durable resume
through delivered input. Explicitly deferred (named here so the gap is visible,
not silently absent): scheduled wake, lease renewal/heartbeat, cross-thread
`send_message` outbox, dispatch query/maintenance/GC, supersession, dead-letter,
and exactly-once pending consumption across a crash strictly between commit and
settle. `RunIngressCapabilities` reports `scheduled_wake = false` accordingly.

## Consequences

- G5 and G6 gain live enforcers: `submit_background` now succeeds on durable
  ingress and still fails closed on direct ingress; the worker adds durability
  over the same `RunExecutor`/`Runtime::resume` a direct caller uses, sharing one
  commit source by construction. Memory-backed tests prove the logic; live
  Postgres tests prove the durable loop and a restart.
- The durable-ingress work gated by ADR-0006 D4 and ADR-0008 now exists as a
  first slice; a `RunExecutionRequest` is the serializable record a queue
  persists, and `RunExecutionContext` rebuilds the live wiring per attempt.
- The in-memory store is the executable specification the Postgres store matches,
  the same pattern `MemoryCommitCoordinator`/`PostgresCommitCoordinator` use.
- A crash strictly between commit and settle may drop one in-flight pending
  delivery; closing that gap (atomic append+freeze in the commit transaction) is
  deferred with the other named items above.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G1, G5, G6, G13, G32.
- [run-ingress-message-delivery.md](../design/run-ingress-message-delivery.md) —
  the dispatch/server boundary, role catalog, and message lifecycle this realizes.
- ADR-0006 — the commit contract the worker reads as authority.
- ADR-0008 — the durable commit backend this persists run truth through.
