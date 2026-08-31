# Run Ingress And Message Delivery

This document owns the Dispatch / Server boundary for Run admission, durable
delivery, pending input, attempt-local control, and committed handoff into runtime.
It exists to keep durable delivery and distributed message mechanics out of
runtime core while still making the runtime boundary testable.

## Owning Context

| Behavior | Owner |
|---|---|
| Direct physical-attempt delivery | Runtime Core through `DirectAttemptDriver` |
| Durable queue, pending input, recovery, replay | Dispatch / Server through `RunDispatch`, `DispatchPool`, and `DispatchWorker` |
| Attempt-local input and neutral controls | Runtime `ActiveAttemptScope` |
| Public sessions and product status | Product adapters |

## Delivery Boundary

There is no common direct/durable service port. Application admission selects one
private foreground-delivery value, then both paths meet at the physical executor:

```text
Application admission
  |- Direct -> DirectAttemptDriver -------------------.
  `- Durable -> RunDispatch -> claim/epoch -> Worker -+-> ActiveAttemptScope
                                                        -> RunAttemptExecutor
                                                        -> ThreadCommit
```

`DirectAttemptDriver` has only `start`, `resume`, and live cancellation. Durable
submission is unrepresentable on that type rather than a runtime error branch.

Durable delivery persists `RunDispatch`, then `DispatchPool` and
`DispatchWorker` own claim, fencing, recovery, pending-input correlation,
materialization, execution, and settlement. Production Hosts compose those
authorities directly; no durable-ingress façade is stored beside the Worker.

## Simplified Current Shape

The current design removes the older common port and capability report. Public
server code sees application commands; infrastructure sees the exact owner:

| Surface | Purpose | Must not include |
|---|---|---|
| Session/Run application | authorize, reserve, activate, and select direct or durable delivery | queue claim state, LiveInbox identity |
| `DirectAttemptDriver` | run one queue-less physical attempt | durable submit/recovery/query methods |
| `DispatchQueue` / `DispatchPool` | persist, wake, claim, fence, and settle durable work | Run outcome or Thread transcript truth |
| `ActiveAttemptScope` | register one exact process-local attempt and optionally create its fresh LiveInbox | Session cache, cross-attempt carry-over, remote mailbox |
| operational query | inspect durable dispatch state | message payload truth, public Session status |

The private delivery enum replaces the former trait object, durability boolean,
and optional concrete durable wrapper. Live input is not a routing fallback:
only an already-active local attempt can expose it.

Pending edit, retract, reorder, and recovery operations belong to the
thread-message or operations surface. They are not direct-driver or Dispatch internals.

## External Use Surface

External callers use run ingress indirectly through product/server adapters.
Those adapters translate public payloads into neutral commands and keep public
ids, statuses, auth grants, and protocol naming outside the run-ingress contract.

External inbound messages use pending input because an application may accept
them before it owns a concrete Run. Internal Agent coordination does not share
that acceptance mechanism merely because its payload is text: source Thread
state owns the committed tool request, and the deterministic target Run owns its
frozen activation input (ADR-0017). Both become target-Thread transcript truth
through `ThreadCommit`, but they enter that boundary through different domain
commands.

| External operation | Public owner | Internal target | Boundary rule |
|---|---|---|---|
| start or continue a run | Product adapter / Server | application admission then direct driver or `RunDispatch` | payload becomes immutable activation data |
| cancel, decide, wake, or resume | Product adapter / Server | exact active-attempt control or durable Dispatch/PendingInput path | no common control façade guesses the target state |
| send external input | Product message adapter | target-thread pending append | accepted-before-Run input uses the pending/freeze/commit lifecycle |
| coordinate another Agent | internal `send_message` command | source `ActiveToolBatch` plus deterministic target activation | no PendingInput or Dispatch outbox duplicates the Thread-owned request |
| edit, retract, or reorder pending input | Thread-message API | pending input store with revision checks | not a run-ingress route or dispatch mutation |
| inspect queued/recoverable work | Operations surface | dispatch projection/query | no message payload truth or public session status |
| receive stream or replay | Protocol adapter | committed events/facts plus live stream when connected | replay derives after commit; live stream is best-effort |

An attempt-local `LiveInbox` is not durable ingress and is never a remote
mailbox. `ActiveAttemptScope` creates a fresh instance only while the serving
process owns the active attempt and the executor advertises safe-boundary input.
For a queued, awaiting, ended, or remotely placed run, the live endpoint fails
closed and the caller submits the message through the ordinary Session event
path. A closed scope discards unconsumed best-effort input; it never carries it
into the next attempt or silently duplicates it into `PendingInput`. There is no
parallel steer queue and no Coordinator-to-Worker callback.

External extensions that need durable delivery should register tools, action
kinds, backend adapters, or protocol adapters above this boundary. They should
not reintroduce a common direct/durable port. Extend the exact application,
Dispatch, executor, or attempt-control authority that owns the new behavior.

## Durable Delivery Responsibilities

Keep durable ingress internals named by ownership, not exposed as public seams.
The design should care about the authority, not the private struct name:

| Responsibility | Owns | Stable boundary it supports |
|---|---|---|
| Durable input buffering | durable submit, decision, wake, and pending input intake | `DispatchQueue` / `Inbox` |
| Dispatch coordination | claim, reconcile, freeze, prepare, activate | `DispatchPool` / `DispatchWorker` |
| Live binding | one exact attempt's controls and optional inbox | `ActiveAttemptScope` |
| Recovery replay | startup scan, reclaim, replay decision | `DispatchService` / `DispatchWorker` |
| Event handoff | observe committed runtime events or stage adapter-owned drafts through the commit boundary | `DurableEventSink` and event-store ports |
| Resolution preparation | carry resolved config and catalog-fingerprint data without owning resolution policy | `RunResolver` and `RunDispatch` |

Application routes depend on their application service. Infrastructure adapters
depend only on the exact Dispatch or attempt-control authority they invoke.

## Durable Ingress Component Catalog

This catalog names the stable durable-ingress boundary roles by authority. Do
not add private helper names here. If an internal component is split or renamed
without changing a cross-boundary authority, update Rustdoc and the internal
responsibility table above rather than expanding this catalog.

| Name | Kind | Owns | Uses | Must Not Own | Failure Mode | Guardrail/Test |
|---|---|---|---|---|---|---|
| `DirectAttemptDriver` | queue-less concrete driver | direct start/resume and exact attempt scope | `RunAttemptExecutor`, Runtime registry | durable queue, recovery, replay | direct/durable invalid combinations are unrepresentable | G5; direct driver tests |
| `DispatchQueue` | durable delivery authority | dispatch/pending rows and claim transitions | durable backend | Run outcome, Thread transcript | a second execution state machine emerges | G5/G6; backend conformance |
| `DispatchWorker` | claimed execution composition | fence, physical attempt, execute/resume, settlement | Queue, executor, committed view | application admission, LiveInbox persistence | stale or duplicated execution | G5/G6; recovery and fencing tests |
| `ActiveAttemptScope` | process-local attempt owner | generation, neutral controls, fresh optional LiveInbox | Runtime registry, executor live-input capability | durable delivery or cross-attempt input | stale discovery or phantom durability | G18; generation/ownership tests |
| `SubmitCommand` | command value | neutral activation or message submit data plus caller intent | `RunActivation`, caller/server intent | runtime loop state, durable internals, routing mode, batching policy | delivery policy leaks into public route contracts | G5, G6; submit-mode tests |
| `RunDispatch` | data value | ingress-to-runtime execution data without live handles | `RunActivation`, durable persistence hints | registry handles, commit coordinator, inbox, cancellation handles | durable replay depends on process-local objects | G3, G4; serialization and replay tests |
| `WorkerContext` | ingress worker wiring | sink, thread context, pending boundary, remote wait, and optional commit/catalog wiring used to build `RuntimeRunContext` | durable worker execution construction | durable input storage, product DTOs, immutable activation data | dispatch data and live handles become indistinguishable | G3, G13; worker-context tests |

`CommitCoordinator` and event-store ports are consumed by durable ingress, but
they are owned by the runtime/store contract. Durable ingress may verify
same-source wiring; it must not redefine the commit mechanism.

`RunDispatch` remains durable data. `WorkerContext` is the ingress adapter
that recreates the runtime-facing `RuntimeRunContext` for one execution attempt.
An internal launcher or host service may exist, but it is not a stable
cross-boundary role unless it gains authority beyond preparing a request and
calling `RunExecutor`.

## Implemented Slice

A first slice of this boundary ships in the `awaken-run-ingress` crate
([ADR-0009](../adr/0009-durable-run-ingress-slice.md)); the Rustdoc there co-owns
the realized behaviour, this document owns the boundary it must keep. Realized
roles: `DirectAttemptDriver`, `ActiveAttemptScope`, `RunDispatch`,
`DispatchQueue` / `DispatchPool` / `DispatchWorker`, `WorkerContext`, the
`RunDispatch` queue (enqueue, single-owner claim/lease, lease-expiry recovery)
and the `PendingInbox` (idempotent append) backed by an in-memory reference store
and a Postgres adapter. The worker decides execute-versus-resume from committed
truth, so the queue never becomes a second authority. Pending input is keyed to
the resume-ticket correlation it answers, so a resume that committed before the
worker settled is never re-applied after a crash, without an atomic append+freeze
([ADR-0010](../adr/0010-idempotent-pending-consumption.md)). A `DispatchService`
daemon drains the queue on a nudge or poll and recovers crashed leases on a
`Clock` injected at the edge, keeping the worker deterministic
([ADR-0011](../adr/0011-autonomous-dispatch-service.md)). The commit and dispatch
layers run on Postgres or embedded SQLite over one portable schema
([ADR-0012](../adr/0012-sqlite-and-postgres-store-backends.md)). Pending input is
mutable before consumption under revision-guarded `edit`/`retract`, and
external cross-service delivery uses a transactional outbox with idempotent
append-then-delete (no 2PC)
([ADR-0013](../adr/0013-pending-lifecycle-and-cross-thread-outbox.md)); a nullable
`available_at` schedules a delivery the daemon fires when due, so `scheduled_wake`
is true ([ADR-0014](../adr/0014-scheduled-delivery.md)). A crash-retry budget
atomically claims a poison run past `max_attempts` recoveries and commits
`Ended(Indeterminate)` through the ordinary Worker terminal/`Done` path;
`DeadLetter` is reserved for explicit operator quarantine, with
`dead_letters`/`requeue` ops
([ADR-0015](../adr/0015-crash-retry-budget-and-dead-letter.md)).
The active drainer owns this special-first scheduling; coordinator-only
maintenance preserves the row while no remote Worker is available.
A queued or awaiting run is cancelled durably — the dispatch is removed and a
terminal `Cancelled` fact is committed through the one finish boundary
([ADR-0016](../adr/0016-durable-cancel.md)). Internal Agent messaging is owned by
source Thread state and a deterministic target activation, not this layer's
outbox ([ADR-0017](../adr/0017-send-message-over-outbox.md)). Fresh work is claimed by
priority, an `enqueue_with` dedupe key dedups concurrent submissions, and
`purge_dead_letters` is an operator GC over dead-lettered rows
([ADR-0018](../adr/0018-priority-dedupe-gc.md)). Multi-node dispatch works on
Postgres (concurrent distinct claim via `SKIP LOCKED`), `renew_lease` keeps a
long run owned, and a pluggable `WakeSignal` (local, or feature-gated NATS)
replaces the daemon's notify
([ADR-0019](../adr/0019-distributed-dispatch-and-wake-signal.md)).
`ScheduledAction` (ADR-0003 mechanism #1 — a committed in-run deferred action,
recovered from committed state for consistency, distinct from this layer's
delayed *delivery*) is a `AwaitReason`, staged by a gate `Schedule` and
performed in-process by the worker
([ADR-0020](../adr/0020-scheduled-action.md)). A message to a thread with no
awaiting run is staged as unbound input the thread's next run consumes
([ADR-0021](../adr/0021-idle-thread-delivery.md)). A submission can supersede a
thread's prior pending/awaiting work by epoch, newest-wins
([ADR-0022](../adr/0022-epoch-supersession.md)). The daemon GCs aged manual
dead-letter quarantines on its cadence
([ADR-0023](../adr/0023-dead-letter-ttl-gc.md)), a renewal heartbeat keeps a long
run's lease fresh across a fleet
([ADR-0024](../adr/0024-daemon-lease-renewal.md)), and `list_dispatches` is the
operational query surface
([ADR-0025](../adr/0025-dispatch-query-surface.md)). Deferred (named, not built):
auto-activating a run from an idle-thread message (needs the thread-snapshot
seam), committing a terminal Cancelled for superseded runs, force-superseding an
in-flight running run, resolving a scheduled-action kind to a concrete non-tool
action runner (the kind axis itself is built,
[ADR-0027](../adr/0027-scheduled-action-kind-axis.md)), and a NATS-backed store
(deferred by evidence — Postgres already gives distributed claim, and a KV store
cannot be verified without a server,
[ADR-0028](../adr/0028-nats-store-deferral.md); the NATS wake signal is built
and live-tested).

Caller-owned Run ids retain one canonical dispatch identity across every
admission path ([ADR-0060](../adr/0060-durable-dispatch-completion-tombstone.md)).
A live retry compares the complete `RunDispatch` except request-local
`traceparent`; an applied `Done` atomically retains the same identity as a
SHA-256 fingerprint on the existing completion tombstone. Different or legacy
unverifiable payloads fail closed before placement or supersession policy can
mask the collision. Exact retries preserve the first accepted dispatch options.

Admission errors preserve commit certainty instead of collapsing every store
failure into a rejection:

| Error | What is known | Required caller behavior |
|---|---|---|
| `Rejected` | This dispatch did not commit. | Correct the request or policy failure before submitting new intent. |
| `Conflict` | A different immutable dispatch identity already exists. | Preserve the existing activity; do not replace it with the colliding payload. |
| `Unavailable` | The response does not prove whether the dispatch committed. | Retry the exact same Run id and fingerprint; do not settle Session activity or create replacement intent. |

This distinction closes the response-loss gap: a database or transport failure
after commit cannot be interpreted as proof of non-commit. Backend identity
checks make the prescribed exact retry idempotent.

## Durable Semantics

Durable behavior is additive around the queue-less driver:

- `DirectAttemptDriver` remains transparent and fast for queue-less attempts;
- durable submission is absent from the direct-driver type instead of becoming
  an unsupported runtime branch;
- wake hints are non-authoritative and only trigger reconciliation;
- thread ownership is the durable input serialization boundary;
- committed runtime facts remain the authoritative state.

## Simple Design And DDD Evaluation

This design satisfies the simple-design target only while it keeps the following
shape:

1. One application admission boundary selects one private delivery value;
   `DirectAttemptDriver` and `RunDispatch` remain concrete owners with no common
   public service port.
2. One pending input lifecycle owned by the target thread.
3. One dispatch plane for claim, lease, retry, wake, and activation opportunity.
4. One runtime commit boundary for messages, run projection, state, events, and
   facts.
5. No route-level delivery-mode taxonomy.
6. No mailbox, background-task, or cell framework in the public surface.

The DDD aggregate split is:

| Aggregate or role | Owns | Must not own |
|---|---|---|
| Thread | pending input, committed message order, thread-scoped state | dispatch retry policy, product session status |
| RunRecord | accepted run intent, execution lifecycle, outcome projection | message payload truth, pending queue ownership |
| RunDispatch | activation opportunity, claim, lease, retry, wake, manual quarantine, or recovery state | run outcome, committed messages, agent-domain facts |
| Runtime Core | execution loop and staged `ThreadCommit` | durable queue internals, public message routes |
| Product adapter | public protocol request and projection names | runtime state names, dispatch truth |

If a proposed type owns data from two rows, split it before implementation.

## Run Ingress And Message Interaction Optimization

The optimized design keeps run delivery, message truth, and runtime execution as
separate authorities:

| Concern | Owner | Rule |
|---|---|---|
| External request parsing | Product adapter / Server | translate public payloads to neutral submit/control commands; do not leak public status names into runtime |
| Run delivery | Application plus private foreground-delivery value | select concrete direct execution or durable Dispatch admission once |
| Durable dispatch | `DispatchQueue` / `DispatchPool` / `DispatchWorker` | own claim, lease, retry, wake, and activation opportunity; never own message bodies as truth |
| Pending message intake | target thread message lifecycle | receive input as durable pending records keyed by stable message ids |
| Run execution | Runtime Core | consume frozen input at a safe boundary and commit runtime facts through `CommitCoordinator` |
| Committed messages | thread aggregate | append-only log with an append fence; projections and protocol replay derive after commit |

Simple design rule: one inbound delivery path appends pending input; separate
policies decide when a run may consume it. Avoid adding route-level concepts for
"active-run message", "new-run message", "background message", or "handoff
message" unless they map to a durable policy value with tests. The public route
should submit a message or control command. Boundary selection, batching, live
fallback, and wake behavior are durable-ingress policy, not product protocol
truth.

DDD rule: the thread is the consistency aggregate for pending and committed
messages. `RunRecord` owns execution intent and outcome. `RunDispatch` owns only
delivery opportunity, claim, lease, retry, and wake state. Moving message payload
truth into dispatch makes recovery and replay ambiguous because dispatch records
can be superseded, retried, or repaired independently from the thread log.

## Internal Agent Coordination Journey

Internal `send_message` is a command from the primary Thread to an ordinary
child Thread, not externally accepted message ingress. The source and target
have separate commit boundaries linked by stable identities; correctness comes
from idempotent recovery, not from a cross-aggregate transaction or outbox.

```text
primary ThreadCommit
  -> persist ActiveToolBatch call as Executing with stable operation id
  -> Session CAS opens one activity epoch for that operation
  -> enqueue deterministic child RunDispatch with frozen user message
  -> parent may commit the accepted tool receipt

child Worker
  -> claim exact RunDispatch epoch
  -> commit frozen input and Run state to the child Thread
  -> execute zero or more ToolCalls under that Thread's physical-attempt fence
  -> commit Awaiting or Ended boundary
  -> Session settles the activity, or transfers it to one deterministic
     primary report Run when a completed child has a committed report
  -> settle child Dispatch only after the Session boundary is accepted
```

The parent receipt and the first target Thread commit are intentionally
unordered after durable dispatch admission. In the ordinary path the parent
receipt commits first. If the parent process crashes, the child may finish and
settle before the parent receipt exists. Recovery reruns only the source
operation protocol: the same Run id and full dispatch fingerprint hit the
permanent completion tombstone, return idempotent success, and let the parent
commit its missing receipt without creating claimable target work.

| Failure window | Durable evidence | Recovery outcome |
|---|---|---|
| before source `Executing` commit | no accepted command | retry begins normally |
| after source commit, before dispatch admission | source `ActiveToolBatch` plus Session activity | exact operation retry; definitive rejection alone may settle activity |
| admission response lost | dispatch may or may not exist | exact Run/fingerprint retry; never infer rejection |
| parent receipt committed before child starts | source receipt plus pending dispatch | child executes normally; receipt does not settle Session activity |
| child settles before parent receipt commit | child Thread boundary plus dispatch completion tombstone | exact source retry commits only the missing receipt; no target re-execution |
| child commits but Worker crashes before settlement | child Thread boundary plus leased dispatch | replacement Worker observes committed truth, repeats Session observation, then settles |

Static ownership stays narrow: `ActiveToolBatch` owns the source request and
receipt; `PersistedSession` owns the activity fence; `RunDispatch` owns target
delivery and its permanent completion identity; child `ThreadCommit` owns target
message, Run, ToolCall, and report truth; Managed Session/Event data is a
projection only. `AgentMessageProtocol.tla` checks all ordering/failure
interleavings and separate early- and late-receipt reachability witnesses.

## Message Input Lifecycle

External message receipt and message consumption are intentionally different
commits:

```text
receive externally accepted input
  -> append target-thread pending message idempotently
  -> bump pending queue revision
  -> emit advisory wake / ensure activation opportunity
  -> durable ingress claims one thread owner
  -> freeze eligible pending input at a run boundary
  -> runtime executes with frozen input
  -> ThreadCommit appends committed messages and run projection atomically
  -> cleanup consumed pending; recovery may reconcile leftovers
```

Pending records are mutable only before freeze. Edit, retract, and reorder check
the pending record revision or queue revision. Once a pending record is frozen
into a run input snapshot, later user-visible changes must become new pending
input or ordinary committed facts; they must not mutate the frozen activation.

This pending-input outbox belongs to cross-service ingress, scheduling, and
delivery—not internal Agent coordination. Independently accepted external input
that must cross stores uses a sender outbox plus idempotent target pending append
without two-phase commit:

```text
accepting service transaction
  -> commit acceptance fact
  -> enqueue PendingInputOutbox(message_id, target_thread_id, payload)

relay/recovery
  -> append target pending by message_id
  -> mark sender outbox delivered after idempotent success
```

If the ack is lost, retrying the same `message_id` returns idempotent success on
the target. If the process crashes before target append, the outbox scan retries.
If target append commits but wake delivery is lost, pending-thread recovery
recreates the activation opportunity.

## Distributed Placement Rules

For distributed deployment, `thread_id` is the shard and consistency key:

| Distributed rule | Reason |
|---|---|
| Many nodes may append pending input when `message_id` idempotency and pending revision CAS are enforced. | concurrent receipt is safe without a global queue lock |
| Only one owner may freeze pending input, prepare a run input snapshot, or execute the active run for one thread. | committed message order and run projection remain serializable |
| Wake records are hints. Durable pending input, committed facts, leases, and outboxes are the recovery truth. | lost or duplicate notifications do not change correctness |
| Runtime reads and commits must use the same source or a fenced `RuntimeExecutionLink` equivalent. | resume cannot observe a torn mix of messages, run projection, and state |
| Remote control requires an ack or minimum fence for read-your-writes paths. | callers can wait until a command is durably visible before reading projection state |
| External cross-service delivery uses transactional outbox and idempotent target append, not 2PC. | each thread remains an independent aggregate with local recovery |

The distributed API should stay narrow: submit input, deliver live control, read
committed events/projections, and inspect durable dispatch state. Pending edit,
retract, reorder, and recovery operations belong to the thread-message or
operations surface, not to public dispatch internals.

## Execution Placement Boundary

This document does not define process lifecycle, mount handling, or
execution-owned leases. The runtime executes tools in-process; the OS process
lifecycle and durable-dispatch mechanics around it are a server/dispatch concern
and are out of scope here. Run ingress may claim a run-dispatch lease, but that
lease is a server-side delivery authority for one thread/run opportunity, not a
general execution lease.

## Failure And Recovery

- `DirectAttemptDriver` failure is live-control or direct execution failure only.
- `DispatchService` / `DispatchWorker` recover by scanning durable pending state and replaying
  from committed facts.
- Retry exhaustion is first claimed under a newer dispatch epoch, then the one
  Worker terminal path commits `Ended(Indeterminate)` and settles `Done`; commit
  failure leaves the dispatch reclaimable after lease expiry.
- Duplicate wake hints are safe because reconciliation reads authoritative store
  state.
- An execution crash maps to a typed tool/backend failure or
  `Indeterminate`; it does not write runtime truth directly.

## Non-Goals

- No universal execution framework inside runtime or run ingress.
- No execution heartbeat as runtime liveness truth.
- No durable ingress internals as route-level dependencies.
- No product session status in dispatch internals.

## Guardrails

G1, G5, G6, and G13 in [INVARIANTS](../INVARIANTS.md).
