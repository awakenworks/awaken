# Run Ingress And Message Delivery

This document owns the Dispatch / Server boundary for run ingress, durable
delivery, pending message intake, and message-consumption handoff into runtime.
It exists to keep durable delivery and distributed message mechanics out of
runtime core while still making the runtime boundary testable.

## Owning Context

| Behavior | Owner |
|---|---|
| Direct run submission and live control | Runtime Core through `DirectRunIngress` role projections |
| Durable queue, pending input, recovery, replay | Dispatch / Server through `DurableRunIngress` |
| Public sessions and product status | Product adapters |

## Run Ingress

Use one server-facing port:

```text
RunIngress
  |- DirectRunIngress   -> direct runtime ingress
  `- DurableRunIngress  -> durable buffered ingress
```

`DirectRunIngress` submits a prepared activation to the runtime through
`RunExecutor` and forwards live control through `LiveRunControl`. It has no
durable queue, no recovery scan, no replay, and no scheduled wake.

`DurableRunIngress` delegates to durable ingress internals. It adds durable
pending input, dispatch claiming, wake reconciliation, resolved-config
materialization, lifecycle wiring, and committed-event handoff before calling the
narrow runtime roles.

## Simplified Current Shape

The current design intentionally collapses the older run-ingress surface. Public
server code should see one port and a small command family:

| Public surface | Purpose | Must not include |
|---|---|---|
| `submit` | accept a neutral run submit or internally authorized message handoff command | live-vs-queue routing mode, batching policy, pending edit semantics |
| `control` | cancel, deliver decision, or wake an active/pending boundary | durable queue internals, product protocol names |
| `capabilities` | report whether the selected ingress supports durable, recoverable, replayable, or scheduled-wake behavior | runtime execution semantics or product policy |
| optional query | inspect durable dispatch state for operations | message payload truth, public session status |

Delivery routing such as live-vs-durable, inline claim, live-then-queue,
boundary selection, batching, and fallback is an internal durable-ingress policy.
It must not become stable route vocabulary or a public `DeliveryIntent` axis.
If an implementation needs a policy value, keep it private to durable ingress and
prove it with table-driven tests.

Pending edit, retract, reorder, and recovery operations belong to the
thread-message or operations surface. They are not public `RunIngress` internals.

## External Use Surface

External callers use run ingress indirectly through product/server adapters.
Those adapters translate public payloads into neutral commands and keep public
ids, statuses, auth grants, and protocol naming outside the run-ingress contract.

Internal multi-agent messages and external inbound messages share the same
bottom mechanism: idempotent append to the target thread's pending input, later
freeze at a safe run boundary, then durable visibility through `ThreadCommit`.
They differ only at the edge. Internal messaging enters through the
`send_message` tool/effect or delegation adapter; external messaging enters
through a product/protocol anti-corruption adapter that translates public ids,
auth, and DTOs before it appends pending input.

| External operation | Public owner | Internal target | Boundary rule |
|---|---|---|---|
| start or continue a run | Product adapter / Server | `RunIngress.submit` | payload becomes neutral activation or message submit data |
| cancel, decide, wake, or resume | Product adapter / Server | `RunIngress.control` or `LiveRunControl` | control is observed at safe runtime boundaries; durable fallback stays ingress-owned |
| send a message | internal `send_message` tool/effect or external message adapter | target-thread pending append | both internal and external messages use the same pending/freeze/commit lifecycle |
| edit, retract, or reorder pending input | Thread-message API | pending input store with revision checks | not a run-ingress route or dispatch mutation |
| inspect queued/recoverable work | Operations surface | dispatch projection/query | no message payload truth or public session status |
| receive stream or replay | Protocol adapter | committed events/facts plus live stream when connected | replay derives after commit; live stream is best-effort |

External extensions that need durable delivery should register tools, action
kinds, backend adapters, or protocol adapters above this boundary. They should
not add new `RunIngress` methods unless they introduce a new authority that
cannot be represented as submit, control, capability, query, message adapter,
executor adapter, or resume.

## Durable Ingress Internal Responsibilities

Keep durable ingress internals named by ownership, not exposed as public seams.
The design should care about the authority, not the private struct name:

| Responsibility | Owns | Stable boundary it supports |
|---|---|---|
| Durable input buffering | durable submit, decision, wake, and pending input intake | `RunIngress` / `DurableRunIngress` |
| Dispatch coordination | claim, reconcile, freeze, prepare, activate | `DurableRunIngress` |
| Live binding | active runtime handles and live-delivery access | `LiveRunControl` and `RuntimeRunContext` construction |
| Recovery replay | startup scan, reclaim, replay decision | `DurableRunIngress` recovery behavior |
| Event handoff | observe committed runtime events or stage adapter-owned drafts through the commit boundary | `DurableEventSink` and event-store ports |
| Resolution preparation | carry resolved config and catalog-fingerprint data without owning resolution policy | `RunResolver` and `RunExecutionRequest` |

Routes depend on `RunIngress`, not directly on these internals.

## Durable Ingress Component Catalog

This catalog names the stable durable-ingress boundary roles by authority. Do
not add private helper names here. If an internal component is split or renamed
without changing a cross-boundary authority, update Rustdoc and the internal
responsibility table above rather than expanding this catalog.

| Name | Kind | Owns | Uses | Must Not Own | Failure Mode | Guardrail/Test |
|---|---|---|---|---|---|---|
| `RunIngress` | server-facing delivery port | delivery semantics and capability reporting | direct or durable ingress implementation | runtime internals, durable ingress internals, product DTOs | unsupported durable behavior is hidden until runtime | G5; capability tests |
| `DirectRunIngress` | queue-less ingress implementation | direct submit/control over live runtime roles | `RunExecutor`, `LiveRunControl` | durable queue, recovery, replay, scheduled wake | caller assumes durability that does not exist | G5; direct rejects durable-only operations |
| `DurableRunIngress` | durable ingress implementation | durable delivery contract for submit, decision, wake, replay, scheduled wake, and recovery operations | durable stores, runtime role ports, event-store ports | runtime loop internals, product protocol status, public DTOs | routes depend on durable internals or assume direct ingress is durable | G5, G6; route parity and recovery tests |
| `RunIngressCapabilities` | capability value | explicit durable, recoverable, replayable, and scheduled-wake support | selected ingress implementation | runtime execution semantics, product policy, background-task semantics | server exposes unsupported operation as if it were safe | G5; fail-closed route tests |
| `SubmitCommand` | command value | neutral activation or message submit data plus caller intent | `RunActivation`, caller/server intent | runtime loop state, durable internals, routing mode, batching policy | delivery policy leaks into public route contracts | G5, G6; submit-mode tests |
| `RunExecutionRequest` | data value | ingress-to-runtime execution data without live handles | `RunActivation`, durable persistence hints | registry handles, commit coordinator, inbox, cancellation handles | durable replay depends on process-local objects | G3, G4; serialization and replay tests |
| `RunExecutionContext` | live wiring adapter | sink, thread context, pending boundary, remote wait, and optional commit/catalog wiring used to build `RuntimeRunContext` | runtime execution construction | durable input storage, product DTOs, immutable activation data | request data and live handles become indistinguishable | G3, G13; execution-context tests |

`CommitCoordinator` and event-store ports are consumed by durable ingress, but
they are owned by the runtime/store contract. Durable ingress may verify
same-source wiring; it must not redefine the commit mechanism.

`RunExecutionRequest` remains durable data. `RunExecutionContext` is the adapter
that recreates the runtime-facing `RuntimeRunContext` for one execution attempt.
An internal launcher or host service may exist, but it is not a stable
cross-boundary role unless it gains authority beyond preparing a request and
calling `RunExecutor`.

## Implemented Slice

A first slice of this boundary ships in the `awaken-run-ingress` crate
([ADR-0009](../adr/0009-durable-run-ingress-slice.md)); the Rustdoc there co-owns
the realized behaviour, this document owns the boundary it must keep. Realized
roles: `RunIngress` / `DirectRunIngress` / `DurableRunIngress`,
`RunIngressCapabilities`, `RunExecutionRequest` / `RunExecutionContext`, the
`RunDispatch` queue (enqueue, single-owner claim/lease, lease-expiry recovery)
and the `PendingInbox` (idempotent append) backed by an in-memory reference store
and a Postgres adapter. The worker decides execute-versus-resume from committed
truth, so the queue never becomes a second authority. Pending input is keyed to
the waiting-ticket correlation it answers, so a resume that committed before the
worker settled is never re-applied after a crash, without an atomic append+freeze
([ADR-0010](../adr/0010-idempotent-pending-consumption.md)). A `DispatchService`
daemon drains the queue on a nudge or poll and recovers crashed leases on a
`Clock` injected at the edge, keeping the worker deterministic
([ADR-0011](../adr/0011-autonomous-dispatch-service.md)). The commit and dispatch
layers run on Postgres or embedded SQLite over one portable schema
([ADR-0012](../adr/0012-sqlite-and-postgres-store-backends.md)). Pending input is
mutable before consumption under revision-guarded `edit`/`retract`, and
cross-thread delivery uses a transactional outbox with idempotent
append-then-delete (no 2PC)
([ADR-0013](../adr/0013-pending-lifecycle-and-cross-thread-outbox.md)); a nullable
`available_at` schedules a delivery the daemon fires when due, so `scheduled_wake`
is true ([ADR-0014](../adr/0014-scheduled-delivery.md)). A crash-retry budget
dead-letters a poison run past `max_attempts` recoveries, with `dead_letters`/
`requeue` ops ([ADR-0015](../adr/0015-crash-retry-budget-and-dead-letter.md)).
A queued or parked run is cancelled durably — the dispatch is removed and a
terminal `Cancelled` fact is committed through the one finish boundary
([ADR-0016](../adr/0016-durable-cancel.md)). The `send_message` builtin tool is
backed by the outbox through a host adapter, addressed by thread
([ADR-0017](../adr/0017-send-message-over-outbox.md)). Fresh work is claimed by
priority, an `enqueue_with` dedupe key dedups concurrent submissions, and
`purge_dead_letters` is an operator GC over dead-lettered rows
([ADR-0018](../adr/0018-priority-dedupe-gc.md)). Multi-node dispatch works on
Postgres (concurrent distinct claim via `SKIP LOCKED`), `renew_lease` keeps a
long run owned, and a pluggable `WakeSignal` (local, or feature-gated NATS)
replaces the daemon's notify
([ADR-0019](../adr/0019-distributed-dispatch-and-wake-signal.md)).
`ScheduledAction` (ADR-0003 mechanism #1 — a committed in-run deferred action,
recovered from committed state for consistency, distinct from this layer's
delayed *delivery*) is a `WaitingReason`, staged by a gate `Schedule` and
performed in-process by the worker
([ADR-0020](../adr/0020-scheduled-action.md)). Deferred (named, not built):
unsolicited delivery to an idle thread (new-input semantics), time-windowed
auto-GC, epoch-based supersession, a NATS-backed store (JetStream durability),
daemon-driven lease-renewal scheduling, a plugin-owned action-kind axis, and the
`RunDispatch*` query/lifecycle store roles above.

## Durable Semantics

Durable behavior is additive:

- direct ingress remains transparent and fast;
- durable-only calls return an unsupported/fail-closed error on direct ingress;
- wake hints are non-authoritative and only trigger reconciliation;
- thread ownership is the durable input serialization boundary;
- committed runtime facts remain the authoritative state.

## Simple Design And DDD Evaluation

This design satisfies the simple-design target only while it keeps the following
shape:

1. One public ingress port, with direct and durable implementations.
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
| RunDispatch | activation opportunity, claim, lease, retry, wake, dead-letter or recovery state | run outcome, committed messages, agent-domain facts |
| Runtime Core | execution loop and staged `ThreadCommit` | durable queue internals, public message routes |
| Product adapter | public protocol request and projection names | runtime state names, dispatch truth |

If a proposed type owns data from two rows, split it before implementation.

## Run Ingress And Message Interaction Optimization

The optimized design keeps run delivery, message truth, and runtime execution as
separate authorities:

| Concern | Owner | Rule |
|---|---|---|
| External request parsing | Product adapter / Server | translate public payloads to neutral submit/control commands; do not leak public status names into runtime |
| Run delivery | `RunIngress` | accept submit/control intent and report capabilities; direct and durable implementations differ only by delivery guarantees |
| Durable dispatch | `DurableRunIngress` internals | own claim, lease, retry, wake, and activation opportunity; never own message bodies as truth |
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

## Message Input Lifecycle

Message receipt and message consumption are intentionally different commits:

```text
receive input or SendMessage result
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

Same-thread `send_message` may append target pending input in the sender's
checkpoint transaction only when the implementation can prove both writes share
one commit source. Cross-thread `send_message` uses a sender outbox plus
idempotent target pending append. It must not require two-phase commit:

```text
sender ThreadCommit
  -> append sender facts/messages
  -> enqueue SendMessageOutbox(message_id, target_thread_id, payload)

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
| Cross-thread delivery uses transactional outbox and idempotent target append, not 2PC. | each thread remains an independent aggregate with local recovery |

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

- `DirectRunIngress` failure is live-control or direct execution failure only.
- `DurableRunIngress` recovers by scanning durable pending state and replaying
  from committed facts.
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
