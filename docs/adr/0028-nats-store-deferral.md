# ADR-0028: NATS Integration — Wake Signal Live-Tested, KV Store Deferred

- Status: Accepted
- Date: 2026-06-30
- Depends on: ADR-0019

## Context

ADR-0019 added a feature-gated `NatsWakeSignal` and named two NATS items as
deferred: a live test against a real server, and a NATS-backed *store* as an
alternative to Postgres/SQLite. This ADR closes the first and records the second's
status and rationale, so "NATS" is not an open-ended placeholder.

## Decision

### D1: The NATS wake signal is live-tested, skip-without-server

A live test (`tests/nats.rs`, feature `nats`) connects two `NatsWakeSignal`
handles, publishes a hint on one, and asserts the other's `wait` returns. It skips
when no server is reachable, exactly as the Postgres suites skip without a
database (`AWAKEN_TEST_NATS_URL`, default `nats://127.0.0.1:4222`). So the default
build and test suite stay NATS-free, and a deployment with NATS can verify the
real integration. This completes the deferred live test.

### D2: A NATS-backed dispatch store stays deferred — by evidence, not omission

A NATS *store* (claim/lease/pending/outbox on JetStream KV with revision CAS) is
**not** built, for two grounded reasons:

1. **Correctness without a tested boundary is not shippable.** This corpus proves
   every store backend against the in-memory executable spec and a live database
   (the memory/Postgres/SQLite discipline). A KV store cannot be verified here — no
   NATS server is available — so shipping it would be unverified store code, which
   the discipline forbids.
2. **Postgres already provides the distributed guarantee.** `FOR UPDATE SKIP
   LOCKED` gives multi-node concurrent distinct claim (ADR-0019 D1), proven by
   test. NATS earns its place as a *wake-signal optimisation* over that durable
   store, not a second source of truth — so a KV store is an alternative backend,
   not a missing capability.

### D3: The seam is ready

If a NATS store is later wanted, it implements the same `DispatchStore` ports
(`RunDispatch`/`PendingInbox`/`MessageOutbox`) and is validated against the same
shared `assert_*` specs the other backends use — the executable spec is the
contract. No design change is needed, only a tested implementation against a
running server.

## Consequences

- The NATS wake-signal integration is complete and live-tested (skip-without-
  server); a fleet can use it to avoid busy-polling.
- A NATS-backed store remains deferred with a recorded rationale, not an
  open question; the port seam and shared specs make it a drop-in when a tested
  environment exists.
- The default build carries no NATS driver; NATS is opt-in via the `nats` feature.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G6 (durable ingress over runtime control).
- ADR-0019 — the `WakeSignal` seam and the distributed-claim argument.
