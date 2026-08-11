# ADR-0074: Session Environment Suspension and Checkpointed Continuation

- Status: Accepted
- Date: 2026-08-11
- Amends: ADR-0056. Clarifies ADR-0073: `SessionEnvironment` remains the sole
  live environment owner; Hand process hibernation remains an orthogonal local
  optimization, and its stop primitive is reused during Environment quiescence.
- Depends on: ADR-0065, ADR-0066, ADR-0071, ADR-0072, ADR-0073

## Context

A Session can currently defer its first environment until execution needs one,
persist the exact live binding before use, adopt that binding after process
loss, and dispose it at the terminal boundary. A container-backed environment
may also stop and lazily reacquire an inactive Hand process. That local
optimization deliberately retains the Sandbox and workspace because no durable
continuation exists for their mutable filesystem.

This leaves a lifecycle gap. A logically idle Session can retain a complete
Sandbox indefinitely, while destroying it would lose installed packages,
workspace edits, repositories, outputs, and other mutable filesystem state.
Terminal cleanup cannot fill the gap: it publishes and retires Session-owned
state, permanently fences execution, and must never support a later resume.

The gap affects the whole Session Environment rather than only the primary
Run. Native delegated Runs and shared-context Skill forks use the same
Sandbox. Session-local MCP processes, in-flight tool calls, and any auxiliary
work that can touch the shared filesystem also participate in its safety. A
primary Run reaching a boundary is therefore necessary but not sufficient
evidence that the Environment can be suspended.

Adding a second idle registry, checkpoint manager, or environment metadata
store would duplicate the existing Session aggregate and
`SessionEnvironmentProvider`. The solution must extend those owners, reuse the
existing root CAS, durable dispatch, realization leases, process supervision,
and recovery loops, and preserve one source of truth.

## Verified current implementation gaps and consolidation

The following gaps were verified against the current code. They are not
reasons to introduce parallel mechanisms; each is resolved by extending or
retiring behavior at its existing authoritative owner.

| Current authoritative path | Conflict or inconsistency | Consolidation decision |
|---|---|---|
| `awaken-session-contract::SessionEnvironmentState` | It stores only `Unmaterialized` or `Resident`, and `SessionEnvironmentReceipt` proves only create/adopt. It cannot represent a recoverable release or distinguish an upload from a completed suspension. | Extend this enum, receipt vocabulary, row codec, and root-CAS transitions in place. Migrate old rows to the equivalent two existing states. Do not add a checkpoint-state table or Session projection. |
| `awaken-provisioning-contract::SandboxProvider` and `Sandbox` | The canonical port exposes create/adopt and live dispose, but no checkpoint/restore capability or receipt. A provider-specific side channel would bypass capability admission and duplicate lifecycle effects. | Add checkpoint/restore capability, opaque references, and idempotent effects to the canonical port and its one conformance suite. Keep disposal on the same Sandbox owner. |
| `SessionApplication::begin_activity` / `settle_activity` | One driving-event epoch currently returns the Session to `Idle` when that event settles. It does not prove that delegated Runs, shared-context forks, auxiliary work, MCP calls, or child processes have reached a safe boundary. | Retain `activity_epoch` as the stale-completion fence, but make full-environment reclaim require the aggregate idle edge plus Runtime quiescence evidence. No persisted counter or second idle status is added. |
| `awaken-runtime-host::BackgroundRuns` | It is one process-wide best-effort `JoinSet`, has no Session/generation identity or Environment relationship, swallows task failure, and drains only for process shutdown. It cannot be used as reclaim evidence. | Evolve this registry into the sole typed structured-background admission path, migrate every spawn caller, and require `SharedEnvironment`, `ExternalDurable`, or `EphemeralCache`. Remove unclassified detached spawns; do not add another background tracker. |
| Session-owned Hand inactivity in `session_environment/container_skills.rs` | Its timer stops only a rebuildable Hand process and deliberately retains the Sandbox. Treating it as Session hibernation would create two timers and still lose mutable filesystem state on Sandbox disposal. | Keep the timer as a local optimization and reuse its stop/reap primitive during Environment quiescence. It never writes Session environment state and never authorizes disposal. |
| Sandbox lease expiry and the former container age reaper | The dead-man contract previously returned a reap cause, while a separate process-local reaper deleted prior-owner containers by exit/age without consulting durable Session references. Either path could destroy the only mutable copy. | Lease decisions now return only `Keep/Fence`; the age-based reaper, its settings, discovery scan, and parallel tests are removed. `reconcile_adoption(live, referenced)` is the sole cross-restart disposal decision, and terminal/checkpoint transitions remove the live reference before that decision. |
| Existing terminal cleanup intent/receipt | It already fences admission, quiesces delegated work, and replays cleanup. A checkpoint-specific deletion saga would compete with that authority. | Add live-Sandbox and checkpoint deletion effects to the existing terminal intent/receipt set. Terminal intent rejects stale suspend/restore receipts and remains the only terminal owner. |

The migration order is contract-first: extend the aggregate and provider
capability; make old rows decode unchanged; migrate all activity/background and
provider callers; enable suspend policy only after every selected provider and
Worker advertises conformance; then remove any temporary compatibility branch.
There is never a period in which two stores, timers, or receipt families can
independently decide Environment lifecycle.

## Decision

### D1: the Session aggregate owns one environment-continuation lifecycle

`PersistedSession.environment` remains the only durable lifecycle authority.
It expands from the current `Unmaterialized | Resident` model to five domain
states:

```text
Unmaterialized
Resident { binding, generation }
Suspending { operation, source_binding, generation, phase }
Hibernated { checkpoint, generation }
Restoring { operation, checkpoint, generation }
```

The top-level vocabulary describes business-significant availability. Crash
recovery progress stays inside the operation:

```text
SuspendPhase = Quiescing | Uploading | ReadyToDispose
```

`SandboxGeneration` carries a stable generation id, creation time, immutable
Environment fingerprint, base-image identity, and fixed expiry. Activity and
restore do not move that expiry. Creating a fresh Sandbox after expiry creates
a new generation and a new fixed window.

`SandboxCheckpointRef` is opaque, secret-free evidence. It identifies the
bytes and records format, digest, size, creation/expiry time, Environment/base
image fingerprints, excluded mount identities, and the exact suspend effect.
It contains no provider URL, host path, credential, or encryption material.

Every transition uses the existing Session root revision. Every external
receipt is bound to `session_id`, environment generation, operation id,
activity epoch, and exact realization lease. A stale Worker, owner incarnation,
generation, or activity completion cannot advance the aggregate.

### D2: one frozen policy owns full-environment idle release

The exact Environment-bound execution policy adds one neutral idle-retention
policy:

```text
EnvironmentIdleRetentionPolicy {
  mode: Resident | CheckpointAndRelease,
  checkpoint_after_secs,
  retention_secs,
  expiry_behavior: FreshFromFrozenEnvironment,
  max_checkpoint_bytes,
  max_checkpoint_duration_secs,
  checkpoint_format,
}
```

The policy is frozen into `EnvironmentSnapshot`; an existing Session never
reopens the current policy version. `Resident` retains the Environment across
idle Session periods. `CheckpointAndRelease` drives the complete suspend saga.
There is no second timer, registry, or state machine that independently decides
full-Environment suspension.

ADR-0073's Hand inactivity policy remains Worker-local and based on actual Hand
use rather than Session status. It can stop a rebuildable process during a long
model turn while the Environment remains resident, so it is not an alternative
Environment lifecycle. Full suspension invokes the same Hand stop/reap
primitive during quiescence and still owns the only durable Environment state.

Warm capacity remains orthogonal. A warm pool owns only never-used, mount-less,
exact-shape capacity. A Session-bound Sandbox never returns to it after
suspension or terminal release.

### D3: static ownership remains within existing bounded contexts

| Owner | Responsibility | Does not own |
|---|---|---|
| Session aggregate/application | desired environment state, generation, operation intent, checkpoint reference, Running interval, root CAS and recovery | provider bytes, host paths, live processes, pricing |
| WorkQueue and Worker claim | durable effect delivery, retries, lease/epoch fencing | Session desired state |
| Runtime Host | shared-environment admission gate, quiescence proof, projection teardown/rebuild | durable lifecycle decisions |
| `SessionEnvironmentProvider` | the one live Sandbox instance and provider selection | a second Session registry |
| `SandboxProvider` | create, adopt, checkpoint, restore and dispose effects | Session status and retention policy |
| Resource and MCP owners | exact pins, write-back/flush, generation staging and publication | filesystem checkpoint lifecycle |
| Lifecycle delivery composition | ordered delivery to every configured consumer through the one transactional outbox fact id | Session state, pricing, or a second outbox |

The provisioning contract extends the canonical `SandboxProvider` port with
`checkpoint_formats`, while the existing `WorkerManifest.checkpoint_formats`
and `PlacementRequirements.checkpoint_format` remain the scheduling source of
truth. No second format field is added to `SandboxCapabilities`, and no
checkpoint-provider registry is introduced. Admission rejects a provider or
Worker that cannot honor the exact frozen format.

The checkpoint byte request carries the existing Workspace owner scope plus
the immutable generation creation and expiry timestamps. Deployment adapters
use that scope only to resolve an existing placement and key-custody authority.
No tenant registry, storage URL,
credential, or pricing data enters the Session aggregate or provider contract.

Customer-visible execution time is projected without adding another Session
state machine. The aggregate retains one typed open Running interval across
overlapping driving events. The transition to idle or terminal closes it and
commits `session.runtime_interval_closed` through the existing
`ManagedLifecycleFact` transactional outbox in the same root CAS. The fact is
secret-free and pricing-neutral; downstream consumers may interpret it, but
cannot author or repair Session execution state. Restore, checkpoint, queue,
drain, and retention never open this interval.

When a deployment has more than one lifecycle consumer, the Control composition
uses the contract's one ordered `CompositeLifecycleFactDelivery`. Every
consumer is attempted; any failure leaves the stable fact pending for replay.
Each consumer must therefore be idempotent by fact id. This extends the one
outbox delivery port and does not add a Billing poller or delivery ledger.

The canonical deployment builder returns the provider together with its
never-used capacity, creation mounts, and CacheVolume initializer. A
composition that adds a checkpoint implementation decorates only that provider
and returns the complete component set to the same Worker builder. It must not
reconstruct the Kubernetes/container provider or discard the paired capacity
owners merely to add continuation.

### D4: environment idleness is a whole-owner quiescence condition

The public Session becomes idle only after every activity that uses the shared
Environment reaches a durable boundary. Runtime work is classified by its
relationship to that Environment:

```text
BackgroundWorkClass =
    SharedEnvironment { session_id, generation_id }
  | ExternalDurable { durable_intent_id }
  | EphemeralCache
```

- The primary Agent, delegated Agents, and shared-context Skill forks block
  idle while actively executing against the Environment.
- A delegated Run awaiting external input does not block suspension after its
  Run state, resume ticket, delegation relationship, and committed-state
  watermark are durable and it owns no live process or filesystem mutation.
- `SharedEnvironment` work is structured, holds the same environment activity
  guard, and keeps the Session running until it ends or reaches a durable
  boundary.
- `ExternalDurable` work continues under its own durable intent and does not
  retain the Environment.
- `EphemeralCache` work may be cancelled without changing durable behavior.
- Session-local MCP/tool effects and supervised processes must finish or be
  stopped before the quiescence receipt is valid.
- Unclassified detached work that can touch the Environment is forbidden.

No persisted activity counter is added. The existing Run/dispatch and
delegation authorities prove durable boundaries; the Runtime admission gate,
activity guards, process supervisor, and MCP generation state prove absence of
live effects. `QuiescenceReceipt` is evidence for one suspend intent, not a
second activity authority.

### D5: suspend persists recoverable evidence before releasing compute

The dynamic suspend sequence is:

```text
last shared-environment activity reaches a durable boundary
  -> root CAS: execution=Idle, environment=Suspending(Quiescing), outbox intent
  -> claim-fenced Worker closes the Environment admission gate
  -> wait for primary/delegated/shared-background activity
  -> finish or stop MCP, Hand, tool and child processes
  -> flush Resource/Memory consistency boundaries and filesystem writes
  -> return exact QuiescenceReceipt
  -> root CAS: phase=Uploading
  -> provider checkpoints the complete mutable filesystem
  -> verify manifest, digest and size
  -> root CAS: phase=ReadyToDispose with durable checkpoint reference
  -> provider disposes the source Sandbox and proves it terminated
  -> root CAS: environment=Hibernated
```

`Running -> Idle` closes Agent execution before infrastructure housekeeping.
Checkpoint/upload/dispose are environment maintenance and cannot admit Agent
work. A failed quiescence, unavailable checkpoint target, quota failure, or
checkpoint timeout retains the source Sandbox and retries; it never disposes
without a durable, verified checkpoint reference.

The ordering closes every crash window:

- upload success before `ReadyToDispose` may leave an orphan object, but the
  source remains available and reconciliation can retry or collect the orphan;
- `ReadyToDispose` before dispose replays idempotent disposal;
- dispose success before `Hibernated` observes terminated source status and
  completes the CAS;
- duplicate commands use the same operation id and cannot create a second
  authoritative checkpoint.

### D6: driving ingress waits for one canonical restore path

A user message, tool result, confirmation, or other driving event is durably
admitted through the existing Session/Run admission and WorkQueue path before
Runtime I/O. No resume queue is added.

```text
durable driving event
  -> ensure_environment_resident
     Unmaterialized -> create from frozen Environment
     Resident -> no environment effect
     Suspending -> wait for the committed suspend operation
     Hibernated and live checkpoint -> Restoring
     Hibernated and expired checkpoint -> fresh generation
     Restoring -> join the existing operation
  -> verify restore receipt and root-CAS Resident
  -> begin_activity and transition Idle -> Running
  -> dispatch exactly one Run
```

An event arriving during suspension remains durable and waits. The first
implementation always completes suspension and then restores; it does not add
a cancellation branch with ambiguous partial uploads. Reads, event streaming,
non-driving metadata updates, and terminal commands do not restore an
Environment merely to inspect or retire the Session.

Restore recreates the exact mutable filesystem against the recorded immutable
base, then reattaches exact Resource pins, realizes fresh credentials, and
stages/publishes the required MCP generations. Expired state creates a fresh
Sandbox from the Session's frozen Environment without deleting conversation or
committed Run history. A missing or corrupt checkpoint before expiry fails
closed; it cannot silently substitute a fresh filesystem.

### D7: checkpoint scope is filesystem continuation, not process continuation

The provider checkpoint must preserve the writable root and workspace,
including runtime-installed packages, created files, repository edits, outputs,
permissions, ownership, links, xattrs, sparse-file shape, and container-layer
whiteouts supported by the selected format.

It excludes pseudo-filesystems, live sockets, PTYs, process memory,
Session-local MCP/Hand processes, and credential mounts. Independently governed
Resource/Memory mounts are flushed and rematerialized from their exact durable
pins instead of copied into a second authority. Short-lived credentials are
revoked or allowed to expire and are freshly materialized on restore.

Workdir, Namespace, Docker, Podman, and Kubernetes providers implement the
same conformance contract. A Kubernetes provider qualifies only when the
complete mutable filesystem is exportable/importable or backed by a
snapshot-capable continuation volume. An ordinary ephemeral writable layer or
`emptyDir` cannot advertise the capability.

### D8: terminal intent always wins and never restores merely to clean up

Archive/delete/terminal failure commits the existing terminal admission fence.
That fence rejects new driving events and stale suspend/restore receipts.
Cleanup disposes any live source or restored orphan, deletes any checkpoint
reference through an idempotent effect, and completes the existing terminal
receipt protocol. It never restores a hibernated Environment to publish or
delete it; outputs that must outlive a Session use their existing durable
publication paths before the Session becomes terminal.

### D9: FMECA drives fail-closed controls

Scores use 1 (low) through 5 (high); detection is 5 when the fault is hardest
to detect before impact. RPN is severity × occurrence × detection and is a
prioritization aid, not an authorization to accept data loss.

| Failure mode | Local effect / end effect | S/O/D | RPN | Required prevention, detection, and recovery |
|---|---|---:|---:|---|
| Source disposed before checkpoint reference commits | Mutable filesystem is permanently lost | 5/2/4 | 40 | Type and transition invariants forbid dispose before `ReadyToDispose`; provider contract tests inject every crash boundary; retain source and alert on checkpoint failure. |
| Lease loss is mistaken for disposal authority | Worker loss becomes Session data loss | 5/3/4 | 60 | The lease action type has no dispose variant; route fenced environments to referenced-set reconciliation and permit disposal only after the durable live reference is absent. |
| Stale Worker, epoch, generation, or realization lease applies a receipt | New work is overwritten or duplicate Environments become authoritative | 5/3/3 | 45 | Bind and verify all receipt identities under root CAS; reject stale evidence; property-test concurrent interleavings. |
| Shared child/background work is omitted or misclassified | Snapshot is inconsistent while a writer remains active | 5/3/4 | 60 | One typed background admission API, generation-scoped activity guards, deny unclassified detached work, and runtime leak assertions. |
| Hand/MCP/tool process survives quiescence | Post-checkpoint mutation or duplicate side effect | 4/2/3 | 24 | Close admission first, supervise bounded stop/reap, require exact quiescence receipt, and retain source on ambiguous termination. |
| Worker crashes during upload or disposal | Orphan bytes, resident leak, or stuck phase | 3/4/2 | 24 | Durable phase before effect, stable operation id, idempotent provider calls, and bounded reconciliation from the committed phase. |
| Checkpoint is corrupt, missing, expired, or format-incompatible | Restore is wrong or unavailable | 5/2/2 | 20 | Digest/manifest/capability verification, fail closed before expiry, fresh generation only at expiry, quarantine and observable reason codes. |
| Terminal intent races suspend/restore or driving ingress | Deleted Session is resurrected or cleanup leaks | 5/3/2 | 30 | Terminal admission fence wins in root CAS, receipts are rejected afterward, cleanup never restores, and race tests cover every phase. |

All severity-5 modes have architectural prevention plus executable detection;
no operational retry is treated as the sole control for irreversible loss.

### D10: test design is cause/effect driven and lives beside the tests

The core causes are:

- C1: environment state and suspend phase;
- C2: primary/delegated/shared-background activity at active or durable
  boundary;
- C3: process/MCP effects active or quiescent;
- C4: activity epoch and realization lease current or stale;
- C5: checkpoint effect success, failure, or ambiguous completion;
- C6: dispose effect success, failure, or ambiguous completion;
- C7: checkpoint valid, expired, missing, or corrupt;
- C8: concurrent driving or terminal command.

The effects are:

- E1: retain Resident and emit no checkpoint effect;
- E2: advance one suspend phase;
- E3: retain source and retry;
- E4: enter Hibernated only after verified checkpoint plus terminated source;
- E5: restore once and admit the driving event once;
- E6: reject stale evidence;
- E7: create a fresh generation only after expiry;
- E8: terminal cleanup wins and no restore occurs.

Minimum reclaim decision table:

| Rule | Primary | Shared child | Shared background | MCP/process | Terminal | Effect |
|---|---|---|---|---|---|---|
| R1 | active | any | any | any | no | E1 |
| R2 | boundary | active | any | any | no | E1 |
| R3 | boundary | awaiting/durable | none | quiescent | no | E2 |
| R4 | boundary | none | active | any | no | E1 |
| R5 | boundary | none | external durable only | quiescent | no | E2 |
| R6 | boundary | none | none | cannot quiesce | no | E3 |
| R7 | any | any | any | any | yes | E8 |

Tests attach the relevant causes, effects, constraints, and rule ids in comments
beside each case. Required layers are:

1. pure transition and property tests for every legal and illegal state edge;
2. SQLite/PostgreSQL repository conformance for CAS, outbox, migration and
   duplicate receipt behavior;
3. application saga tests for crashes before and after every CAS/effect boundary;
4. Runtime tests for primary, delegated, Skill-fork, Background, MCP and process
   combinations;
5. one provider contract suite covering metadata fidelity, excluded state,
   corruption, expiry and idempotency;
6. real Docker/Podman and Kubernetes tests proving source termination and
   restore into a distinct environment;
7. active-active Coordinator, stale Worker lease, Worker crash, terminal race,
   and duplicate driving-event tests.
8. lifecycle fan-out tests proving every consumer is attempted and any failure
   keeps the single outbox fact retryable.

Completion requires every decision-table rule, every state transition, and the
before/during/after crash window of every external effect. Mock-only evidence is
insufficient for source disposal and cross-environment restore.

### D11: lease timing and idle release are independent policies

The Sandbox lease is a liveness fence for a Worker owner. Idle retention is a
Session business policy. Neither duration authorizes the other's transition,
so the lease TTL is not required to be greater than
`checkpoint_after_secs`, checkpoint duration, or checkpoint retention.

The only timing constraints are local to each policy:

```text
renew_interval <= lease_ttl / 3
recovery_grace >= lease_ttl + reconciliation_interval
checkpoint_after > 0
retention > checkpoint_after
checkpoint execution <= max_checkpoint_duration
```

Lease expiry yields only `Fence`; it never disposes a Sandbox. A fenced live
Sandbox remains protected while the Session aggregate references its binding.
Only the ordered suspend saga may release it after checkpoint evidence is
committed, and only terminal cleanup may release it under the terminal fence.
This separation lets operators tune failure detection and cost independently:
shorten lease TTL to detect a dead Worker faster; shorten
`checkpoint_after_secs` to reclaim idle compute faster; lengthen retention to
increase the continuation window. Changes to either setting still pass its own
validation and do not introduce a cross-policy inequality.

## Consequences

- Idle Sessions can release their complete execution environment without losing
  the guaranteed filesystem continuation.
- First use, resume, update, and terminal paths continue through one Session and
  one Environment owner.
- Shared Agent/background concurrency becomes explicit and mechanically
  classifiable instead of inferred from primary Session status.
- Providers gain a stricter capability and a substantial conformance burden;
  unsupported backends fail closed rather than retain an inaccurate guarantee.
- Resume latency includes environment restoration, Resource realization, and MCP
  publication. Durable admission and idempotent joining prevent that latency
  from creating duplicate work.
- Filesystem continuation deliberately does not promise live process memory or
  connection continuation.

## Reuse, modification and new code

- Reused unchanged: Session root CAS/repository, `activity_epoch`, realization
  lease identity/fencing, durable WorkQueue/Worker claim recovery, immutable
  Environment and Resource pins, MCP generation protocol, process supervisor,
  terminal cleanup receipt pattern, and provider idempotent disposal after
  referenced-set authorization.
- Modified: `SessionEnvironmentState`, Environment execution policy/snapshot,
  Session activity and admission orchestration, the existing lifecycle outbox,
  `SessionRuntime`, Runtime Host
  background classification and quiescence, canonical `SandboxProvider`,
  existing Worker checkpoint-format admission, lease timing/fencing,
  referenced-set reconciliation, and terminal checkpoint deletion.
- New: checkpoint/generation/operation value objects and receipts, the typed
  pricing-neutral Session Running interval value, provider
  checkpoint/restore implementations, suspend/restore application
  reconciliation, provider conformance tests, and full-environment integration
  scenarios.

No new idle service, checkpoint manager, resume queue, Session registry, or
checkpoint metadata database is introduced.

## Alternatives considered

1. **Retain only Hand process hibernation.** Rejected because the Sandbox and
   mutable filesystem remain resident and cannot move across Workers.
2. **Dispose and rebuild from the Environment on every idle edge.** Rejected
   because runtime-installed packages and Agent-authored filesystem state are
   lost.
3. **Store process/VM memory with CRIU-like continuation.** Rejected for the
   initial contract because portability, security, kernel compatibility, and
   restore failure modes are much larger than the required filesystem
   guarantee.
4. **Add a standalone checkpoint lifecycle service.** Rejected because it would
   duplicate Session environment state and require distributed synchronization
   for activity, terminal, and restore races.
5. **Cancel suspension when a message arrives.** Deferred because completing
   one idempotent suspend and joining one restore is simpler and closes the
   ambiguous-upload race. Cancellation may be introduced later only if it
   preserves the same aggregate and effect identities.
