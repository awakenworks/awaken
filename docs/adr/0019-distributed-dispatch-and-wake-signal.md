# ADR-0019: Distributed Dispatch — Lease Renewal and a Wake Signal

- Status: Accepted
- Date: 2026-06-30
- Depends on: ADR-0009, ADR-0011, ADR-0015

## Context

The durable dispatch queue was designed single-process, but a multi-node fleet is
the eventual target. The reference reaches it with a NATS KV store plus JetStream
wake signals and a lease-renewal manager. Two questions: how much of "distributed"
is already true, and is a NATS *store* the right next step when no NATS server is
available to test against.

## Decision

### D1: The Postgres store is already a multi-node queue

`claim` uses `FOR UPDATE SKIP LOCKED`, so several worker processes sharing one
Postgres claim *distinct* runs concurrently without a global lock — the
single-owner-per-run guarantee holds across nodes. This is proven by a test that
runs two concurrent claims and asserts they take different runs. No new store is
needed for multi-node claim; the durable store is the shared queue.

### D2: Lease renewal is the missing liveness knob

A node executing a long run must not have its lease reclaimed by another node's
recovery. `renew_lease(run_id, owner, lease_ms, now)` extends the lease iff the
caller still owns it, and returns `false` if the lease was lost (stolen, settled,
unknown) so the holder stops. Recovery and the crash-retry budget (ADR-0015) are
unchanged; renewal only keeps a *live* run owned.

### D3: A wake signal is a hint, behind a port; NATS is one adapter

Wake records are hints (run-ingress design): losing one only delays a drain to
the next poll, so correctness never depends on them. `WakeSignal` is the neutral
push port. `LocalWakeSignal` is the single-process implementation, and it
*replaces* the daemon's ad-hoc notify — the daemon now waits on a `WakeSignal`,
so the local and distributed cases share one seam. `NatsWakeSignal` (feature
`nats`) fans the hint across nodes over a core-NATS subject (fire-and-forget,
matching the at-most-once a hint allows).

### D4: NATS is optional and integration-deferred

The NATS adapter is behind an off-by-default `nats` feature, so the default build
and test suite carry no NATS driver. It compiles under the feature; a live test
against a NATS server is deferred (none is available here), exactly as the
Postgres live tests skip without a database. A full NATS *KV store* (an
alternative to Postgres/SQLite) is not built: the durable store already provides
distributed claim, so NATS earns its place as a wake-signal optimisation, not a
second source of truth.

## Consequences

- Multi-node dispatch works today on Postgres (concurrent distinct claim),
  hardened by lease renewal so long runs are not stolen.
- The daemon's wake is now a pluggable `WakeSignal`; a fleet uses `NatsWakeSignal`
  to avoid busy-polling, with poll as the always-correct fallback.
- The default build stays lean; NATS is opt-in and its live test deferred.
- A NATS-backed store, JetStream durability, and per-run renewal scheduling in the
  daemon remain named, deferred items.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G5/G6 (durable ingress over runtime control).
- [run-ingress-message-delivery.md](../design/run-ingress-message-delivery.md) —
  "Distributed Placement Rules"; wake records are hints, durable state is truth.
- ADR-0009 — the dispatch claim and `SKIP LOCKED`.
- ADR-0015 — the crash-retry budget renewal interacts with.
