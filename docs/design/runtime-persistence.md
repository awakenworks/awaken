# Runtime Persistence — Contexts, Ports, and Backends

This document expands [ADR-0039](../adr/0039-runtime-persistence-port-convergence-and-store-naming.md)
into an implementable design: which bounded contexts own durable state, the port
each exposes, the backend adapters that satisfy them, and the two invariants every
backend must uphold. It is the P2 design that the store slices implement.

Naming follows ADR-0039: `Store` is a type suffix only for a genuine mutable keyed
store (`MemoryStore`); every other port names its authority; `awaken-store-<medium>`
is the crate category for the persistence seam (media `inmem` / `fs` / `postgres` /
`sqlite`).

## Context map

Durable state is not one domain. Three domains own truth or operational state, a
fourth is a downstream projection, and the backend crates are shared adapters that
cut across them.

```text
   protocol adapters ─────────────────────────────────────────────┐
   │  Protocol projection (adapter / anti-corruption)   [P3]       │  reads facts → wire DTOs
   │  port: ProtocolReplayLog        NOT runtime truth (taxonomy)  │
   └───────────────▲──────────────────────────────────────────────┘
                   │ consumes committed facts, after commit
   ┌───────────────┴──────────────────────────────────┐
   │  Agent truth (runtime kernel)                     │  the durable truth
   │  ports: CommitCoordinator · CheckpointReader ·    │  G1 / G13
   │         StreamSink (live, best-effort)            │  awaken-agent-contract
   └───────────────▲───────────────────────────────────┘
                   │ drives run/resume, delivers input, records terminal
   ┌───────────────┴──────────────────┐        ┌──────────────────────────────┐
   │  Dispatch / run ingress          │        │  Config / registry           │
   │  ports: DispatchQueue · Inbox ·  │        │  port: ConfigRegistry        │
   │         Outbox                   │        │  (resolved input → kernel)   │
   │  G5 / G6 / G18                   │        │  awaken-config-store         │
   │  awaken-run-ingress-contract     │        └──────────────────────────────┘
   └──────────────────────────────────┘

   awaken-store-{inmem, fs, postgres, sqlite}: shared adapters implementing the ports above
```

Reading the map:

- **Agent truth** is the center. It owns *what the agent did* — committed messages,
  run/state facts, and committed events. Only `CommitCoordinator` writes it (G1).
- **Dispatch / run ingress** is a **separate bounded context**. It owns *operational
  scheduling* — which runs are pending/claimed/parked, leases, pending input, and
  cross-thread delivery. It depends on the kernel (it drives `run`/`resume` and
  records the terminal commit) but never owns agent truth. This is why dispatch and
  the agent-truth store are two domains, not one.
- **Config / registry** is a third context: it produces the resolved input a run
  activates from; it is upstream of execution, not part of it.
- **Protocol projection** is a downstream **adapter** context. It *reads* committed
  facts and projects them into public wire events (AI SDK, AG-UI, Managed, …). Per
  the [commit/fact/projection taxonomy](commit-fact-projection-taxonomy.md) it is a
  projection, never truth; its `ProtocolReplayLog` is a projection cache written in
  the source commit's transaction or derived after it. Deferred to P3.
- **`awaken-store-*`** crates are **not a domain**. Each is one medium's adapter that
  implements the ports of several contexts (agent-truth + dispatch + config). The
  domains live in the contract crates; the media live in the store crates.

## Ports

### Agent truth — `awaken-agent-contract`

```rust
// write: the single durable boundary (G1). Extended so events + outbox intents
// commit atomically with the checkpoint (G13, D3).
trait CommitCoordinator { async fn commit(&self, plan: ThreadCommit) -> Result<CommitRecord>; }
struct ThreadCommit { thread_id, run_fact, messages, state, events, outbox, waiting }

// read: the single after-commit repository. Subsumes today's ThreadReader + RunStore.
trait CheckpointReader {
    fn committed_messages(&self, thread: &ThreadId) -> Vec<Message>;
    fn committed_state(&self, thread: &ThreadId) -> Vec<StateCommand>;
    fn run(&self, run: &RunId) -> Option<RunRecord>;
    fn latest_run(&self, thread: &ThreadId) -> Option<RunRecord>;
    fn waiting_ticket(&self, run: &RunId) -> Option<WaitingTicket>;
    async fn list_events(&self, scope: EventScope, from: Option<Cursor>, limit: usize) -> EventPage;
}

// live: best-effort progress (G10), reconciled by committed history.
trait StreamSink { async fn send(&self, event: StreamEvent) -> Result<()>; }
```

### Dispatch / run ingress — new `awaken-run-ingress-contract`

Extracted from `awaken-run-ingress` so backends can depend on the contract without
the host crate (G2). The traits are the current ones, renamed and grouped:

```rust
trait DispatchQueue { /* enqueue, claim, renew_lease, settle, reap, cancel,
                         parked_run, dead_letters, requeue, superseded, list */ }
trait Inbox         { /* append, list, edit, retract (pending input) */ }
trait Outbox        { /* stage, relay (cross-thread delivery) */ }
trait Dispatch: DispatchQueue + Inbox + Outbox {}
// Worker (the claim/run loop) and Signal (wake) are in-process, not store ports.
```

### Config — `awaken-config-store`

`ConfigRegistry` (today's `ConfigStore`, renamed): config records + publications
keyed by id/fingerprint. Versioned-registry surface is P1.

## Backend adapters (port × backend)

Each `awaken-store-<medium>` crate implements the union of the ports for the
contexts it backs. `✔` shipped, `P1/P3` deferred by phase.

| Port \ backend | inmem | fs | postgres | sqlite |
|---|---|---|---|---|
| `CommitCoordinator` | 2.2 | 2.3 | 2.4 | 2.5 |
| `CheckpointReader` | 2.2 | 2.3 | 2.4 | 2.5 |
| `Dispatch` (Queue/Inbox/Outbox) | 2.6 | 2.6 | 2.6 | 2.6 |
| `ConfigRegistry` | ✔ | ✔ | ✔ | — |
| `ProtocolReplayLog` | P3 | P3 | P3 | P3 |

Backend mechanisms (per ADR-0039 slices):

- **inmem** — `RwLock`/`HashMap`; atomic multi-write via a commit `Mutex` +
  snapshot-on-entry / restore-on-failure; live via `broadcast`.
- **fs** — temp-file + `rename` + a write-ahead checkpoint journal; crash recovery
  replays the journal on open; events are a per-scope append-only numbered index.
- **postgres / sqlite** — staged writes in one SQL transaction (stronger than the
  inmem snapshot); reads come from committed tables, not an in-process cache (D4).

## Two invariants every backend must uphold

1. **Atomic staged commit (G1/G13).** `events` and `outbox` intents on `ThreadCommit`
   become visible in the same transaction as messages/state/run, or are derived
   after commit — never through a parallel writer. The old `EventLog::append`
   side-write is removed. `CheckpointReader::list_events` is the read side.
2. **Fact-authority reads (D4, ADR-0006).** `CheckpointReader` reads committed
   tables / the fact log, so a durable run resumes after a process restart. Serving
   reads from an in-process projection cache (as the current SQL backends do) is a
   bug: the cache is empty in a fresh process and resume fails. A `FactCheckpointReader`
   that rebuilds the read model from the fact log is an internal helper, not a port.

## Conformance

One trait-generic suite every backend runs (ADR-0039 slice 2.6): commit atomicity,
event order (G1), cursor paging, idempotency, crash recovery (fs), **cross-instance
resume** (drop the projection, resume through a fresh reader — proves invariant 2),
and a projection-failure-does-not-uncommit case (G13). A backend with no passing
conformance run is not done.

## Slices

Per ADR-0039: **2.1** extract `awaken-run-ingress-contract`, define `CheckpointReader`,
extend `ThreadCommit`, apply the rename set (behavior-preserving) → **2.2**
`awaken-store-inmem` + the cross-instance resume test → **2.3** `awaken-store-fs`
(journal + recovery) → **2.4/2.5** postgres/sqlite full surface, read-from-tables →
**2.6** conformance suite + durable `Dispatch` backend.

## Guardrails

G1, G2, G5, G6, G13, G18 in [INVARIANTS](../INVARIANTS.md). Truth levels and the
projection rules that keep protocol replay out of runtime truth are owned by
[commit-fact-projection-taxonomy.md](commit-fact-projection-taxonomy.md); stable
commit/read/dispatch roles are catalogued in
[runtime-interface-boundaries.md](runtime-interface-boundaries.md#role-catalog).
