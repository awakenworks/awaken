# Worker-local credential liveness test design

## Objective and ownership

Worker-local credentials remain opaque to Control and to the generic runtime. The
Worker-side `CredentialMaterialResolver` is the sole adapter that can inspect the
local provider login. The Worker registry remains the sole published observation
store, and durable dispatch remains the sole placement/claim authority.

This slice closes the time-of-check/time-of-use gap without introducing a second
credential selector, health registry, or heartbeat:

```text
Provider adapter probe -> Worker observation snapshot -> registry/dispatch admission
                                                        -> exact use-time revalidation
```

The credential observation is a bounded fact, not a grant. Authorization, the
claim epoch, and the exact published `CredentialRef` remain independent inputs.

## Static structure

| Owner | Type/port | Responsibility |
| --- | --- | --- |
| Credential contract | `CredentialObservation` | Exact revision, typed provider state, and reason; never a lease or secret |
| Worker composition | credential probe supervisor | Runs independently of Worker heartbeats, stamps trusted observation/deadline times, and atomically publishes the latest complete snapshot |
| Worker contract | `WorkerCredentialObservation::is_selectable_at` | One freshness predicate shared by placement and claim |
| Worker registry | `WorkerSnapshot` | Replaces the observation set on heartbeat; never extends an observation deadline |
| Runtime materialization | `CredentialMaterialResolver` | Revalidates the exact Worker reference immediately before local use |

No token, auth-file bytes, local path, or provider-private state crosses these
boundaries.

## Dynamic behavior

```text
startup
  -> adapter probes its configured local references and reports a typed result per reference
  -> publish Ready + complete observation snapshot

resident operation
  -> probe supervisor refreshes its in-memory snapshot
  -> heartbeat publishes the latest snapshot without waiting for a provider probe
  -> Control admits only Available observations whose deadline is in the future

attempt execution
  -> placement checks Worker lease + exact fresh observation
  -> claim atomically checks the same predicate
  -> resolver re-probes/revalidates the exact reference before material use
  -> changed/missing/login-required state rejects before Agent launch
```

A failed probe blocks only the affected credential. It does not drain the Worker,
revoke unrelated sessions, or turn a previous `Available` observation into an
unbounded sticky fact. Loss of Worker registry authority still drains the Worker,
because that is a different liveness plane.

## Safety invariants

For Worker `w`, credential revision `c`, and time `t`:

```text
Selectable(w, c, t) ==
    WorkerLive(w, t)
    /\ WorkerAcceptsClaims(w)
    /\ ExistsExactObservation(w, c)
    /\ ObservationState(w, c) = Available
    /\ ObservationObservedAt(w, c) <= t
    /\ t < ObservationValidUntil(w, c)
```

Execution adds two more conjuncts:

```text
MayExecute(w, c, t) ==
    Selectable(w, c, t)
    /\ ClaimEpochIsCurrent(w, t)
    /\ ExactResolverRevalidationSucceeds(w, c, t)
```

Required properties:

1. An expired observation never admits placement or claim, even while the Worker
   lease remains live.
2. A nearby credential revision is never equivalent to the exact published one.
3. Probe failure for one credential never removes unrelated fresh credentials or
   Worker liveness.
4. A probe that stops making progress decays by deadline; no explicit negative
   message is required for safety.
5. A credential removed after placement but before launch fails use-time
   revalidation and never reaches the Agent process.
6. Heartbeat publication never manufactures a later credential deadline.

## Cause-effect and decision tables

### Placement and claim

| Rule | Worker live | Exact revision | State | Fresh | Result |
| --- | --- | --- | --- | --- | --- |
| P1 | no | any | any | any | reject |
| P2 | yes | no | Available | yes | reject |
| P3 | yes | yes | non-Available | yes | reject |
| P4 | yes | yes | Available | no | reject |
| P5 | yes | yes | Available | yes | admit |

### Probe publication

| Rule | Credential A | Credential B | Worker heartbeat | Published result |
| --- | --- | --- | --- | --- |
| H1 | Available | Available | succeeds | both fresh |
| H2 | ProbeFailed | Available | succeeds | A blocked, B selectable |
| H3 | probe task stalled | prior Available | succeeds | prior facts expire at their original deadlines |
| H4 | any | any | registry authority lost | Worker drains |

### Use-time revalidation

| Rule | Placement fact | Local state before launch | Result |
| --- | --- | --- | --- |
| U1 | fresh Available | same exact usable state | launch |
| U2 | fresh Available | logged out | `LoginRequired`, no launch |
| U3 | fresh Available | missing/invalid | typed rejection, no launch |
| U4 | fresh Available | another revision/account | revision/identity rejection, no launch |

## Test layers

1. **Contract unit tests** exhaust state, exact-revision, lower/upper deadline,
   and future-observation boundaries.
2. **Registry/HTTP integration tests** prove live Worker + stale credential is
   rejected and a later fresh heartbeat restores eligibility.
3. **Worker lifecycle tests** prove probe errors are credential-local and a
   stalled probe cannot stall heartbeats.
4. **Real-process E2E** runs the production cell and `awaken-worker` process,
   drives a mutable opaque test adapter through Available -> expired -> Available,
   and proves only the fresh window can claim/execute.
5. **TLA+ bounded model** explores Worker lease, credential deadline, probe
   success/failure, placement, claim, local loss, and use-time validation. It
   checks `ExpiredNeverExecutes`, `ExactRevisionOnly`, and
   `ProbeFailureDoesNotDrainWorker`.
