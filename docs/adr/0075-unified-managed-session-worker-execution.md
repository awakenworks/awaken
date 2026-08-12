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

`Preparing` remains a short-lived durable recovery state inside the one create
transaction sequence. It is not an invitation for another actor to author
desired state. A Worker cannot add or replace Session inputs.

### D2: Environment WorkQueue is the sole Session Worker placement path

A frozen, nonterminal Session whose Environment is self-hosted always projects
to its Environment WorkQueue. Placement does not depend on caller identity,
application registration, or the presence of a local Worker decorator.

The stable Work id is the long-lived Session execution ownership coordinate.
Enqueue replay is idempotent and reconciliation reconstructs a missing
projection from the durable Session.

### D3: a Run claim is an attempt fence, not another placement decision

When Awaken's registered Worker transport executes a Run for that Session, the
Run claim scopes one attempt under the already-selected Session Work. It proves
the exact Worker incarnation and epoch allowed to mutate physical realization.
It does not create Session desired state and does not compete with WorkQueue
placement.

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
        | finalize + root CAS                  one Session truth
        |
        +-- local Environment ----------> local realization
        |
        `-- self-hosted Environment ----> Environment WorkQueue
                                              |
                                              | Work lease
                                              v
                                      custom / local Worker
                                              |
                                              | optional Run claim
                                              v
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
  -> insert Session root in Preparing
  -> finalize Resources/MCP and freeze root by CAS
  -> local: realize and acknowledge -> Idle
     self-hosted: enqueue stable Work item -> Preparing until Worker realization

Worker trigger
  -> claim Work
  -> optionally claim a Run attempt
  -> authenticate Worker incarnation and verify live epoch
  -> resume frozen Session projection
  -> acquire per-Session realization admission
  -> stage physical effects
  -> activate/publish
  -> acknowledge exact receipts -> Idle/Running
```

Failure and retry rules:

- create-time normalization conflicts fail before any Session row is inserted;
- a WorkQueue write failure leaves durable Session truth reconcilable and does
  not manufacture readiness;
- stale Work/Run ownership fails before Worker effects;
- response loss replays the same Work id, root revision, realization lease, and
  generation receipts;
- a physical effect failure records the existing realization failure state and
  is retried only through the same phase driver;
- archive/delete/termination remains the sole terminal Session path and stops
  further Work projection.

## Implementation classification

### Reused unchanged

- `ManagedSessionRepository`, root revision CAS, lifecycle outbox, Resource
  state, MCP generation state, and `SessionRealizationLease` remain the durable
  authorities.
- Environment `WorkQueue`, its HTTP API, stores, lease rules, and reconciliation
  remain the placement mechanism.
- `DispatchQueue`, Worker authentication, recovery, and the backend executor
  registry remain the attempt mechanisms.
- `drive_session_realization` remains the sole physical phase driver.

### Modified

- `SessionCreationIntent` contains only complete Control inputs and finalizes in
  one step.
- profiled/Dream creation supplies local inputs up front.
- every eligible self-hosted Session now uses `needs_work_dispatch` without a
  caller-specific exclusion.
- Worker Control naming and transport express frozen Session realization.

### Added

No new runtime mechanism was added. Only decision-table coverage and this
canonical decision record are new.

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

## Consequences

There is one source of Session desired state and one Session Worker placement
path. Local and custom Workers differ only in adapters and physical effects.
Applications remain free to build complete Session commands and to decorate the
neutral attempt executor, but they cannot mutate Session truth from a Worker.
