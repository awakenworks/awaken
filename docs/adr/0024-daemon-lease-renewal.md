# ADR-0024: Exact-Claim Lease-Renewal Ownership

- Status: Accepted
- Amended: 2026-08-31 — exact-claim renewal is the only local and remote
  Run-lease authority; the redundant owner-wide transport is removed.
- Amends: ADR-0019 — schedules the `renew_lease` operation it introduced; kept
  as its own number for history, but it is a refinement, not an independent
  decision.
- Date: 2026-06-30
- Depends on: ADR-0011, ADR-0019, ADR-0065

## Context

A Run can execute or resolve for longer than its dispatch lease. Without
renewal, another Worker can reclaim it and duplicate an external effect.

The original decision assigned an owner-wide heartbeat to `DispatchService`.
That description no longer matches the local or remote execution path:
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
Recovery may advance the mutation claim, but D5 still forbids the peer from
entering an external executor until predecessor quiescence is acknowledged.
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

### D3: Remote Workers renew each exact claim

A database-independent remote Worker cannot transfer a process-local guard into
the Coordinator, but it has the epoch-bearing claim returned by the signed claim
endpoint. Its remote guard therefore invokes the same `renew_lease` contract for
that exact claim. The Coordinator derives owner, lease duration, and time from
authenticated Worker authority. No owner-wide renewal verb, compatibility route,
or parallel daemon is retained.

### D4: Epoch-bearing renewal is a coordinated wire cutover

The authenticated remote request carries `run_id + lease_epoch`; the Control
side derives owner, lease duration, and time from trusted Worker authority.
Missing epoch fails closed rather than falling back to owner-only renewal. A
release containing this amendment therefore drains old Workers and deploys
Control plus Worker binaries as one coordinated maintenance cutover; no legacy
renewal endpoint or epoch-default compatibility path is retained.

### D5: The Dispatch aggregate also owns physical-attempt quiescence

A live claim does not by itself prove that a superseded model, tool, or Sandbox
future has returned. Each Dispatch row therefore carries at most one exact
`(active_attempt_owner, active_attempt_epoch)` slot. `begin_attempt` admits only
the current claim into an empty slot; an exact retry is idempotent; a successor
claim receives `Blocked` while the predecessor slot remains. `finish_attempt`
may clear only the matching exact slot, even after its mutation lease advanced.
It grants no commit, checkpoint, or settlement authority.

```text
claim A -> begin A -> external future A
lease A expires -> claim B
begin B -> Blocked
future A returns -> finish A
begin B -> Applied -> external future B
```

Only explicit return from the owned executor Future acknowledges quiescence.
Cooperative cancellation may cause that return; dropping or aborting the Future
does not prove an opaque remote request stopped. Task abort, process crash, Lease
expiry, and heartbeat expiry therefore leave the slot occupied. Recovery waits
for an explicit old-worker ACK, or for a future authority command backed by an
authoritative provider terminal receipt. Process/Pod termination can prove only
process-bound tool/Sandbox work. This chooses safety over availability when
external work cannot be proven stopped.

The slot is not another lease or state machine. Claim, attempt admission,
settlement, retry exhaustion, and dead-lettering mutate the same Dispatch row in
one backend transaction. `ThreadCommit` remains the sole execution-result truth.
Direct, non-durable ingress uses Runtime's one process-local Thread gate and
holds it across the complete executor Future. Durable `RunService::start` and
`resume` fail closed; their only legal path is durable submission and claim.

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

remote Worker claims with run_id + lease_epoch
  -> one remote exact-claim guard starts
  -> signed renewal carries run_id + lease_epoch
  -> Coordinator derives owner/time and renews only that claim

every local or remote durable executor call
  -> begin exact physical slot
  -> model/tool/Sandbox future returns
  -> finish exact physical slot
  -> then settle/relinquish may remove or release the row
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
- Remote topology uses the same signed exact-claim renewal; no owner-wide
  compatibility route remains.
- `renew_lease`, physical attempt admission, and quiescence are covered across
  Memory, SQLite, PostgreSQL, real HTTP, Worker, Pool, Direct, and TLA layers.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G6 (durable ingress additive over control).
- ADR-0011 — edge Clock injection and autonomous draining.
- ADR-0019 — exact `renew_lease` and distributed claim fencing.
- ADR-0065 — database-independent remote Worker topology.
