# ADR-0075: Unified Managed Session Worker Execution

- Status: Accepted
- Date: 2026-08-12
- Amended: 2026-08-28 — terminal Repository publication uses the same Worker realization owner
- Supersedes: the late Worker-authored Session-input path in ADR-0063,
  ADR-0065, and ADR-0066
- Preserves: ADR-0065 claim recovery and attempt execution; ADR-0066 Session
  root CAS, MCP generations, and realization leases

## Context

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
restriction, Resources, and initial MCP candidates before invoking
`SessionApplication::create_session`. `SessionCreationIntent::finalize`
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
`sessions_token`. The WorkQueue stores only a domain-separated SHA-256 digest
on the same Work row and binds it to the current owner, lease epoch, and expiry.
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
After claim-fenced effects commit, private registered dispatch releases that
Session Work immediately before settling the subordinate Run. Public custom
Workers retain the official explicit `stop` operation. Graceful registered
Worker deregistration releases any residual active Session Work for that exact
incarnation; crash recovery remains lease-expiry based. A later admitted event
wakes the same stable Work item.

### D4: Worker Control realizes only committed truth

The Worker resumes an already-frozen projection through the claim-fenced
Session Control endpoint. It may stage, activate, publish, acknowledge, renew,
or fail physical effects under `SessionRealizationLease`. It must fail closed if
Control has no frozen Session or if the claimed Resource projection differs.

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
- a physical effect failure records the existing realization failure state and
  is retried only through the same phase driver;
- archive/delete/termination remains the sole terminal Session path, retires
  Work, and is retried by the same reconciliation scan after response loss.

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
the delivered snapshot for lease renewal, and an overlapping claimed Run must
carry the same snapshot or fail closed before any effect. Historical baselines
without an exact revision retain their existing compatibility behavior.

## Amendment (2026-08-24): Session root retains canonical runtime intervals

Managed event history is a projection of the existing Session, Thread, Run,
message, and audit facts. The Managed protocol cache and lifecycle outbox are
not event stores: the outbox remains delete-after-delivery, while the Session
root CAS retains every closed customer-visible Running interval for the Session
lifetime.

Each interval freezes its opening/closing root revisions, exact Runtime
lifecycle observations, cumulative neutral usage, and historical budget limit.
The existing Event-entry processed CAS freezes the exact Runtime commit anchor
that made that command's effect listable; `state_command.commit_sequence` does
the same for compaction, Outcome, and disposition facts. Admission revisions
remain root ordering provenance but are never compared with Runtime commit
coordinates. One interval therefore projects exactly one aggregate
Running/Usage/Idle bracket, including child-only continuations and multiple
resumes of the same Run.

The projector consumes only recovery snapshots fenced at those committed
boundaries. New entries without an immutable anchor are withheld, and a final
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
then let the existing Open lifecycle supervisor reconcile retained rows. A
legacy terminal aggregate with an incomplete Event batch executes no new
Runtime work: the same root-CAS batch owner resolves it at the terminal cleanup
cursor, or at the explicitly isolated pre-anchor legacy prefix when that old
row has no cursor. This is a narrow, non-destructive compatibility repair, not
a second scheduler or event store. Deploy the new revision only after that
reconciliation is clean, create new Sessions for the complete-history guarantee,
then reopen admission.

Event-batch cutover validation is a read-only projection of that same
supervisor, not a general Session-health claim or Cloud-side scan. After
Resource, continuation, realization, Event-batch, and Outcome repair, the
supervisor performs one final authoritative
`reconcilable_sessions()` scan. Each successful final scan atomically publishes
a process-local generation plus only three aggregate counts: terminal Sessions
with incomplete Event batches, Event-batch failures observed by that recovery
cycle, and quarantined rows returned by the same scan. A failed final scan does
not advance or replace the preceding generation and remains a retryable recovery
failure. The projection contains no Session ids, quarantine reasons, clocks,
credentials, or database details.

The existing Coordinator admin listener exposes this snapshot at
`GET /admin/session-event-batch-cutover-validation`; it returns `503` until the
first successful final scan. After every old writer is stopped, deployment automation
records each exact candidate Pod's baseline generation, then requires a strictly
newer generation with all three counts zero from every candidate before reopening
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
hosted product profile
  -> required WorkUnit/Interactive mode
  -> atomic repository receipt preflight
     exact live receipt -------------------------------> return durable aggregate
     exact ActivationFailed/tombstone -----------------> typed HTTP 409
     absent receipt and identity ----------------------> continue
  -> one complete CreateProfiledSessionCommand
  -> resolve published defaults + direct inputs + Repository/MCP candidates
  -> SessionCreationIntent::finalize
  -> repository create(root revision 1 + idempotency receipt)
     Applied -> realization -> activation -> eligible WorkQueue projection
     Replayed -> return repository durable aggregate; perform none of those effects
```

`ManagedSessionRepository::create` owns the concurrent race and returns
`SessionCreateResult::{Applied(PersistedSession), Replayed(PersistedSession)}`.
Only `Applied` may continue into Runtime realization, activation CAS, cleanup,
lifecycle wake, or WorkQueue dispatch. `Replayed` discards the newly lowered
candidate and returns the repository's current durable aggregate without
repeating any effect. The atomic `replay_create` preflight uses the same owner,
receipt, identity, and tombstone classification; it is an optimization, not a
second replay authority.

An exact replay whose durable aggregate is `ActivationFailed` returns
`SessionCreationError::Tombstoned`, projected as HTTP 409, and never attempts to
resurrect, replace, realize, activate, or dispatch it. Conflict, tombstone, and
payload mismatch retain typed conflict outcomes; unavailable storage remains
unavailable, while dangling or otherwise corrupt durable identity becomes the
typed internal error. No case falls back to the locally compiled candidate.

The creation cause/effect table is:

| Rule | Atomic preflight | Repository create result | Durable state | Effect |
|---|---|---|---|---|
| C1 | receipt and identity absent | `Applied` | revision-1 complete desired truth | realize, activate, and project eligible Work exactly once |
| C2 | exact receipt | not called | live/current | return that durable aggregate; no lowering or external effect |
| C3 | absent before a concurrent winner | `Replayed` | live/current | return that durable aggregate; no realization, activation, cleanup, lifecycle wake, or Work dispatch |
| C4 | exact receipt or `Replayed` | any | `ActivationFailed` | `Tombstoned` / HTTP 409; no external effect |
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
