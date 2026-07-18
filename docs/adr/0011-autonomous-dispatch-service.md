# ADR-0011: An Autonomous Dispatch Service with a Clock at the Edge

- Status: Accepted
- Date: 2026-06-30
- Depends on: ADR-0009

## Context

The ADR-0009 worker only runs when a caller drives it: `submit_background`
inline-drives one run, and recovery happens only when someone calls `recover`.
A durable host needs the queue to keep moving on its own — draining new work and
reclaiming crashed leases without a caller — which is what makes a durable submit
truly fire-and-forget.

The reference (`awaken-worktrees/goal`) runs three background tasks (recover,
dispatch-signal, maintenance) plus a per-run lease-renewal manager and a
per-thread worker state machine. That is the right shape for a multi-node,
high-throughput deployment; it is more machinery than a single in-process host
needs.

## Decision

### D1: One daemon, woken by a nudge or a poll

`DispatchService` spawns one background task that drains the queue
(`run_until_idle`) and then waits on either a nudge (new work was submitted) or a
poll timeout, until shutdown. The poll is what reclaims a crashed lease without
new work; the nudge is what makes a submit responsive. One loop covers what the
reference splits across a recover task and a dispatch-signal task. Shutdown
cancels the loop and awaits the in-flight drain, so stop is clean.

### D2: The clock lives at the edge; the worker stays deterministic

The worker takes "now" as a parameter on every call, so it has no clock and is
fully deterministic and replayable. Only the daemon reads wall-clock time,
through a `Clock` port — `SystemClock` in production, `ManualClock` in tests. A
test drives recovery by hand: hold a lease, advance the clock past it, nudge, and
assert the run recovered — no sleeps, no flakiness in the logic under test.

### D3: Lease renewal and multi-worker concurrency are deferred

A single in-process daemon is the only claimant, so an in-flight run's lease is
never contended and needs no renewal; a generous lease plus poll-driven recovery
is sufficient. Per-run lease renewal, suspended (HITL) leases, per-thread worker
pools, and wake signals belong to the distributed milestone, where a second
claimant makes them necessary. `RunIngressCapabilities.scheduled_wake` stays
false until a durable timer lands.

## Consequences

- A durable submit is now fire-and-forget: `DispatchService::submit` enqueues and
  returns; the daemon runs it. `deliver` does the same for an aawaiting run's input.
- Crashed leases are recovered automatically on the poll cadence, not only when a
  caller asks.
- The deterministic worker plus the `Clock` port keep the daemon's tests fast and
  non-flaky, including the recovery path.
- `DurableRunIngress` still exposes the synchronous façade (inline `submit_background`,
  `deliver_resume`, `recover`); the daemon shares the same worker and store, so
  both paths flow through one queue.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G5, G6 (durable ingress is additive over
  runtime control).
- ADR-0009 — the worker and dispatch store this service drives.
- [run-ingress-message-delivery.md](../design/run-ingress-message-delivery.md) —
  the durable-ingress boundary.
