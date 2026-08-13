# ADR-0075: Unified Managed Session Worker Execution

- Status: Accepted
- Date: 2026-08-12
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

### D3: a Run claim is an attempt fence, not another placement decision

When Awaken's registered Worker transport executes a Run for that Session, the
Run claim scopes one attempt under the already-selected Session Work. It proves
the exact Worker incarnation and epoch allowed to mutate physical realization.
It does not create Session desired state and does not compete with WorkQueue
placement. Registered dispatch atomically acquires that exact Session item from
the same `WorkQueue`; an official/custom Worker holding it wins, and the
registered attempt is rejected. Run claim checks and realization renewal renew
the same Work lease, while every realization phase verifies the exact owner.
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
  -> registered adapter only: atomically acquire the exact same Work item
  -> optionally claim a subordinate Run attempt
  -> authenticate Worker incarnation and verify live Work + Run epochs
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
