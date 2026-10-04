# ADR-0075: Unified Managed Session Worker Execution

- Status: Accepted
- Date: 2026-08-12
- Amended: 2026-08-28 — terminal Repository publication uses the same Worker realization owner
- Amended: 2026-08-29 — pre-realization cancellation does not fabricate a Runtime interval
- Amended: 2026-08-29 — unattempted Dispatch projection amendment is Coordinator-owned
- Amended: 2026-08-29 — profiled products submit typed Runs through canonical Session admission
- Amended: 2026-08-31 — registered Workers record terminal publication rejection
- Amended: 2026-09-06 — committed terminal Dispatch settlement survives Work retirement
- Amended: 2026-09-11 — recovery and realization bind the exact Session Work epoch
- Amended: 2026-10-01 — registered Worker preparation of a complete Session needs no model Run
- Supersedes: the late Worker-authored Session-input path in ADR-0063,
  ADR-0065, and ADR-0066
- Preserves: ADR-0065 claim recovery and attempt execution; ADR-0066 Session
  root CAS, MCP generations, and realization leases

## Context

### 2026-09-28: complete the profiled Run boundary

Profiled Run admission commits native Messages, not Managed input Events. An
input-envelope window in Managed history cannot identify the admitted Run, and
Session status cannot prove that its delivery authority has settled.

The Session application reuses `DispatchOperationalFeed` and its Runtime's
exact `RunRecoverySnapshot`. Canonical Coordinator composition binds the same
dispatch source once; absence and duplicate installation fail closed. Owner
validation precedes either read. Bounded pages contain only the requested
primary Run's existing facts, retain their original cursors, and derive output,
usage and the current ticket from one committed Thread prefix read after
observing dispatch. No state ledger or copied Managed input history is added.
Before the first Thread commit, the existing native admission activity receipt
proves exact Run membership. Only delivery facts are then available: committed
state/version remain explicitly absent and the lifecycle cursor is unchanged.
Consumers retain the `Settled(Done)` fence before projecting terminal completion.

The private profiled Session boundary gains exact Run event and reply routes.
Replies lower to the existing Session Thread reply command, activity transfer,
transactional continuation Outbox/Inbox and committed `ResumeApplied` receipt.
Free-form input extends that same closed reply value and reuses the native
resume validator. Budget continuation retains its separate Session authority.

Unchanged owners are Session authorization/activity CAS, committed Run state,
dispatch operations, resume validation and continuation delivery. Modified
owners are Session read/reply composition and the profiled protocol adapter.
New code is limited to native transport DTOs/routes and their bounded read
projection. The static boundary is product -> Session application -> existing
Thread/dispatch/continuation authorities. The sequence is owner check -> exact
native page -> caller projection; reply is owner check -> committed ticket ->
activity transfer -> durable delivery -> Worker resume -> committed receipt.
Dependency failure preserves the same operation; stale or foreign input is
rejected before delivery.

Test comments own the cause/effect rules for no Managed input Events,
exact/foreign Session and Run, bounded/empty pages, original cursor replay,
settlement fencing, terminal causes and exact/replayed/changed/stale replies.
Real-model product delivery remains an independent acceptance requirement.


### Duplication review

Managed Agents already defines the external execution boundary: a complete
Session is created under Control, a self-hosted Environment receives one
durable Work item, and a custom Worker claims that item. Awaken also had a
second path in which a registered Worker supplied mounts, environment values,
prompts, network policy, Resources, and MCP inputs after Session creation.

The two paths overlapped in placement, desired-state authoring, retry identity,
and realization. A marker excluded the late-input Session from the normal
Environment WorkQueue, while a separate Worker-to-Control command, receipt,
provisioner, and HTTP endpoint completed it through Run dispatch. That made the
same business capability depend on two aggregate transitions and two queue
interpretations.

The authoritative owners found by the code review are:

| Responsibility | Authoritative owner | Reused mechanism |
|---|---|---|
| Session purpose and immutable execution specification | `SessionApplication` and `PersistedSession` | `SessionCreationIntent`, root CAS, frozen `SessionBaseline` |
| Self-hosted placement | executable Environment and `WorkQueue` | `enqueue_session_work` and its idempotent reconciler |
| Attempt assignment | `DispatchQueue` | exact owner, epoch, and live Run claim |
| Physical projection | Worker Runtime Host | `SessionRealizationControl` and `drive_session_realization` |
| Backend execution | Runtime Host | one `RunAttemptExecutor` route |

No additional aggregate, queue, registry, receipt family, or local Worker plan
is required.

## Decision

### D1: Session creation receives complete desired state

Every caller supplies immutable mounts, environment values, prompts, network
restriction, Resources, and initial MCP candidates before invoking the one
SessionApplication creation protocol. `SessionCreationIntent::finalize`
normalizes MCP precedence and compiles the baseline before the root is inserted.

The first durable insert already contains the consumed complete intent, frozen
baseline, initial Resource/MCP state, and budget policy. `Preparing` is only the
execution state awaiting physical realization; there is no partially-authored
creation row and no post-insert finalization CAS. A Worker cannot add or replace
Session inputs.

### D2: Environment WorkQueue is the sole Session Worker placement path

A frozen, nonterminal Session whose Environment is self-hosted always projects
to its Environment WorkQueue. Placement does not depend on caller identity,
application registration, or the presence of a local Worker decorator.

The stable Work id is the long-lived Session execution ownership coordinate.
Enqueue replay is idempotent and reconciliation reconstructs a missing
projection from the durable Session. Reconciliation never resurrects a stopped
item: only a newly admitted Session event may explicitly wake it. A terminal
Session retires the item and clears its lease.

At the Managed HTTP edge, the authenticated Environment credential is the
stable lease authority. The official `WorkPoller` sends
`Anthropic-Worker-ID` on `poll` for poller identity and metrics, while its
`ack`, `heartbeat`, and `stop` calls retain the Environment bearer but omit that
header. The generated raw `work.poll()` method also makes Worker ID optional
and may carry the same credential as `X-Api-Key`. The adapter stores only a
domain-separated credential fingerprint as owner. When present, Worker ID is a
separate ephemeral poller observation; when omitted, the opaque owner is the
fallback observation coordinate. Header-only callers use Worker ID for both
roles. All forms enter the same atomic `WorkQueue` claim and mutation fence; no
credential map or second lease registry exists.

The claim of a Session Work additionally returns the official one-time
base64url Work `secret`, whose JSON payload contains a random
`sessions_token`. The Environment execution application mints the token and
its domain-separated SHA-256 digest; the WorkQueue receives only the digest,
stores it on the same Work row in the claim transaction, and binds it to the
current owner, lease epoch, and expiry.
The token is never returned by list/retrieve, is rotated on reclaim, and is
cleared by stop, wake, release, retirement, or expiry. HealthCheck Work never
receives one. The outer Managed capability edge maps a current token only to
that Work, its Session event surface, the Session's frozen Skill versions, and
its attached MemoryStores (including read-only write denial). Generic local or
Cloud IAM remains authoritative for every other route and credential.

This token is not the Environment credential, a registered-Worker identity, an
egress credential, or a model/provider secret. Native Awaken Workers continue
to realize Memory through the canonical `MemoryStoreMounter`; only an external
official `EnvironmentWorker` uses its SDK Memory client, against the same
MemoryRepository API. One execution therefore never runs two Memory sync
owners, and a Work token cannot be reused as an outbound gateway capability.
Every token-authenticated ack, heartbeat, and stop compares the recovered owner
and lease epoch inside the WorkQueue mutation transaction, so an in-flight
request from a reclaimed token cannot mutate its replacement lease.

### D3: a Run claim is an attempt fence, not another placement decision

When Awaken's registered Worker transport executes a Run for that Session, the
Run claim scopes one attempt under the already-selected Session Work. It proves
the exact Worker incarnation and epoch allowed to mutate physical realization.
It does not create Session desired state and does not compete with WorkQueue
placement. Registered dispatch atomically acquires that exact Session item from
the same `WorkQueue`; an official/custom Worker holding it wins, and the
registered attempt is rejected. Run claim checks and realization renewal renew
the same Work lease, while every realization phase verifies the exact owner.
`SessionWorkAcquisition` makes the trigger explicit: a live `ClaimedRun` may
repair a stopped item when predecessor retirement wins a wake race, but
`RealizationRenewal` cannot resurrect stopped Work after its Run settled.
Delegated children use the existing `RunDispatch::session_thread_id()` parent
affinity and borrow that Work; child settlement retains it for the waiting
parent, while a root Run owns its release.

The claim response carries an opaque `SessionWorkAuthorityReceipt` minted only
from that canonical Work acquisition. It binds the Session, registered Worker
incarnation, Work id, Environment id, and lease epoch. The Worker must return
the receipt on frozen-Session realization and committed recovery reads. Those
endpoints hold the exact Dispatch claim guard, re-read the canonical Work row,
compare every stable receipt coordinate, and reject before reading committed
Thread truth or starting realization when the receipt is missing, foreign,
stale, or unavailable. Reclaim by the same owner advances the Work epoch, so an
old receipt cannot survive same-owner ABA. The receipt is transport evidence,
not a second Work store: decoded bytes remain untrusted until this comparison,
and ordinary non-Session dispatch never accepts one.

This check does not make Work and Thread persistence one transaction. The
renewed Work lease supplies the immediate admission window; database transaction
correctness, asynchronous scheduling after admission, explicit revocation after
the comparison, and the committed Thread snapshot remain their existing trust
boundaries.
After claim-fenced effects commit, private registered dispatch releases that
Session Work immediately before settling the subordinate Run. Public custom
Workers retain the official explicit `stop` operation. Graceful registered
Worker deregistration releases any residual active Session Work for that exact
incarnation; crash recovery remains lease-expiry based. A later admitted event
wakes the same stable Work item.

Claim-authenticated frozen Session resume normally reassigns the realization
lease through the existing Session root CAS, including when owner and process
incarnation are unchanged. Normal Work retirement can already have closed that
process-local predecessor while its durable lease is still live. Returning the
same epoch would make the next approval continuation reuse a closed authority;
the successor epoch also excludes delayed predecessor renewal/retirement from
the new Resource preparation.

An unfinished retry of the same Run may instead supply its exact, still-live
process-local realization lease. The Host selects this continuation under the
existing slot lock only for the same Run and Worker incarnation; a cold,
expired, closed, or different-Run slot cannot supply it. Coordinator validates
the authenticated Worker and current Work/Dispatch authority, renews through
the existing exact-fence CAS, then resumes without requesting reassignment.
Both Coordinator and transport client reject a returned generation different
from the assertion, including a replacement between renewal and projection
loading. A rejected continuation never silently falls back to reassignment.
The incoming claim is published with the admitted frozen projection under the
same slot lock, not before the Control request. Otherwise a failed request for
a new Run could relabel the predecessor's live lease and make the next retry
assert a false same-Run continuation. Resource preparation can expose renewable
lease authority earlier, but cannot publish that Run-to-projection association.
This preserves the creation effect identity during a live retry without
reopening retired local authority. It does not recover historical creation
effects after lease expiry or Worker death; those still require their own exact
physical evidence. Ordinary application replay without explicit reassignment
continues to reuse the exact live owner/incarnation. Neither path changes Work
identity or the claim/physical-quiescence requirements.

The bounded `SessionRealizationContinuation` composition reuses
`SessionRealizationAuthority` transitions and adds only Run/request observers.
Its decision rules cover failed new-Run requests, atomic fresh publication,
same-Run continuation, and close/replacement during response delivery. The
existing formal mutation gate checks early claim publication, missing closed
authority rejection, and missing generation comparison, plus a reachable valid
continuation. This is finite protocol evidence, not Rust refinement or proof of
authenticated transport, database CAS, physical stopping, or crash recovery.

### D4: Worker Control realizes only committed truth

The Worker resumes an already-frozen projection through the claim-fenced
Session Control endpoint. It may stage, activate, publish, acknowledge, or fail
physical effects under `SessionRealizationLease`. A separate lease-only renew
command extends the exact same fence and resident Runtime projection; it cannot
create/reassign a projection or replay Stage/Publish. The Worker must fail
closed if Control has no frozen Session or if the claimed Resource projection
differs. An effect-capable Control projection that conflicts with the immutable
claimed Resource envelope is a terminal resolution failure: the existing
claim-fenced failure commit settles the Run and releases its queue position.
It must not relinquish and repeatedly claim the same incompatible envelope,
nor substitute newly authored resources into that Run. Temporary Control
unavailability and ownership contention retain their contract retry disposition.

A lease-only response is applied only to its unchanged local assertion: the
slot compare/write is atomic and cannot recreate a removed projection, regress
a later expiry, or overwrite a replacement generation. Failure-driven
revocation rechecks the same assertion under the existing realization/lifecycle
locks before interruption and retirement. A delayed response is not authority
over a newer projection; the durable Work/Run fences remain mandatory for its
execution. This uses the same slot and cleanup path, with no grace period or
additional lease registry.

After complete Resource/layout preflight, the same slot exposes the assigned
realization lease **before** the first awaited Resource/Skill read. A scoped
preparation cancellation signal carries no lease or execution authority: the
existing lease-only scheduler renews the slot, and baseline/runtime/admission
remain unpublished until preparation succeeds. The read lifetime follows the
current same-generation expiry, not the original response's deadline. Explicit
ownership loss (including Worker-wide revocation) cancels preparation before
waiting for its realization lock; interruption/retirement still rechecks the
exact assertion after taking the existing owner locks. Resource cache writes
check preparation authority atomically with their slot publication. Final
projection publication rechecks the scoped preparation and preserves a renewed
expiry. Failure/drop removes only a newly introduced unpublished lease, never a
replacement owner; an existing resident lease remains available to its normal
fenced cleanup path. No grace period, second renewal loop, or recovery queue is
introduced.

The attached cause/effect tables in `host/tests/resource_staging_lease.rs` and
`awaken-dispatch/tests/any_store/resolution_failure.rs` cover the read/renewal
and terminal-commit failure boundaries. `SessionRealizationMutex` checks the
bounded preparation/renewal/cancellation ordering; `SessionRunProtocolResolution`
uses the existing Run/Session/Work kernels for conflict exclusion and committed
child-Run settlement under fair observation/storage progress. Mutation checks
reject a hidden preparation lease, cancelled publication, and conflict execution;
a settlement reachability witness prevents vacuous success. These are bounded
model checks plus executable tests, not a Rust-to-model refinement proof or a
guarantee of progress during a permanent Control/database outage.

The process-local Session slot remains the only realization owner. Claimed Run
admission publishes its claim after acquiring that slot's realization fence;
retirement that removed the slot requires acquisition of the replacement fence.
Lease-only heartbeat I/O remains independent of a slow Stage/Publish operation.
Its response may update only the exact selected slot and lease tuple, including
expiry. Conclusive loss acquires the realization fence and checks that identity
again before interrupting and retiring the projection. An obsolete response
cannot overwrite a newer lease, remove a new claim, or recreate a removed slot.
Temporary failure retains only an unexpired, still-current lease. No additional
lease registry or realization driver participates in this ordering.

The Coordinator's `Dispatch` projection is not a Worker realization receipt.
Before any external attempt observes a pending Resource generation, the Session
aggregate may revise that generation in place; the Coordinator may therefore
replace its own same-revision, unclaimed projection with the complete revised
manifest. `Realization` mode never receives this authority: a claimed Worker may
install an identical projection idempotently or a strictly newer generation,
but same-revision different content fails closed. This is one explicit install
mode decision, not a content comparison fallback or a second Resource state.

There is no late-input command, contribution receipt, provisioner, special MCP
origin, public creation flag, or contribution-specific Worker endpoint.

### D5: Managed Agents compatibility is structural

The public Managed Session create shape stays aligned with Managed Agents.
Self-hosted/custom Workers use the existing Environment Work endpoints. Awaken's
registered Worker is an optional transport/runtime adapter over the same frozen
Session and does not require a public Session extension. Cloud-only usage
accounting remains outside this decision.

## Static structure view

```text
Client / product adapter
        |
        | complete create command
        v
SessionApplication -------------------- ManagedSessionRepository
        | compile, then one insert              one Session truth
        |
        +-- local Environment ----------> local realization
        |
        `-- self-hosted Environment ----> Environment WorkQueue
                                              |
                                              | Work lease
                                              v
                                      custom Worker or registered Worker
                                              |             |
                                standard Work API       exact Work acquire
                                                            + Run claim
                                              \             /
                                               v           v
SessionRealizationControl <----------- Worker Runtime Host
        | frozen projection                   |
        `------------------------------> drive_session_realization
                                               |
                                               `-> RunAttemptExecutor
```

Dependencies point toward contracts owned by the relevant bounded context.
The Worker depends on a frozen projection and effect ports; it does not depend
on Session authoring DTOs or the Session repository.

## Dynamic behavior view

```text
create trigger
  -> resolve Agent + exact Environment revision
  -> normalize all initial MCP candidates
  -> compile immutable baseline
  -> insert one complete frozen Session root (execution=Preparing)
  -> local: realize and acknowledge -> Idle
     self-hosted: enqueue stable Work item -> Preparing until Worker realization

Worker trigger
  -> Managed edge separates poller observation from credential-derived owner
  -> claim Work under that one owner
  -> Session Work only: mint one epoch-bound sessions token; persist its digest
  -> outer capability edge maps that token to exact Session/Skill/Memory routes
  -> registered adapter only: atomically acquire the exact same Work item
  -> optionally claim a subordinate Run attempt
  -> authenticate Worker incarnation and verify live Work + Run epochs
  -> claimed Run only: repair a stopped item and retry once after a lost wake
     realization renewal: never wake stopped Work
  -> delegated child: use parent session_thread_id and retain Work on child settle
  -> resume frozen Session projection
  -> acquire per-Session realization admission
  -> stage physical effects
  -> activate/publish
  -> acknowledge exact receipts -> Idle/Running
  -> later admitted event: explicitly wake stopped Work
  -> terminal Session: retire Work and clear its lease
```

Failure and retry rules:

- create-time normalization conflicts fail before any Session row is inserted;
- a WorkQueue write failure leaves durable Session truth reconcilable and does
  not manufacture readiness;
- stale Work/Run ownership or a mismatched Environment bearer fails before
  Worker effects;
- a missing, wrong, expired, reclaimed, or stopped Work sessions token is
  rejected before Session/Skill/Memory access; read-only Memory rejects writes;
- a token request authenticated just before reclaim still fails its atomic
  Work mutation when the persisted lease epoch has advanced;
- response loss replays the same Work id, root revision, realization lease, and
  generation receipts;
- if an externally realized Session admits an activity while `Preparing` or
  `Activating` and that Run is cancelled before a customer-visible `Running`
  interval opens, settlement consumes the exact activity epoch without
  fabricating a Runtime interval; every other Run boundary must still belong to
  the active interval;
- a physical effect failure records the existing realization failure state and
  is retried only through the same phase driver;
- archive/delete/termination remains the sole terminal Session path, retires
  Work, and is retried by the same reconciliation scan after response loss.
- Work retirement fences further execution, but cannot prevent closure of an
  already committed terminal Run. The existing Worker settlement endpoint may
  finish `Done` after Work is retired, reassigned, or lost during release only
  when the authenticated current Dispatch claim has a matching recovery snapshot
  proving that exact Run is `Ended`. It never releases another owner's Work or
  revives stopped Work. Missing/nonterminal proof retains the live Work
  requirement; Dispatch settlement still checks its epoch after the recovery
  guard is dropped. Work storage errors remain retryable failures, including a
  lost release response; the retry uses the same committed Run and Dispatch.
  This prevents terminal cleanup's retire-then-quiesce sequence from waiting
  forever on a Dispatch whose completed Runtime effects already justify closure.

## FMECA and cause-effect analysis

The cause graph has four serial authority gates: complete durable Session truth
(`S`), one Environment Work lease (`W`), an optional subordinate Run claim
(`R`), and one realization lease/generation (`P`). A physical effect is legal
only when `S ∧ W ∧ R ∧ P` is true; Cloud/non-Session execution removes `W`
from the conjunction rather than manufacturing an empty Work lease. Terminal
truth makes every execution gate false and drives Work retirement.

Severity (S), occurrence (O), and detection difficulty (D) use 1–10 scales;
RPN is the pre-mitigation product. The listed mitigation is part of the
authoritative path, not a compensating parallel mechanism.

| Failure mode | Local effect / end effect | S/O/D · RPN | Detection | Authoritative mitigation and terminal outcome |
|---|---|---:|---|---|
| Incomplete or conflicting create input | orphaned or ambiguous desired state | 9/3/6 · 162 | complete-create decision tests | normalize/compile before persistence; first insert is already frozen; invalid input leaves no row |
| Crash or lost response after Session insert | caller retries while realization is absent | 8/4/4 · 128 | repository idempotency and injected later-CAS failure | replay the same root/key; reconciler projects the frozen truth; no partial creation state exists |
| Work enqueue outage or lost response | self-hosted Session remains Preparing | 8/4/3 · 96 | dispatch failure classification and reconciliation report | return `session_work_dispatch_failed`; stable idempotent Work id is retried; never report false readiness |
| Reconciler revives completed Work | idle Worker loops forever | 7/4/5 · 140 | stopped/enqueue/wake conformance rule | idempotent enqueue preserves `Stopped`; only a driving event calls explicit wake |
| Missing identity, mismatched Environment credential, or Worker ID incorrectly required by the raw/helper SDK lifecycle | official clients cannot claim/finish Work, anonymous owners collide, or the current owner is disrupted | 10/3/4 · 120 | credential/Worker cause-effect table, raw client and WorkPoller E2E, cross-backend owner-fence tests, HTTP 400/412 | derive the lease owner from the authenticated credential fingerprint; treat Worker ID as an optional observation label; allow unlabeled SDK operations only under that same credential; atomically reject mismatched/missing authority without state change |
| Worker crashes or a response is replayed after reclaim | two Workers execute one Session | 10/4/5 · 200 | expiry/epoch conformance | expiry returns item to queued; next claim increments monotonic epoch; stale owner/epoch cannot mutate |
| Official/custom and registered Workers race | parallel execution paths | 10/3/6 · 180 | exact acquire contention test | both contend in the same WorkQueue transaction; exactly one lease wins; loser fails before Session Control |
| Run is claimed without its Session Work, or Work owner changes | subordinate attempt escapes placement authority | 10/3/6 · 180 | signed Worker rules T17–T19 | resume atomically acquires exact Work; claim checks/renewal extend it; every phase verifies exact incarnation owner |
| Run claim expires or is replaced mid-operation | stale attempt commits effects | 10/4/4 · 160 | stale/expired signed claim tests | exact owner+epoch guard is held across admission/commit; check before and after realization assignment; reject stale attempt |
| Realization lease/generation is stale | stale physical Resource/MCP publication | 10/3/4 · 120 | realization generation and receipt tests | phase commands require live exact realization lease and generation receipts; stale writes are no-ops/rejected |
| Physical effect succeeds but acknowledgement is lost | duplicate provisioning/publication | 8/4/4 · 128 | phase replay tests | replay the same phase/generation and compare exact receipts; publish only post-CAS Active truth |
| Session becomes terminal while Work is active | leaked Work or post-terminal execution | 10/3/5 · 150 | terminal retirement and stale settlement tests | terminal root CAS fences activity; coordinator retires Work and clears lease; reconciler retries cleanup |
| Work/repository storage is unavailable, corrupt, or its epoch is exhausted | authority cannot be proven or a fence could repeat | 9/3/3 · 81 | typed storage-failure and epoch-boundary tests | fail closed with service/storage error; never substitute empty ownership, zero, or a saturated epoch; retry outages, quarantine corrupt Session truth |
| Ordinary Cloud or non-Session Run reaches shared verifier | false rejection from an unrelated Work boundary | 6/4/5 · 120 | authority scope rules W1–W3 | missing/Cloud Session returns `NotRequired`; only frozen self-hosted Session requires Work ownership |
| Registered Worker answer commits immediately before process restart | predecessor Work remains active and blocks every queued Session for one TTL | 8/4/4 · 128 | signed transport T20/T21, WorkQueue G1/G2, multi-restart CLI E2E | exact Run settlement releases its Session; exact graceful deregistration stops any residual owner rows; crash path retains TTL fencing |
| Successor wake loses to predecessor retirement | approved/queued Run remains behind stopped Work | 9/3/5 · 135 | Session Work W3 and real same-Session continuation | live `ClaimedRun` reuses canonical wake after failed acquire and retries once; no second queue/state exists |
| Realization renewal revives stopped Work after Run settlement | orphan active lease blocks every Session in the Environment | 9/3/5 · 135 | Session Work W5 and Native final-state inspection | `RealizationRenewal` may renew dispatched Work but never wake stopped Work; it stays unowned and the stale local projection is revoked |
| Child Run uses child thread for Work or releases borrowed parent Work | parent/child deadlock or waiting parent loses its fence | 10/2/5 · 100 | signed transport T22/T23 and durable child-run regression | use canonical `session_thread_id()` for Work/resume; only a root Run whose own thread equals that affinity releases |

The reduced decision table for the interacting ownership causes is:

| Rule | Frozen self-hosted Session | Work owner exact/live | Run exact/live | Realization exact/live | Effect |
|---|---|---|---|---|---|
| F1 | no | n/a | exact | exact | execute without Work adapter (Cloud/non-Session) |
| F2 | yes | no | any | any | reject before physical effects |
| F3 | yes | yes | no | any | reject before physical effects |
| F4 | yes | yes | yes | no | reject phase before physical effects |
| F5 | yes | yes | yes | yes | execute one phase and persist exact receipt |
| F6 | terminal | any | any | any | reject execution; retire Work |
| F7 | yes | exact predecessor | committed/settling | exact | release Work, then settle Run |
| F8 | yes | exact graceful incarnation | any residual | any | deregistration releases only that incarnation |
| F9 | yes/stopped | claimed successor | exact/live | any | wake canonical item, retry once, then lease or remain unowned |
| F10 | yes/stopped | realization renewal | n/a | any | remain unowned; never resurrect |
| F11 | delegated child | parent Work exact/live | child claim exact/live | exact | resume through parent affinity; retain Work on child settle |

## Implementation classification

### Reused unchanged

- `ManagedSessionRepository`, root revision CAS, lifecycle outbox, Resource
  state, MCP generation state, and `SessionRealizationLease` remain the durable
  authorities.
- Environment `WorkQueue`, its HTTP API, stores, and one-active-item rule remain
  the placement mechanism.
- `DispatchQueue`, Worker authentication, recovery, and the backend executor
  registry remain the attempt mechanisms.
- `drive_session_realization` remains the sole physical phase driver.

### Modified

- `SessionCreationIntent` contains only complete Control inputs and finalizes in
  one step.
- profiled/Dream creation supplies local inputs up front.
- Session creation inserts complete frozen truth once; every eligible
  self-hosted Session uses `needs_work_dispatch` without a caller-specific
  exclusion.
- Managed poll separates the optional SDK Worker ID observation from the
  authenticated Environment credential fingerprint used by the same
  owner-fenced Work mutations; stopped Work has explicit wake semantics and
  terminal Work has coordinator-owned retirement.
- registered dispatch acquires/renews/verifies the exact Session Work owner
  before using its existing Run and realization fences, then releases it on
  exact settlement or graceful incarnation deregistration.
- the same authority classifies `ClaimedRun` versus `RealizationRenewal`, and
  registered transport consistently uses `RunDispatch::session_thread_id()`;
  no wake flag, child lease table, or release counter is persisted.
- Worker Control naming and transport express frozen Session realization.

### Added

`SessionWorkLeaseAuthority` is a narrow internal adapter over the existing
`WorkQueue`; `WorkMutationResult` exposes its existing atomic mutation outcome.
The existing claim input now carries the lease owner and poller observation
separately. None adds storage, a registry, a poller, or another source of truth.
New decision-table coverage and this canonical decision record verify the
adapter.

### Removed

- the late Worker-authored Session-input domain types and persistence fields;
- the public create flag and special MCP origin;
- the Worker provisioner and late-input HTTP/client endpoint;
- the special example/E2E path and compatibility snapshots for that surface.

## Verification model

Tests attach their cause/effect tables to the owning cases. Required rules are:

| Rule | Complete input | Self-hosted | Claim current | Frozen | Effect |
|---|---|---|---|---|---|
| U1 | yes | no | n/a | yes | local realization, one idle fact |
| U2 | yes | yes | n/a | yes | one idempotent Work projection, no false idle |
| U3 | yes | yes | yes | yes | resume and realize exact projection |
| U4 | yes | yes | no | yes | reject before physical effects |
| U5 | yes | yes | yes | no | fail closed as not ready |
| U6 | conflicting MCP | any | n/a | no | reject before insertion |
| U7 | yes | yes | settled | yes | release exact Work; next queued Session runs |
| U8 | yes | yes | restart before settle | yes | deregistration releases predecessor immediately |
| U9 | yes | stopped | claimed successor | yes | wake once and acquire; renewal stays unowned |
| U10 | child affinity | yes | child exact | yes | parent Work is renewed and retained until root settlement |
| U11 | unattempted same-revision manifest amendment | yes | no external claim | yes | Coordinator replaces only its Dispatch projection; Worker replacement is rejected |

## Consequences

There is one source of Session desired state and one Session Worker placement
path. Local and custom Workers differ only in adapters and physical effects.
Applications remain free to build complete Session commands and to decorate the
neutral attempt executor, but they cannot mutate Session truth from a Worker.

## Amendment (2026-08-13): profiled creation accepts product MCP candidates

A product adapter that creates a Session from an immutable Agent publication
supplies its explicit Session MCP inputs on the existing
`CreateProfiledSessionCommand`. The sole `create_profiled_session` composer
joins those candidates with the publication's Agent candidates, invokes the one
`normalize_mcp_drafts` path once, and leaves Session-over-Agent precedence and
target uniqueness to `SessionCreationIntent::finalize`.

```text
published Agent MCP --\
                       +-> create_profiled_session -> normalize once -> finalize once -> root insert
product Session MCP --/
```

The command carries raw protocol-neutral candidates, not normalized drafts.
Products must not pre-read the Agent profile, pre-merge candidates, or reproduce
URL/stdio/credential normalization. An invalid candidate or equal-origin name
conflict fails before insertion; a Session candidate with the same logical name
as an Agent candidate replaces it independent of input order. The persisted
frozen MCP set remains the only desired-state authority.

Hosted product composition uses the explicitly namespaced Awaken extension
`POST /v1/awaken/sessions` to deliver that same complete command. The extension
owns only strong wire types and lowering; it calls the existing
`create_profiled_session` composer and cannot create, update, or realize a
Session independently. Embedded products call the composer in process. Thus
placement changes transport only: mounts, environment variables, prompts, MCP
credential references, and network policy are frozen by the same application
owner before the root insert.

The HTTP edge accepts both a first create and an exact receipt replay with
`202 Accepted` as soon as that complete root is durable. The returned Session id
is the asynchronous operation identity; no Job record exists. Neither response
waits for disposable Managed projection or physical realization, and an exact
replay of `Preparing` or `ActivationFailed` returns the same root identity
without repeating effects. Subsequent Session reads project the root's current
state and durable failure reason.

Profiled Repository inputs use the same rule. The extension may carry a
Repository URL, mount path, and exact secret-free `CredentialRef`, but it lowers
them into the existing Resource Catalog and Session credential pin before the
root insert. The existing `RepositoryRealizer` remains the only checkout and
publication mechanism. A raw `MountSource::Secret` is not a distributed
Repository credential: a database-less Worker consumes the recipient-bound
`CredentialAccess` produced by the Session application, never a product-owned
secret resolver or a second Git delivery path.

## Amendment (2026-08-15): Worker projection carries the exact Agent publication

A registered Worker remains authority-store-free, so the initial Session
realization cannot reopen Control's publication store and cannot wait for a Run
claim that is admitted only after the Session exists. The existing Coordinator
catalog therefore projects the complete exact executable snapshot selected by
the Session baseline into the frozen Session projection.

The snapshot is rebuildable transport input, not Session state. The Session
aggregate continues to persist only the Agent identity, source revision and
backend projection; Control remains the publication authority. A Worker retains
the delivered snapshot for execution and recovery, while lease renewal updates
only its exact resident fence. An overlapping claimed Run must carry the same
snapshot or fail closed before any effect. Historical baselines without an
exact revision retain their existing compatibility behavior.

## Amendment (2026-08-24): Session root retains canonical runtime intervals

Managed event history is a projection of the existing Session, Thread, Run,
message, and audit facts. The Managed protocol cache and lifecycle outbox are
not event stores: the outbox remains delete-after-delivery, while the Session
root CAS retains every closed customer-visible Running interval for the Session
lifetime.

A historical Awaiting target remains tool-classification evidence after a
later failure or cancellation clears the current ResumeTicket. Root and child
transcripts use the same occurrence selector and encoder: an unmatched intent
without a result or committed wait stays hidden, while a committed wait keeps
its original source-qualified public tool event. Internal ticket names can
differ from model-facing aliases; exact Thread, committed source Run, call id,
and arguments bind that alias without consulting mutable configuration. Legacy
messages without source Run coordinates still require exact name matching.
This is a disposable projection of existing facts, not a new ticket store or
permission decision, and it never invents a ToolResult for unexecuted work.

ACP permission suspension preserves the same distinction during bounded process
settlement. Cancelling one pending request does not invalidate a sibling tool's
already observed execution. The existing response pump retains a late terminal
result only for a previously observed, still-unsettled sibling with unchanged
arguments; it uses the original fact sink and monotonically increasing sequence.
The Await target, unknown/new calls, repeated terminal facts, changed call
identities and assistant text remain excluded. Later permission requests are
rejected without invoking the original policy or minting grants. Prompt facts
and permission requests must also match the active ACP Session identity. These facts
join the existing executor ThreadCommit alongside the unchanged ResumeTicket;
neither the protocol map nor the external SDK transcript becomes another store.
EOF, malformed frames or a settlement deadline do not invent missing results or
replace the original Await outcome. This closes loss of observed late facts; it
does not prove receipt of every opaque side effect across physical process loss.

Each interval freezes its opening/closing root revisions, exact Runtime
lifecycle observations, cumulative neutral usage, and historical budget limit.
Every accepted Event batch freezes the Runtime store high-water observed before
its Session root CAS. That admission coordinate makes the complete inbound DTO
immediately listable at one immutable public position. The existing Event-entry
processed CAS separately freezes the Runtime commit anchor that observed its
effect; it may add `processed_at` but cannot relocate the inbound Event.
`state_command.commit_sequence` remains the corresponding effect coordinate for
compaction, Outcome, and disposition facts. Admission revisions remain root
provenance and are never compared with Runtime commit coordinates. One interval
therefore projects exactly one aggregate
Running/Usage/Idle bracket, including child-only continuations and multiple
resumes of the same Run.

The projector consumes only recovery snapshots fenced at those committed
boundaries. Persisted accepted entries without an admission coordinate are
invalid rather than routed through a compatibility projector, and a final
Session revision reread discards a mixed root/Runtime snapshot. Budget-reached
events first become listable at the first closed interval whose cumulative
usage covers that exact transition. Parent terminal events first become
listable only after the existing terminal-cleanup quiescence seam persists the
Runtime commit high-water in the Session root. A later refresh may append a
higher coordinate but cannot insert a new durable fact before a previously
listable durable cursor. Process-local `session.updated` is held behind its
accepted durable predecessor and remains outside the cross-replica guarantee;
the live deletion notification retains its existing best-effort semantics.

This aggregate JSON expansion is not mixed-writer compatible:
`PersistedSession` rejects unknown fields, so an old Coordinator cannot decode a
row after a new writer stores interval history. Beta uses a maintenance cutover:
close Flow/public admission, drain and fully stop all old Coordinator writers,
and require every persisted accepted Event batch to carry its admission
coordinate before the new reader starts. There is no read-time coordinate
synthesis, pre-anchor fallback, or parallel compatibility projector. Deploy the
new revision only after that cutover condition is true, then reopen admission.

## Amendment (2026-09-01): Managed refresh uses one disposable prefix reducer

Every Managed refresh privately clones the paired last-valid result and source
checkpoint, applies the current `PersistedSession` plus one fenced committed
Thread/Run/lifecycle/Outcome prefix, validates the complete candidate, and then
swaps it into the process cache once. A failed or mixed-prefix fold stutters.
Cold rehydration starts the same reducer from an empty result/checkpoint; there
is no separate warm projector.

`SessionRecord` explicitly separates rendered result, source checkpoint, and
process-local transient `session.updated` overlay. Those are the only three
cache roles. Child lifecycle, usage, Event visibility, and message-consumption
coordinates are outputs of the same reducer, not independently synchronized
state machines. An accepted command is ordered by its immutable batch admission
coordinate even while its effect remains pending. The normal reducer can
therefore append already-closed Runtime facts without a root-wide barrier, while
a later effect observation updates only that command's progress. No event store,
child registry, usage authority, dispatch-derived lifecycle, fallback prefix, or
parallel compatibility projector is introduced.

Read-only Managed observations may serve a previously published complete
prefix when a new candidate races a committed append. Session and Thread Event
pagination add a minimum observation condition: that prefix must contain the
requested Event cursor. A cursor issued by another replica cannot be declared
invalid using an older cached prefix. The same refresh entrypoint therefore
uses its existing bounded strict refresh when the cursor is absent; convergence
permits ordinary pagination, persistent churn returns `503`, and only a complete
refreshed projection can reject a genuinely unknown cursor with `400`. Known
cursors and cursor-free reads retain the existing coherent-prefix availability.
No cursor registry, new store, second reducer, or write retry is introduced.
The actual two-replica HTTP decision table covers both Session and Thread routes;
abstract projection-publication proofs alone do not establish this API condition.

Event-batch cutover validation is a read-only projection of that same
supervisor, not a general Session-health claim or Cloud-side scan. After
Resource, continuation, realization, Event-batch, and Outcome repair, the
supervisor performs one final authoritative
`reconcilable_sessions()` scan. Each successful final scan followed by the
ADR-0074 canonical global Environment-phase count atomically publishes a
process-local generation plus only four aggregate counts: terminal Sessions
with incomplete Event batches, Event-batch failures observed by that recovery
cycle, quarantined rows returned by the same scan, and Restoring Environments.
A failed final scan or global count does not advance or replace the preceding
generation and remains a retryable recovery failure. The projection contains no
Session ids, quarantine reasons, clocks, credentials, or database details.

The existing Coordinator admin listener exposes this snapshot at
`GET /admin/session-event-batch-cutover-validation`; it returns `503` until the
first successful final scan. After every old writer is stopped, deployment automation
records each exact candidate Pod's baseline generation, then requires a strictly
newer generation with all four counts zero from every candidate before reopening
admission. A missing Pod, stale generation, failed scan, or nonzero count is
diagnostic no-proof and keeps the forward-only cutover closed. No readiness
probe, log timestamp, second scheduler, migration store, or Cloud database read
substitutes for this per-process proof.

`serde(default)` lets the new reader consume an old row; it does not make an
ordinary rolling deployment safe or reconstruct facts an old writer never
retained. A later two-stage reader-first rollout and an online legacy-history
migration are explicitly deferred.

## Amendment (2026-08-27): profiled creation is one complete insert

The immutable post-create policy and its mutation matrix are owned by
[ADR-0066](0066-session-service-binding-and-realization.md#2026-08-27-amendment-immutable-post-create-mutation-authority).
This amendment owns only how profiled product intent reaches that authority.

The private `ProfiledSessionCreate` requires `mode` with the closed wire values
`work_unit | interactive`, represented by
`ProfiledSessionMode::{WorkUnit, Interactive}`. WorkUnit lowers to
`SessionMutationPolicy::Frozen`; Interactive lowers to
`SessionMutationPolicy::FileResources`. The private wire deliberately exposes
no `Managed` mode, while ordinary `/v1/sessions` creation remains Managed and
keeps its existing Anthropic-compatible shape.

Direct `resource_inputs`, Repository inputs, published Agent defaults, MCP
candidates, Environment selection, tools, mounts, environment values, prompts,
and network policy all enter the sole `create_profiled_session` composer. It
resolves them before `SessionCreationIntent::finalize`, then persists the frozen
baseline, initial Resource/MCP truth, and repository-owned `IdempotencyRecord`
in the original revision-1 root insert. Its complete direct attachments are
already present in `resources.desired()`; active Resource truth is published
only after realization. The receipt, not mutable Session metadata, owns
profiled create replay.

```text
profiled product intent
  -> required WorkUnit/Interactive mode
  -> select the existing completion policy
     hosted HTTP edge ---------------------------------> AcceptDurableRoot
     historical in-process caller --------------------> AwaitRealization
  -> atomic repository receipt preflight
     exact live receipt -------------------------------> return durable aggregate
     exact ActivationFailed + AcceptDurableRoot ------> return failed aggregate
     exact ActivationFailed + AwaitRealization -------> Tombstoned
     absent receipt and identity ----------------------> continue
  -> one complete CreateProfiledSessionCommand
  -> resolve published defaults + direct inputs + Repository/MCP candidates
  -> SessionCreationIntent::finalize
  -> repository create(root revision 1 + idempotency receipt)
     Applied -> realization -> activation -> eligible WorkQueue projection
     Replayed -> apply the same completion policy to the durable aggregate;
                 perform none of those effects
```

`ManagedSessionRepository::create` owns the concurrent race and returns
`SessionCreateResult::{Applied(PersistedSession), Replayed(PersistedSession)}`.
Only `Applied` may continue into Runtime realization, activation CAS, cleanup,
lifecycle wake, or WorkQueue dispatch. `Replayed` discards the newly lowered
candidate and returns the repository's current durable aggregate without
repeating any effect. The atomic `replay_create` preflight uses the same owner,
receipt, identity, and tombstone classification; it is an optimization, not a
second replay authority.

The completion policy changes only the replay result, not durable ownership or
effects. Historical `AwaitRealization` callers reject an exact
`ActivationFailed` replay as `SessionCreationError::Tombstoned`. The hosted HTTP
edge uses `AcceptDurableRoot`, so the same replay returns the failed asynchronous
operation root with HTTP 202 and lets ordinary Session reads expose its durable
failure reason. Neither policy resurrects, replaces, realizes, activates, or
dispatches the replayed root. Conflict, tombstone, and payload mismatch retain
typed conflict outcomes; unavailable storage remains unavailable, while dangling
or otherwise corrupt durable identity becomes the typed internal error. No case
falls back to the locally compiled candidate.

The creation cause/effect table is:

| Rule | Atomic preflight | Repository create result | Durable state | Effect |
|---|---|---|---|---|
| C1 | receipt and identity absent | `Applied` | revision-1 complete desired truth | realize, activate, and project eligible Work exactly once |
| C2 | exact receipt | not called | live/current | return that durable aggregate; no lowering or external effect |
| C3 | absent before a concurrent winner | `Replayed` | live/current | return that durable aggregate; no realization, activation, cleanup, lifecycle wake, or Work dispatch |
| C4a | exact receipt or `Replayed` | any | `ActivationFailed` + `AwaitRealization` | `Tombstoned`; no external effect |
| C4b | exact receipt or `Replayed` | any | `ActivationFailed` + `AcceptDurableRoot` | return the failed durable aggregate / hosted HTTP 202; no external effect |
| C5 | occupied/tombstoned identity or mismatched receipt | conflict | any | typed HTTP 409; no insertion or external effect |
| C6 | dangling, ahead, or double identity | corrupt | any | typed internal failure; no insertion or external effect |

The hosted Flow projection uses that one command. Its former post-create
Resource resolve/fingerprint/whole-manifest authoring path is removed rather
than retained as a synchronized fallback. Later Interactive File changes use
the ordinary item-level Managed Resource verbs and ADR-0066's application gate;
they are not creation completion.

This new non-Managed baseline field and profiled receipt contract join the
already-required maintenance cutover above. Stop every old Coordinator writer
before writing a profiled row. The mutation-policy owner in
[ADR-0066](0066-session-service-binding-and-realization.md#compatibility-and-maintenance-cutover)
defines the forward-only rule: no old profiled metadata/default-receipt adoption
or backfill, and Flow uses a new post-cutover identity. Reader defaults preserve
historical Managed rows but do not make mixed writers safe or infer a stricter
policy for an old Session.

## Amendment (2026-08-28): terminal Repository publication stays on the unified Worker path

ADR-0063 owns the explicit terminal Repository publication decision. This
amendment fixes its Worker placement: publication is one phase of the existing
terminal Session realization and cleanup protocol, not a Run attempt and not a
new Work item.

Static ownership stays one-way. `SessionCleanupOperation` derives the immutable
`SessionRepositoryPublicationCommand`; `SessionRealizationControl` exposes that
command and records its exact receipt under the current
`SessionRealizationLease`; the authority-store-free Worker receives the command
separately from the frozen Session projection; and the existing
`RepositoryBindingVerifier` plus `RepositoryRealizer` validate and execute the
effect. The Worker never rereads a mutable Repository catalog, fabricates a
`RunClaim`, stores a publication queue, or gains Session authoring authority.

For an externally realized Session, the causal sequence is:

```text
terminal cleanup assignment under exact realization lease
  -> execute and durably acknowledge every child cleanup command
  -> poll the root-derived Repository publication command
  -> re-derive exact command before transport authorization
  -> verify frozen Workspace/Repository/config and live Worker/lease
  -> authorize direct or Gateway transport without persisting capability
  -> re-derive exact command after authorization
  -> publish through the one RepositoryRealizer
  -> record exact receipt through the Session root CAS
  -> poll and execute the ordinary root cleanup command
```

A stale lease, changed command, wrong binding, unavailable verifier, or
non-canonical receipt fails before durable acknowledgement and leaves the same
operation retryable. A legacy or new cleanup with no explicit publication
intent emits no publication command and retains the exact pre-amendment v1
cleanup path.

## Amendment (2026-08-29): profiled Run submission is a typed leaf over canonical admission

The private profiled Session edge needs to start an exact product-owned Run
without turning public Managed Event batches or the simplified background
operation into a second command transport. The existing
`SessionApplication::admit_session_run_for_owner` remains the sole owner of Run
reservation, Session activity admission, durable recovery, and retry identity;
`activate_admitted_session_run` remains the sole asynchronous activation seam.
Runtime Host and Run ingress retain the only dispatch, capability-placement,
claim, commit, and settlement authorities.

The private wire adds exactly one leaf operation:

```text
POST /v1/awaken/sessions/{session_id}/runs
  body: exact Agent, operation, Run, Messages, and execution requirements
  -> require authenticated Workspace and an existing non-Managed profiled Session
  -> verify the frozen Agent
  -> AdmitSessionRun { path Session, exact body, no subject/trace, PreservePrior }
  -> fingerprint the complete immutable command before Runtime projection
  -> SessionApplication::admit_session_run_for_owner
  -> verify admitted Session and Run identities
  -> SessionApplication::activate_admitted_session_run
  -> stable { session_id, run_id } receipt
```

The body cannot repeat `session_id`, select replacement behavior, inject trace
or data-subject identity, or carry unknown fields. A missing, foreign, or
ordinary Managed Session is hidden as `404` and is never created implicitly.
Invalid input and a changed command under the same Run identity are
`400`; unavailable reservation, projection, or activation is `503`; internal
or mismatched admission truth is `500`. Exact replay returns the same receipt.
The receipt is an acknowledgement after activation, not another Run status.

The static dependency direction remains one-way; process composition mounts the
Coordinator Ingress implementation while the shared Host consumes only its
contracts and the shared dispatch-execution owner:

```text
profiled wire DTO/router -> Managed wire lowering -> SessionApplication
                                                -> RunApplication port (Host implementation)
Runtime Host -> run-ingress contracts + shared dispatch-execution
Coordinator composition -> RunIngress implementation (durable queue/claimed commit)
Managed GET/SSE projection <------------- committed Session/Thread/Run truth
```

Public Managed Event mutation remains the compatibility surface for Managed
clients, and committed Managed Events remain a valid observation/resume/cancel
projection for product adapters. They are not a fallback for profiled Run
start. The simplified `SessionRunBackgroundApplication` also remains separate:
it may derive identities and default requirements for its own caller, whereas
this edge must preserve every supplied identity, Message, tool narrowing, and
required Worker capability. No Run table, queue, event store, mapping
repository, dual-write, or compatibility route is added.

The Session contract owns one versioned `SessionRunCommandFingerprint`. The
current compact form is `session-command-v1:sha256:<lowercase hex>` over the
Session, Agent, operation, Run, Messages, data-subject identity, tool narrowing,
required Worker capabilities, and replacement intent. Trace context is excluded
because it identifies a delivery attempt. Runtime Host computes this value from
the raw `AdmitSessionRun` before Skill expansion or current Agent, model,
Resource, Environment, placement, and snapshot projection. `RunDispatch`
persists it as one optional serde-default field; completion reuses the same
compact string. Existing JSON request and completion columns already own these
values, so this amendment adds no table or DDL.

New Session reservations require the exact current fingerprint and reject a
missing, malformed, or unknown incoming version before persistence. For replay,
only stored evidence selects compatibility: a stored current live row compares
the current fingerprint, a stored pre-field live row uses the former Session
subset equivalence, and a stored old `sha256:` completion uses its matching
legacy compact equivalence. Unknown stored versions fail closed. Thus a newer
Runtime projection can recognize the first command without overwriting its
dispatch, while a changed operation or required-capability set cannot be
mistaken for replay.

The cause/effect rules for this leaf are:

| Rule | Cause | Required effect |
|---|---|---|
| R1 | exact existing profiled Session and Primary command | reserve and activate the exact identities, Messages, Configured narrowing, and required capabilities; return the exact receipt |
| R2 | exact current command fingerprint retries before or after reservation, activation, recovery claim, completion, or response loss; trace/current Runtime projection alone drifts | return the same durable phase and receipt without another reservation, activity, activation, or dispatch overwrite |
| R3 | the same Run identity carries changed Agent, operation, Messages, data subject, tool narrowing, required capabilities, replacement, or Session | derive another command fingerprint and reject without overwriting or relinking durable truth |
| R4 | OutputRepair uses a distinct Run on the same Session | preserve prior Runs, deny all tools for the repair, and retain the frozen Agent |
| R5 | target is missing, foreign, or ordinary Managed | return `404` and create no Session |
| R6 | projection, reservation, or activation authority is unavailable | return `503`; unchanged retry remains safe |
| R7 | malformed input, deterministic rejection, or internal failure | preserve the `400` versus `500` classification |
| R8 | admission returns another Session or Run identity | fail before activation and expose no receipt |
| R9 | the exact command requires a Worker capability that is absent and later appears | retain both the requirement and command fingerprint on the first dispatch; an incapable Worker cannot claim and a capable Worker can converge, while changing the required set conflicts |
| R10 | Runtime commits Running, output, usage, or terminal truth | ordinary Managed GET/SSE reconstructs it from the same committed Session/Thread/Run authority |

Product adapters should build one protocol-neutral `AdmitSessionRun` value for
both embedded and hosted placement. Embedded composition passes it directly to
the application; hosted composition projects the same fields into this wire
and lets the leaf restore only the path-owned and fixed fields. That shared
projector, not an Event batch builder, owns stable product Message identities
and per-attempt tool/capability restrictions.

## Amendment (2026-09-10): exact product execution evidence is one read projection

A product that accepts a durable artifact needs to prove which committed Run
produced its source tool call and which immutable model/backend selection that
Session used. Replaying the product command, decoding a Managed Event id, or
copying model usage into a product table would create a second execution
authority. The existing authorities are sufficient and remain unchanged:

- `PersistedSession` owns the frozen Agent/model/backend baseline and retained
  Event-batch retry identity;
- the primary Thread recovery snapshot owns committed Runs, assistant tool-use
  blocks, and model-request completion audit records;
- Managed Events remains the only public usage projection.

`SessionApplication::session_execution_observation` joins those facts without
driving realization, recovery, or execution. The Awaken extension adapter adds
one GET-only leaf:

    GET /v1/awaken/sessions/{session_id}/execution-evidence
      -> enforce the authenticated Workspace against the Session root
      -> read the frozen Session baseline
      -> read one internally consistent primary-Thread recovery snapshot
      -> for each committed Run, project its retained idempotency key when known,
         canonical assistant tool-use ids, and completed model-request coordinates
      -> return the Session revision and execution state with those observations

This route is evidence, not another status model. It adds no table, queue,
receipt store, settlement rule, usage counter, or compatibility alias.
Create-time historical Runs can legitimately have no retained idempotency key.
Missing Runtime history returns zero Runs; malformed committed audit payloads
and ambiguous Run/tool coordinates fail closed. Token usage is deliberately
absent because Managed Events already owns that projection.

The static ownership is:

    Awaken extension DTO/router -> Managed HTTP adapter -> SessionApplication query
                                                       -> PersistedSession
                                                       -> SessionRuntime snapshot

The cause/effect rules are:

| Rule | Cause | Required effect |
|---|---|---|
| X1 | owned frozen Session and one committed Run/tool/model completion | return the exact Session -> Run -> tool/model coordinates without mutation |
| X2 | Run has no model completion or no source tool call | retain the Run with an empty observation; never promote it to executed |
| X3 | foreign Workspace or missing Session | hide it as `404` and perform no Runtime write |
| X4 | malformed model audit or ambiguous retained coordinate | fail closed; never fabricate successful execution evidence |
| X5 | GET is retried or the Session advances after the read | return a newly reconstructed projection carrying its observed Session revision; create no durable read state |

## Amendment (2026-08-31): registered Workers record the same publication outcome

ADR-0063 now distinguishes the sole Repository realizer's canonical receipt
from a typed permanent expected-prior CAS rejection. This does not add Worker
work or retry state. The existing terminal publication command remains the only
projection under `SessionRealizationLease`; `SessionRealizationControl` accepts
the corresponding receipt or rejection as mutually exclusive variants of that
one command outcome.

For either embedded or registered-Worker placement, the Session root CAS makes
the outcome durable before the existing ordinary root cleanup command becomes
visible. A lost HTTP response replays the same command-bound outcome. An
unavailable realizer or control call records nothing and retains the command;
an exact durable rejection suppresses further Git calls while still allowing
root cleanup to remove the fresh Session working tree. The Worker HTTP adapter
adds only the rejection variant of the existing command/receipt control wire;
it owns no queue, Session phase, Git transport, or sidecar.

## Amendment: final Event-batch non-admission for response-loss recovery

The objective is to settle an application reservation without guessing whether
a lost request executed. Ordinary input remains `sessions.events.send` on the
Managed SDK surface. The native-only
`POST /v1/awaken/sessions/{id}/event-admissions/close` receives the exact original
Managed body, Idempotency-Key, and optional data-subject header. It resolves
acceptance rather than retrying execution or rebuilding transcript projections.

| Scope | Existing authority | Decision |
| --- | --- | --- |
| Reused unchanged | Session repository root CAS, Workspace ownership, canonical request fingerprint and Managed input schema | Remain the sole identity, isolation and persistence boundaries. |
| Modified | SessionApplication Event-batch admission | Acceptance and final close share one CAS implementation and retained identity. |
| Added | Empty final-close variant of the existing root batch; native receipt endpoint | No executable Event, activity, new queue, receipt table or application-token authority is introduced. |

```text
original SDK send ----+--> Session root CAS --> accepted batch --> existing reconciler
native close --------+          |
                                +--> closed identity --> reject delayed SDK send
```

If acceptance won, close returns `accepted` and the original batch coordinate.
If close won, its empty, complete batch permanently fences that retry identity
while the root is retained. Repeated exact close is read-only; changed
fingerprints conflict. Neither timeout nor 404 establishes final non-admission.
An accepted receipt is not a Run terminal outcome or a published artifact.
The durable codec rejects closed receipts containing executable events,
activity or trace state; omitted flags on historical accepted batches mean
accepted, not closed.

Tool replies have an additional root-owned admission invariant: one pending
occurrence can retain only one result, even when two clients use different HTTP
keys. Managed still validates the public call family and resolves its exact
ResumeTicket; Runtime still owns result consumption. `SessionEventBatch` checks
reply occupancy against the retained root and earlier entries in the new batch.
`SessionApplication` invokes that check inside every root-CAS iteration, after
exact-key replay and before append. A CAS loser reloads and rejects the occupied
call instead of compiling another successful receipt. No table, local lock or
recovery registry is added.

The occurrence uses target, Run, correlation, Runtime call and the known pending
commit. A changed public spelling, payload, Thread version or processed marker
cannot free it. A legacy missing pending commit cannot prove a new occurrence
with otherwise identical neutral coordinates, so admission fails closed. Exact
key/body retry still returns its original receipt. This adds no serialized field
or migration and does not rewrite historical duplicate receipts. Bounded model
checks and barrier-controlled two-replica SQLite/PostgreSQL races cover admission before
delivery; they do not establish arbitrary SQL/async refinement. Their cause/effect
tables remain beside the domain, application and model-mutation tests.

Deletion extends the existing tombstone, not a second receipt registry. Both
stores derive the minimal key/fingerprint/batch/outcome inventory from the root
locked by the successful delete CAS. Message content, tool inputs, and trace
material are not retained. Lookup is scoped by the persisted Workspace owner.
An existing receipt survives deletion unchanged (`proof: event_batch`); an
absent key in a complete inventory returns Closed with `proof: session_deleted`
and the positive deletion revision, never an invented batch id. Late sends are
excluded by the same permanent Session tombstone. Nullable legacy inventories
remain unavailable/unknown; NULL must never be interpreted as an empty list.
Changing the original fingerprint remains a conflict even after deletion.

Native Event-batch receipts additionally expose ordered `event_ids`, projected
by the same `durable_inbound_event_id` function as standard SDK send receipts
and committed history. The existing root Event entries own the operation
coordinates; live and deletion evidence use `SessionEventBatch::operation_ids`
without a parallel event-id registry. Accepted input is not necessarily driven
yet, so a browser must observe these exact events rather than match text or
treat an older Idle snapshot as the new command's completion. These ids are
admission/observation coordinates, not a Run terminal or publication proof.

The existing tombstone JSON retains only the ordered canonical operation ids
alongside its prior key/fingerprint/batch/outcome. Store decoding rejects foreign
batch coordinates, gaps, duplicates, malformed ids and executable ids on Closed
receipts. Old tombstones without that optional field still prove their original
admission outcome; the observation identity stays unknown and is omitted from
the native reply. Closed receipts expose an empty list; a deletion-only proof
has no event list. No message text, tool arguments or trace payload is retained.
Old binaries reject the newly extended tombstone JSON because their decoder
denies unknown fields: upgrade all readers before enabling new deletion writers,
or drain and replace the single-version deployment. Do not erase retained fields
to make an old binary accept them or interpret decode failure as non-admission.

The HTTP decision-table test compares a two-input batch containing identical
text against SDK-issued event ids, then verifies unchanged replies after actual
deletion and cold reconstruction. The shared SQLite/PostgreSQL conformance
suite checks the same retained projection. This is protocol/storage evidence,
not a live browser/model execution-completion proof.

The shared current DDL tail appends a nullable column inside the selected
table-owning Session lineage without rewriting historical checksums. The old
convergence marker stays unchanged and owns no table DDL. Deploy the migration and deleting
writers as a coordinated cutover: an old writer can still create NULL evidence.
SQLite/PostgreSQL conformance, a cold HTTP adapter test with actual cleanup,
and retained-proof codec decision tables cover the implementation boundary.
Classroom's bounded SessionCommandConvergence model includes effective lookup
after deletion and rejects receipt-erasure and unknown-release mutations.
ToolResultProtocol still checks root-CAS alternatives, not cross-product quota
liveness or universal Rust/SQL refinement. Historical missing evidence cannot
be reconstructed by these fixes and requires an explicit reconciliation decision.


### Logical step identity across client-tool resume

A real Native coordinator Session exposed different coordinates after a client
confirmation: the assistant Message advanced to step 1 while the new ToolBatch
restarted at physical-drive step 0. `send_message` admitted the child under the
batch's operation, then the committed-link reader derived another Thread from
the Message coordinate and refused the valid receipt. This is a Runtime
identity defect, independent of report wording or the application's business
archive.

The Runtime now derives one logical source step for inference evidence,
assistant Messages, tool operations and phase hooks. The finite step ceiling
also belongs to the same Run across resume; a new physical drive does not reset
it. Local drive position remains only for selecting its first resumed stream
checkpoint.

Committed operation reads reuse `ActiveToolBatch::operation_for_message` in
both Host coordination and Session ordering. When a batch was retained at the
assistant's commit, its recorded operation owns the accepted effect, including
historical executions with the mismatch above. The existing transcript
coordinate contract remains available to snapshots without retained batch
evidence. Conflicting arguments, ambiguous batch identities and foreign child
receipts remain errors; no Message, receipt, Thread, or durable store is
rewritten. Batch decoding is shared with recorded execution-domain recovery.

Cause/effect rules remain attached to the tests: Runtime confirmation→resume→
physical-tool identity and finite Run ceiling; recorded source warm/cold and
conflict cases; Host published-result/publication-gap recovery of the original
child. No protocol route or second execution/relationship registry is added.

## Amendment (2026-10-01): initial preparation without a model Run

An externally placed, complete `Preparing` Session can have MCP and Environment
effects that must finish before a product can validate its published Agent. The
registered Worker previously entered the realization driver only after a Run
claim. Starting a synthetic model Run to prepare the Session would duplicate
execution truth, while letting the Coordinator Host realize it would install a
second physical owner. The durable Session root already indexes the initial
realization obligation; a separate job table or Flow-specific status is not
needed.

An explicit, idempotent Session-root command retains the typed
`run_free_initial_preparation` request inside existing realization progress.
Historical and ordinary Run-driven Sessions default to false; a caller may
request preparation of an exact pre-upgrade `Preparing` Session without
recreating it. The registered Worker may claim an
initial-preparation directive only for such a root-owned obligation, under its
authenticated Ready incarnation and an exact
Session realization lease. The Coordinator checks the frozen Environment,
Resources and required Sandbox capabilities through the same physical
placement compiler used for Runs. The Worker drives the existing projection
synchronizer and realization phase protocol without a Run claim or inference
call. An already-live foreign lease is never stolen. Self-hosted Environment
placement continues to require the existing WorkQueue lease and is excluded
from this no-Run claim; its ownership protocol is not bypassed. The result is
observed from the ordinary Session root. An unavailable Worker leaves
`Preparing` retryable, while a durable failed phase remains a failure rather
than a fabricated ready result.
