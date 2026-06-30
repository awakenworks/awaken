# ADR-0024: Daemon Lease-Renewal Heartbeat

- Status: Accepted
- Amends: ADR-0019 — schedules the `renew_lease` it introduced; kept as its own
  number for history, but it is a refinement, not an independent decision.
- Date: 2026-06-30
- Depends on: ADR-0011, ADR-0019

## Context

`renew_lease` (ADR-0019) extends one run's lease, but nothing called it on a
schedule, so a run that executes longer than its lease could be reclaimed by
another node's recovery mid-flight. The worker is deliberately clock-free
(ADR-0011) and drains runs synchronously, so it cannot renew a lease *while* it is
busy executing one. Renewal therefore belongs to the daemon, which holds the
clock.

## Decision

### D1: A bulk renew, owned by the lease owner

`renew_owned_leases(owner, lease_ms, now_ms)` renews every running dispatch held
by `owner` to `now_ms + lease_ms`, returning the count. The daemon does not track
individual in-flight run ids (the synchronous drain hides them), so it renews by
*owner* — exactly the set of runs this daemon is executing. It is a liveness
operation, not a claim decision, so it stays out of the deterministic claim path.

### D2: A separate heartbeat task, concurrent with the drain

The drain task is busy awaiting a long run, so renewal runs on its own
`tokio` task spawned beside it, sharing the shutdown token. Each tick it reads the
daemon's `Clock` and calls `renew_owned_leases`, keeping in-flight leases fresh
while the drain executes. The two tasks share nothing but the store and the
shutdown signal, so renewal never blocks the drain or vice versa.

### D3: Opt-in, well under the lease

`DispatchServiceConfig.lease_renewal_interval` is `None` by default — a single
in-process daemon has no peer to steal its runs, so it needs no renewal. A
multi-node deployment sets the interval well under the lease duration (e.g. a
third) so a renewal always lands before expiry. Renewal failure is swallowed like
the drain loop's other steps; the next tick retries, and a genuinely dead daemon
simply stops renewing and is recovered.

## Consequences

- A long in-flight run keeps its lease across a multi-node fleet and is not
  reclaimed while still executing.
- The worker stays clock-free and deterministic; only the daemon reads time.
- Opt-in: single-process daemons are unaffected.
- `renew_owned_leases` is proven across the three backends against one shared spec.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G6 (durable ingress additive over control).
- ADR-0011 — the clock-free worker / daemon-injects-time split.
- ADR-0019 — `renew_lease`, the single-run renewal this schedules in bulk.
