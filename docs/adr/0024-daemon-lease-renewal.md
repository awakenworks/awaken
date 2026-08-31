# ADR-0024: Exact-Claim Lease-Renewal Ownership

- Status: Accepted
- Amended: 2026-08-21 — exact-claim renewal replaces the former owner-wide
  local heartbeat while retaining the remote Worker transport adapter.
- Amends: ADR-0019 — schedules the `renew_lease` operation it introduced; kept
  as its own number for history, but it is a refinement, not an independent
  decision.
- Date: 2026-06-30
- Depends on: ADR-0011, ADR-0019, ADR-0065

## Context

A Run can execute or resolve for longer than its dispatch lease. Without
renewal, another Worker can reclaim it and duplicate an external effect.

The original decision assigned an owner-wide `renew_owned_leases` heartbeat to
`DispatchService`. That description no longer matches the local execution path:
the service synchronously awaits a drive, process pools resolve a Session Worker
after claiming, and foreground child execution bypasses both daemons. An
owner-wide loop also retains unrelated claims after their activity has ended.

`DispatchWorker` remains clock-free in the structural sense: it stores no Clock
and reads no private SystemClock. The edge that starts a drive owns one
`Arc<dyn Clock>` and passes that source through the whole claim lifecycle.

## Decision

### D1: One exact-claim guard owns local renewal

`renew_claim_while_active(store, claim, lease_ms, clock)` creates one renewal
guard for one fenced claim. Every interval (`lease_ms / 3`) it invokes
`renew_lease(claim, lease_ms, clock.now_ms())`. The complete `RunClaim`
(`run_id`, owner, monotonic `lease_epoch`) is the same fencing identity used by
commit and settlement; an owner name is never sufficient because a process
identity may eventually be reused. Losing ownership stops the task; dropping
the guard cancels it.

Each renewal request is bounded to half the regular interval. A transient
transport/store error retries on the same guard with a short bounded delay,
without moving the next regular renewal away from its absolute cadence. If no
successful renewal proves ownership by two thirds of one lease, the guard
cancels the attempt before a peer may legitimately recover the durable lease.
This local proof deadline is conservative signalling only; the dispatch store
remains the sole lease-expiry authority.

The guard, not a daemon-wide in-flight registry, is the renewal authority. A
direct service or foreground drive creates it at the canonical Worker drive. A
process pool creates it immediately after claim so slow Session/Environment
resolution is covered, then transfers that same guard into the Worker drive.
The Worker must not create a second Tokio task at that handoff. Resolution
failure, retry terminalization, and other Pool-owned non-execution branches keep
the Pool guard until that exact operation returns and then drop it.

### D2: One edge Clock spans claim, renewal, verification, and settlement

The service, pool, or foreground entry supplies a single `Arc<dyn Clock>` for an
operation. The same source determines:

1. claim eligibility and lease deadline;
2. renewal timestamps;
3. claim-bound pre-effect ownership verification;
4. retry deadlines and coordinated settlement fencing.

`DispatchWorker` has no `ownership_clock` field or clock-setting builder. A
scalar `now_ms` snapshot is insufficient because renewal and verification occur
later. Tests supply a `ManualClock`; production edges supply `SystemClock`.

### D3: Remote owner heartbeat is a transport adapter, not a local daemon

A database-independent remote Worker cannot transfer a process-local guard into
the Coordinator. Its signed Worker-control heartbeat therefore adapts the same
ownership responsibility to the existing `renew_owned_leases` transport verb.
The Coordinator validates the authenticated owner and reads its own authoritative
Clock. Local Service/Pool code must not call this bulk verb or run a parallel
owner-wide heartbeat.

### D4: Epoch-bearing renewal is a coordinated wire cutover

The authenticated remote request carries `run_id + lease_epoch`; the Control
side derives owner, lease duration, and time from trusted Worker authority.
Missing epoch fails closed rather than falling back to owner-only renewal. A
release containing this amendment therefore drains old Workers and deploys
Control plus Worker binaries as one coordinated maintenance cutover; no legacy
renewal endpoint or epoch-default compatibility path is retained.

## Dynamic behavior

```text
local edge claims with Clock C
  -> one exact guard starts with C
  -> optional Pool resolution retains that guard
  -> Pool transfers the guard and C to DispatchWorker
  -> exact epoch renewal succeeds on an absolute cadence
     or transient failure retries inside the safety window
     or lost/unprovable ownership cancels the attempt
  -> Worker verifies ownership with C before an external effect
  -> Worker settles with C
  -> guard drops and renewal stops

remote Worker heartbeat
  -> authenticated owner request
  -> Coordinator Clock supplies now
  -> existing bulk transport adapter renews that remote owner's claims
```

If renewal reports lost ownership, no replacement guard is created. The same
Clock makes the next ownership check fail closed, and claim/commit epoch fencing
prevents stale settlement.

## Consequences

- Long local Runs, foreground child Runs, cancellation, and slow resolution all
  retain their exact leases without an in-flight registry.
- A blocked renewal request cannot consume the whole lease, and transient
  failures do not wait one full regular interval before retrying.
- Pool-to-Worker handoff has one renewal task, not two concurrent writers.
- Deterministic claims are never compared with an unrelated wall clock.
- Remote topology retains its signed bulk transport for compatibility, while
  local execution has no owner-wide renewal loop.
- `renew_lease` and the remote-only bulk adapter remain covered across backends;
  cause/effect tests count local renewal writes to enforce task cardinality.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G6 (durable ingress additive over control).
- ADR-0011 — edge Clock injection and autonomous draining.
- ADR-0019 — exact `renew_lease` and distributed claim fencing.
- ADR-0065 — database-independent remote Worker topology.
