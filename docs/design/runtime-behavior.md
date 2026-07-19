# Runtime Behavior, State, Effects, And Extensions

This doc covers behavior inside the runtime core: run states, state/effect
application, plugin hooks, stop/cancel semantics, scheduled work, context
compaction, and runtime-facing observability. It is the development guide for
features that change how a run executes without adding server or product
semantics.

## Owning Context

| Behavior | Owner |
|---|---|
| Run state machine, tool-call lifecycle, cancellation, stop policy | Runtime Core |
| Thread/run/message facts, state keys, snapshots, effects, neutral runtime events | Runtime Core and Store contracts |
| Plugin registration, hook filtering, state-machine extension | Runtime Core extension seam |
| Scheduled actions, reminders, deferred work | Runtime extension plus Dispatch / Server for durable wakeup |
| Trace export, datasets, eval history, admin review | Projection / Analytics above committed runtime facts |
| Public protocol event names, UI-specific payloads | Product adapters |

## Run Lifecycle

A run is a runtime aggregate rooted in its thread. The minimal lifecycle is:

```text
RunActivation
  -> resolve validated runtime input
  -> build context
  -> model/tool phase loop
  -> stage state/effects/events
  -> commit
  -> continue, wait, cancel, stop, or finish
```

Implementation rules:

- state transitions are typed runtime states, not public protocol statuses;
- every state-changing step stages through the commit boundary;
- thread ownership serializes conflicting writes for the same thread;
- pending input and resume commands enter through dispatch/server ingress, then
  become runtime commands;
- tool-call suspension and resume are runtime decisions, with public names mapped
  by adapters.

Parallel tools may execute concurrently only when their writes are safe under the
state/effect model. A same-key conflict must fail or serialize by policy; it must
not produce last-write-wins behavior by accident.

## Run And Thread Lifecycle Boundary

Run and thread lifecycle are related, but they are not the same axis. A run owns
the execution phase. A thread owns the durable serialization boundary for
messages, the latest run projection, and thread-scoped state.

```text
RunActivation
  -> live runtime loop and StateStore
  -> RunRecord { state, run-scoped PersistedState }
  -> ThreadCommit { message write intent, run projection, thread-state snapshot }
  -> CommitCoordinator checkpoint
  -> ThreadResumeSnapshot { committed messages, message version, latest run, thread state }
```

The committed run `RunState` is the single stored terminal authority — a sum type,
not a flat status plus a separate outcome field. The runtime commits a state only
at an await or an end; `Created` and `Running` are live, uncommitted states
that never become a stored fact.

| Committed `RunState` | Payload | Rule |
|---|---|---|
| `Awaiting` | resume ticket (in the checkpoint's awaiting slot, not on the run fact) | awaiting on a structured reason; resumes only through ingress, dispatch, or recovery. Awaiting is not an end and carries no end cause |
| `Ended` | `EndCause` | the single terminal authority: `NaturalEnd`, `MaxSteps`, `Cancelled`, or `Error(Failure)`. Run status, the published outcome, and the error flag are *derived* from it and never stored beside it ([ADR-0005](../adr/0005-run-terminal-state-single-authority.md)) |

`EndCause::Error` carries a `Failure` kind (`Inference`, `CapabilityBound`,
`StateConflict`); the fault detail lives in the committed authority, not in a
separate status string. The resume ticket keeps same-run resume data structured;
its reason today is `ToolPermission`. Public status names, HTTP states, or product
protocol names are adapter projections and do not replace these runtime values.

The thread side follows different rules:

- the committed message log is append-only;
- appending messages requires a non-empty delta and an `expected_message_count`
  fence;
- run/state-only checkpoints use an explicit no-message write intent instead of
  smuggling an empty append;
- run-scoped state is stored on the `RunRecord`;
- thread-scoped state is written as a thread-state snapshot in the same
  checkpoint transaction when it changes;
- resume reads use a consistent `ThreadResumeSnapshot` containing committed
  messages, message version, latest run, and thread-scoped state.

This split keeps four concerns independent: execution phase, message-log
serialization, run-scoped state, and thread-scoped state. They are connected at
the checkpoint boundary only. If implementation code has both a live loop
run-lifecycle state key and a durable run lifecycle value, documentation and API
names must identify the plane: live loop control versus durable run projection.

## State, Actions, And Effects

State is addressed by typed keys and explicit scope:

| Scope | Use |
|---|---|
| Run | ephemeral state for one run |
| Thread | durable conversational or workflow state |
| Shared/Product | product resource state reached through an approved port, not stored in the runtime state map |
| Profile/Config | selected configuration, pinned before runtime execution |

Actions request a state transition. Applying a patch to `StateStore` changes only
the active run's live revisioned projection. Durable truth appears only when the
next `ThreadCommit` exports the relevant `PersistedState`, facts, and events
through `CommitCoordinator`. Ordinary effects are post-apply requests interpreted
by registered handlers; if an effect must survive crash or retry, model it as a
committed fact, durable outbox entry, or scheduled action. Resume snapshots are
projections of committed facts and committed state, not a second source of truth.

Development rules:

1. Define the `StateKey` or effect type before adding convenience APIs.
2. Decide the scope and conflict policy up front.
3. Stage durable state export with the run commit.
4. Rebuild resume snapshots from committed facts in tests.
5. Keep shared/product state behind resource or product ports.

## Runtime State Model

The runtime state model has one write path and several read/projection shapes:

```text
Snapshot(revision n)
  + StateCommand
  -> validate keys, handlers, and optional base revision
  -> merge by StateKey conflict policy
  -> apply MutationBatch to the live StateStore
  -> Snapshot(revision n + 1)
  -> dispatch post-apply ordinary effects
  -> export run-scoped and thread-scoped PersistedState at checkpoint
```

The model deliberately separates these concepts:

| Concept | Role | Rule |
|---|---|---|
| Committed run `RunState` | durable run state value on `RunRecord` | illegal combinations are unrepresentable; a resume ticket lives only in `Awaiting`, the `EndCause` authority only in `Ended`, and status/outcome are derived, never stored |
| `StateKey` | typed extension-state identity | owns value/update types, validation, apply logic, merge policy, scope, and serde |
| `StateCommand` | runtime command envelope | carries a state patch plus scheduled actions and effects from hooks or tools |
| `MutationBatch` | atomic state patch | validates registered keys and base revision before applying all updates or none |
| `StateStore` | live revisioned projection | materializes the current run state; it is not the durable store or fact log |
| `Snapshot` | read-only state view | supplied to hooks/tools/effect handlers; mutation returns through `StateCommand` |
| `PersistedState` | durable export/import shape | serializes registered persistent keys for run-scoped and thread-scoped storage |
| External/product state | product/resource data | addressed through approved ports; never hidden inside runtime extension state |
| Event/activity state | progress/projection data | live or derived output; not authoritative runtime state |

`StateCommand` keeps hook and tool side effects on the same path. A phase hook
or tool may read a snapshot, but it cannot mutate the store directly. It returns
a command; the runtime validates registered keys and handlers, applies the
`MutationBatch` to the live `StateStore`, then dispatches ordinary effects
against the resulting
snapshot.

The live state apply is not the durable commit boundary. Use "apply" for
`StateStore` revision changes and reserve "commit" for `ThreadCommit` /
`CommitCoordinator` durable visibility.

Persistence is scope-aware:

- `Run` scoped keys are cleared at run start and persist on the run projection
  only when marked persistent.
- `Thread` scoped keys survive across runs on the same thread and are written as
  thread-state snapshots with the same checkpoint transaction.
- `Shared/Product` state stays outside the runtime state store and is reached by
  resource, profile, or product ports.

Parallel writes are safe only when the key says they are safe:

- disjoint keys may merge;
- overlapping `Commutative` keys may merge;
- overlapping `Exclusive` keys fail or are serialized by the phase/tool
  pipeline;
- hidden last-write-wins is a bug.

Use an ordinary effect handler only for best-effort post-apply work. Use a
scheduled action, durable outbox, committed fact, or dispatch/server wake when
the system must replay, retry, or recover that work after a crash.

## Runtime Event Model

Runtime events are neutral runtime-domain records. They are not product protocol
events and they are not an independent truth source. Names stay short inside
their module; paths express the context:

| Path | Public alias | Meaning |
|---|---|---|
| `agent::stream::Event` | `StreamEvent` | live stream output; may be early, best-effort, and lossy |
| `agent::stream::Sink` | `StreamSink` | current live connection delivery |
| `agent::event::Draft` | `EventDraft` | pre-commit candidate event |
| `agent::event::Record` | `EventRecord` | committed neutral event record |
| `agent::event::Reader` / `Subscriber` | `EventReader` / `EventSubscriber` | after-commit durable event read or subscription |
| `agent::fact::run` | no standalone alias | run fact builders/readers and run projection payloads |
| `agent::fact::thread` | no standalone alias | thread commit visibility facts and thread projection payloads |

Protocol events and replay rows are downstream projections. They are not defined
by the runtime event model and must be derived from committed event records or
facts when a protocol slice is added.

The runtime event pipeline is split by plane:

```text
phase / tool / plugin output
  -> StateCommand / Effect
  -> agent::stream::Event -> agent::stream::Sink
  -> agent::event::Draft + agent::fact::* commit plan
  -> CommitCoordinator
  -> agent::event::Record + agent::fact::*
  -> agent::event::Sink / replay projection
```

Ordering rules:

- live `agent::stream::Sink` order follows emission order for one run activation;
- durable `agent::event::Record` order follows commit order;
- thread-visible order follows the thread commit serialization boundary;
- a projection may lag, but it must catch up from committed event records or
  facts;
- a replayed run reuses committed facts and recorded verdicts instead of
  treating live pre-commit events as new truth.

Durability and failure rules:

- if the commit fails, `agent::event::Draft` values are discarded with the staged
  state and fact plan;
- if a `StreamSink` fails, runtime truth remains governed by committed facts and
  event records;
- protocol replay entries are derived from committed event records or facts, never
  from live stream events;
- implementation-local tracing and metrics can be best-effort, but they are not
  contract events and cannot be replay truth;
- plugin hooks can request state/effect/event output only through registered
  seams; they cannot write protocol events or replay entries directly.

Do not introduce a new event kind until it has a module path, an alias only when
it crosses a public boundary, a live or durable durability rule, an ordering rule,
and a projection/replay test.

## Call Timing And Commit Visibility

Hook, tool, and model calls belong to execution while they are running. Their
outputs cross into other axes only as data: `StateCommand` for state, event or
fact drafts for event/fact staging, and ordinary effects for post-apply
handling. None of those values is durable runtime truth until the run constructs
a `ThreadCommit` and the `CommitCoordinator` commits it.

The visibility rules are:

- call success means a candidate output exists, not that state changed;
- `StateCommand` and event drafts are validated and staged before commit;
- live `StreamEvent` output may be delivered before commit, but replay must be
  rebuilt from committed records;
- ordinary effects run only after their source state command has been validated
  and applied to the live state projection;
- durable effects, retries, scheduled actions, and outbox work must be modeled
  as committed facts or commit-owned records before they are treated as
  recoverable;
- projection, protocol replay, eval, and observability consume committed truth
  and never write it.

This keeps the call timing separate from the durable checkpoint boundary. The
execution axis invokes work; the state and event axes stage candidate changes;
the commit axis makes runtime truth visible; projection observes only after that
point.

## Plugins And Hooks

Plugins are runtime extensions. A plugin is a factory that declares a
`PluginManifest` and a `CapabilityBound`, then resolves to `Contributions` under
a call context. A plugin can add tools, hook phase behavior, state-machine logic,
permission gates, context builders, or observability sinks only through those
resolved contributions.

Hook guidance:

- hook activation is selected during resolution and materialized into runtime
  input;
- hook filters are capability configuration, not authorization grants;
- hooks cannot bypass tool descriptors, permission policy, or commit staging;
- hook output is data that the runtime validates and commits or rejects;
- product-facing labels stay outside the hook contract.

The state-machine extension should model a workflow as state, action, effect, and
guard logic over committed runtime state. It should not add a parallel workflow
store.

Plugin loading and plugin activation are separate decisions. `plugin_ids` selects
which plugins are loaded; the active plugin scope selects which loaded plugins
contribute runtime behavior. This is a deliberate non-orthogonal junction: it
affects phase hooks, gate hooks, plugin tools, transforms, keys, and lifecycle
hooks. Use the contribution matrix in
[runtime-interface-boundaries.md](runtime-interface-boundaries.md#plugin-contribution-matrix)
before adding a new plugin surface.

## Runtime Extension Surface

Runtime extension means "new behavior that enters through a declared runtime
seam and still commits through the normal thread/run boundary." It does not mean
adding a second scheduler, protocol, store, or product-specific task framework
inside runtime core.

Runtime core owns mechanisms that are necessary to make all runs replayable,
recoverable, and policy-checkable. A mechanism must be behavior-neutral by
default: installing the core alone must not add model-visible tools, mutate
prompts, choose delegate agents, schedule concrete work, call external services,
or change an agent's workflow. Behavior-changing contributions are installed by
plugins, first-party extensions, or external adapters and then validated by the
core mechanisms.

Use this split:

| Area | Core mechanism | Behavior contribution owner | Rule |
|---|---|---|---|
| Multi-agent delegation | tool-call, permission, child-run correlation, `RunIngress` / backend handoff | `agent_run` descriptor/tool, delegate roster config, local/remote execution adapter | core enables delegation safely; installed tools and target choices change agent behavior |
| Message delivery | committed message write intent, append fence, resume snapshot, frozen input consumption | internal `send_message` tool/effect, external message adapter, durable pending queue, recovery tool | core commits messages; adapters/tools decide how pending input arrives |
| Scheduled/background work | `ScheduledAction`, `ResumeTicket`, correlation/idempotency key, resume validation | action kinds, timers, concrete task tools, result adapters | no `BackgroundTask` umbrella; durable work is a committed request plus validated resume |
| Plugin mechanism | `Plugin` factory, `PluginManifest`, `CapabilityBound`, resolved `Contributions` (hook slots, tool gates, transform slots, key registry, output validation) | plugin packages, first-party extension bundles, product-selected active plugin scope | core resolves and bound-checks contributions; plugins decide behavior |
| Client-executed tools | pending `ResumeTicket` (client-tool await reason), descriptor fingerprint, neutral resume command validated by the shared `ResumeValidator` | public wait/result projection and client adapter | public result ids are projections; runtime validates the pending call before resume |
| State-machine workflows | typed state/effect/action/guard mechanism and replay-safe commit path | plugin or first-party extension workflow semantics | workflow state lives in runtime state/facts, not a parallel workflow store |
| Tool and capability expansion | `ToolDescriptor`, `ToolExecutor`, `ToolGateHook` contracts | tool package or MCP adapter | visibility and authorization remain separate decisions; tools run in-process |
| Context and memory | committed messages/facts, `ContextCompaction` fact shape, lineage checks | selected compaction policy, memory/resource adapters | summaries are append-only facts with lineage; source messages remain truth |
| Observability and eval | neutral events/facts and normal execution ports | analytics/eval surface, trace store, dataset builder | observability consumes runtime truth and never becomes runtime truth |
| Resource and credential access | opaque refs, permission policy, typed result boundaries | product data plane and orchestration layer above | runtime receives opaque references and typed results, not vault data or host authority |
| Protocol projection | committed `EventRecord` / facts and adapter mapping seam | product/protocol adapter owns public DTO names and replay rows | live stream output is not replay truth; protocol state derives after commit |

Core candidates must satisfy all of these checks:

1. The feature is needed to preserve runtime invariants for many unrelated
   agents.
2. The feature is behavior-neutral until configuration or a plugin supplies a
   contribution.
3. The feature can be represented as typed data, a port, a lifecycle value, or a
   validation rule in the runtime contract.
4. The feature has replay, commit, permission, or recovery tests that belong to
   the runtime package.

Plugin or external-adapter candidates include anything that names a concrete
tool id, prompt/context policy, model-visible descriptor, delegate roster,
scheduled action kind, external service, resource strategy, workflow semantics,
public protocol shape, or product/operator policy. Those contributions may
change agent behavior, but only through the core's registered seams and commit
rules.

## External Exposure Model

Multi-agent delegation, messaging, scheduled work, and external-agent execution
are externally usable mechanisms, but runtime core exposes them only through
stable mechanism seams. External users and packages must not import runtime loop
internals or mutate runtime state directly.

| External need | Stable exposure | Extension point | Runtime-owned validation |
|---|---|---|---|
| let a model call another Agent | model-visible `agent_run` descriptor from an extension | config publishes the Agent's delegate roster; a first-class child Run executes locally or remotely through the same lifecycle | target is in resolved roster, descriptor fingerprint matches, permission gate passes |
| let agents or external callers send messages | shared target-thread pending append mechanism | internal `send_message` tool/effect or external message adapter; durable input buffer | message id idempotency, target thread binding, pending freeze before runtime consumption |
| let plugins schedule later work | `ScheduledAction` request committed with the run/thread checkpoint | plugin registers action kinds and result adapter; server owns timer/wake | committed request exists, correlation/idempotency key matches, snapshot/fingerprint match |
| let a client execute a tool | wait/resume channel (client-tool await reason) | protocol adapter projects wait/result; tool descriptor may come from per-run client config | pending wait exists, descriptor fingerprint matches, result is not duplicate/expired/mismatched |
| let a service-backed tool execute work | `RawTool` adapter behind resolved descriptors | adapter package owns transport, auth, and result mapping inside its in-process `invoke` | capability satisfies requirements; result returns through tool output and commit path |
| let a product expose public status | projection from committed facts/events and dispatch state | protocol/product adapter owns DTO names and cursors | live stream is not replay truth; public ids map back to neutral ids |

The external API should therefore be a composition of small surfaces:

```text
configuration publication -> selects tools, plugins, delegate rosters, action kinds
plugin registration ------> contributes descriptors, hooks, state keys, actions
run ingress --------------> submits input and live control
message ingress/effect ----> appends pending input for a target thread
executor/backend adapter --> runs a tool or agent work by resolved id
resume endpoint ----------> maps an inbound result to neutral resume command
projection API -----------> reads committed facts/events/dispatch projections
```

External extension packages may change agent behavior only after configuration
selects them for a run and the resolver includes their descriptors or hooks in
`ResolvedExecutionEnv`. A package that is installed but not selected contributes
no behavior. Runtime-required plugins are the exception; they may be added by the
runtime only to preserve core invariants such as stop/context behavior, not to
add product-specific behavior.

### Multi-Agent Extension

Multi-agent support is a delegation extension, not a new runtime mode. The first
slice is the first-party `agent_run` tool:

```text
resolved root agent
  -> delegate roster in resolved spec
  -> model-visible `agent_run` descriptor with allowed target metadata
  -> tool gate validates tool id, `agent_id`, permission, and capability
  -> execution chooses local run, remote backend, or durable ingress path
  -> child result returns as normal tool output or committed facts/events
```

The parent and child runs remain ordinary runs. If child execution needs durable
delivery, it goes through `RunIngress`. If it is remote, the remote link must
still preserve activation, live context, stream, and commit semantics. The
delegate roster and descriptor fingerprint make replay able to prove which
agents were visible to the model.

### Message Delivery Extension

Internal multi-agent messaging and external inbound messages share the same
underlying lifecycle. Both enter as target-thread pending input, freeze at a safe
run boundary, and become durable truth only through `ThreadCommit`. The durable
pending queue belongs to dispatch/server and the target thread's message
lifecycle. Runtime sees either committed history from `ThreadResumeSnapshot` or
frozen input inside `RunActivation`:

```text
pending input outside runtime
  -> freeze at a safe run boundary
  -> RunActivation input
  -> runtime step loop
  -> ThreadCommit message write intent
  -> committed message log
```

This keeps live control, durable pending delivery, and committed message truth
separate. `send_message` is the internal multi-agent tool/effect over this
message lifecycle; an external public message endpoint is an anti-corruption
adapter into the same lifecycle. Same-thread delivery may share the current
checkpoint when the same commit source is guaranteed; cross-thread delivery uses
an outbox plus idempotent target pending append.

### Scheduled And Background Extension

The runtime may wait on background-like work, but the mechanism is not a generic
`BackgroundTask` object. Use these names by authority:

| Need | Model it as |
|---|---|
| resume this run when an external answer arrives | `ResumeTicket` with the precise typed `AwaitReason` |
| ask for future runtime work after commit | `ScheduledAction` with correlation/idempotency key |
| deliver or recover queued execution | durable ingress dispatch, lease, and wake state |
| run a process or tool outside runtime | orchestration-layer / backend execution |
| expose public job status | product projection over committed runtime and dispatch facts |

There is no catch-all `AwaitReason::BackgroundTasks`. Approval, delegation,
scheduled action, user input, and external result each keep their own typed
reason and correlation. The durable request must be committed before wake
delivery, and the later result must match the committed correlation, run/thread
binding, snapshot, and descriptor fingerprint.

### Multi-Agent, Message, And Scheduled-Work Flow

These mechanisms can be used together, but they still share one runtime rule:
core provides neutral commit, permission, resume, and dispatch seams; behavior
enters only through selected extensions, adapters, and configuration. All
official model-callable builtin tools live in `awaken-ext-builtin-tools`. Runtime
core may define `Tool`, `RawTool`, descriptors, permission gates, commit staging,
and resume validation, but it must not define concrete builtin tool ids or tool
implementations such as `agent_run`, `send_message`, `bash`, `read`, `write`, or
recovery tools outside tests.

The normal interaction flow is:

```text
configuration publication
  -> resolved run environment selects plugins, delegate roster, action kinds
  -> builtin extension contributes model-visible descriptors when selected
  -> model calls agent_run, send_message, or another selected tool
  -> tool gate validates descriptor fingerprint, permission, capability, target
  -> tool/effect returns StateCommand, child-run request, pending-message append,
     external wait, or ScheduledAction request
  -> runtime stages effects and messages in ThreadCommit
  -> commit makes facts, pending outbox entries, awaiting state, or scheduled
     requests durable
  -> dispatch/server observes committed requests and wakes or resumes through
     RunIngress
  -> runtime validates correlation, idempotency, snapshot, and descriptor
     fingerprint before consuming any resumed result
```

`agent_run` is the multi-agent delegation path. The builtin delegation extension
registers one descriptor with an `agent_id` argument. Resolution hides the tool
when the current agent has no delegate roster. Invocation fails closed if the
target agent is not in the resolved roster or if permission/capability checks do
not pass. After validation, execution may be a local child run or a durable
`RunIngress` submit; the child result returns as a normal tool output or
committed fact. Parent and child runs remain ordinary runs.

`send_message` is the internal multi-agent message path. It uses the same
bottom lifecycle as external inbound messages: append target-thread pending
input idempotently, freeze eligible pending input at a safe boundary, execute a
run activation, then make the message durable through `ThreadCommit`. Same-thread
delivery may share the sender checkpoint only when one commit source is proven.
Cross-thread delivery uses a sender outbox plus idempotent target pending append;
it must not require two-phase commit.

Scheduled/background-like work is the deferred-work path. A selected plugin or
tool may request future work only by staging a `ScheduledAction` with a
correlation/idempotency key, run/thread binding, snapshot, and descriptor
fingerprint. The same commit may also await the run in `ResumeTicket`.
Dispatch/server owns timers, retries, and wake delivery. The
runtime accepts a later result only through ingress/resume and only after it
matches the committed request. Do not add a `BackgroundTask` aggregate or a
background-task tool family; model-visible task tools, if any, belong to the
builtin extension and produce ordinary runtime effects.

There is no `BackgroundTask` recovery capability. Recoverability comes from the
specific committed mechanism: `ScheduledAction` request records, resume tickets
and pending external results, durable pending input, dispatch leases, outboxes,
and committed facts/events. An uncommitted candidate effect is not recoverable.
A committed request can be recovered only by validating its correlation,
idempotency key, run/thread binding, snapshot/catalog fingerprint, deadline, and
current run lifecycle before accepting any result.

For extensions such as `awaken-ext-goal`, cancellation cancels the run's ability
to consume an outstanding asynchronous result; it does not make the runtime own a
background task process. If async goal evaluation used a `ScheduledAction` or an
external wait/resume ticket, the cancel commit records the terminal run state and
marks the outstanding request cancelled or superseded for that run. Dispatch or
backend adapters may attempt best-effort cancellation only when their profile
supports it. A later result with the old correlation is stale and must be
rejected before it can mutate committed facts. Continuing after a terminal
cancel requires a new run or a new goal command with a new correlation identity.

### Scenario Validation

Cross-mechanism behavior needs scenario coverage in the same spirit as
`~/Codes/oversight-next`: a small Given/When/Then matrix mapped to executable
tests, not a separate prose-only specification. Unit tests still cover individual
values and validators; GWT-style scenarios cover boundary interactions,
authority splits, and crash/retry behavior.

[runtime-scenario-validation.md](runtime-scenario-validation.md) owns the
canonical scenario ids, Given/When/Then text, test directory layout, and coverage
rules. This document owns the runtime mechanisms that those scenarios validate.

## Runtime Behavior Role Catalog

This catalog covers stable roles inside the runtime loop. Do not list every
helper. A role belongs here only when it owns phase behavior, state/effect/event
authority, extension activation, or replay-sensitive decisions.

| Name | Kind | Owns | Uses | Must Not Own | Failure Mode | Guardrail/Test |
|---|---|---|---|---|---|---|
| `RunActivation` | aggregate input value | prepared runtime input for one run attempt | resolved spec, thread/run ids, selected backend and tools | HTTP route state, product DTOs, live registry objects | runtime starts from adapter-shaped or mutable input | G2, G3; activation serde/API checks |
| `RuntimeRunContext` | per-attempt live wiring | cancellation, input receiver, stream sink, commit source, pinned resolver scope, persistence mode, and thread context cache for one execution attempt | `RunExecutor`, ingress/runtime execution construction | durable request data, public DTOs, config records, immutable executable snapshot data | replay or durable dispatch depends on process-local handles | G2, G3, G5, G13; activation/context split tests |
| Committed run `RunState` | durable run state authority (`Awaiting \| Ended(EndCause)`) | the single stored terminal authority; derived status/outcome/error projections | `RunRecord`, resume ticket, `EndCause`/`Failure` | a stored status/outcome field, public protocol status, live control handles, thread serialization | a second stored notion of the end drifts, or adapter status becomes runtime truth | G1, G10, G31; terminal projection tests |
| `RunRecord` | durable run projection | run identity, input/output ranges, the `RunState` authority, timing, token counters, run-scoped state | activation snapshot, message ranges, persisted state | thread message-log ownership, thread-scoped state authority, product session state | resume reads a run projection that cannot explain the committed thread state | G1, G13; run persist/resume tests |
| `ResumeTicket` | durable waiting payload | structured reason, resume tickets, dispatch marker, wait message | tool-call suspension, ingress resume, recovery wake | terminal outcome, product pause labels, ad hoc status strings | awaiting run cannot be safely resumed or recovered | G5, G9, G13; await reason and ticket validation tests |
| `ThreadCommit` | atomic thread checkpoint plan | message write intent, append fence, latest run projection, optional thread-state snapshot | commit coordinator, persisted state exports, message delta | whole-log rewrite, unguarded message append, product outbox payloads | duplicate/reordered messages or split run/thread truth | G1, G13; append-fence and checkpoint atomicity tests |
| `ThreadResumeSnapshot` | consistent resume read model | committed message view, message version, latest run, thread-scoped state | resume store, dispatch recovery, context builder | mutation authority, product session replay, live sink state | resume observes a torn mix of messages, run projection, and state | G1, G13; snapshot consistency tests |
| `RuntimeResumeStore` | runtime read port | narrow resume reads needed by runtime execution | durable thread/run storage, committed message view | full CRUD/query surface, commit writes, product projections | runtime depends on server store internals or reconstructs state with torn reads | G1, G13; port-boundary and resume tests |
| `PhaseHook` | plugin extension hook | observe or request state changes at defined runtime phases | `StateCommand`, committed runtime context | tool authorization, direct commit writes, product labels | hook mutates state outside staged commands | G1, G8, G14; phase hook tests |
| `StateKey` | typed state key | state identity, scope, merge/conflict policy | state store and effect application | product resource mutation outside approved ports | accidental last-write-wins or cross-scope leak | G1, G8; state merge tests |
| `StateCommand` | runtime command envelope | state patch, scheduled actions, and ordinary effects requested by hooks or tools | phase logic, plugin hooks, tool output | durable store write, public projection, direct handler execution | unvalidated hook/tool output becomes truth or executes before commit | G1; staged command validation tests |
| `MutationBatch` | atomic state patch | ordered registered-key mutations plus optional base revision | `StateKey`, `StateStore`, merge policy | scheduled action queueing, effect dispatch, durable projection | partial state commit or revision conflict is hidden | G1, G8; atomic commit and revision tests |
| `StateStore` | live revisioned state projection | materialized typed state for one active run and monotonic revisions | key registry, mutation batches, commit hooks | durable fact log, product resource store, public session state | live state is mistaken for durable truth | G1, G13; persisted-state replay tests |
| `Snapshot` | read-only state view | frozen revision and typed state map supplied to hooks, tools, and handlers | `StateStore`, applied command output | mutation authority, second source of truth | hook/tool mutates through captured state instead of command output | G1; read-only context tests |
| `PersistedState` | durable state export value | registered persistent key values split by run/thread scope | key registry, commit coordinator, resume store | live tool state, unregistered keys, product resource records | resume loses state or imports unknown/corrupt keys silently | G1, G13; export/import and unknown-key tests |
| `EffectHandler` | post-apply effect seam | typed interpretation of ordinary effects after live state apply | effect payloads, resulting snapshot, registered handler table | durable truth, dispatch scheduling, replay guarantee | side effect is treated as committed or survives crash accidentally | G1, G13; effect/replay tests |
| Run/thread fact payloads | committed fact payload/read model | authoritative aggregate truth after commit | staged commands, effects, commit coordinator | product protocol names, live stream delivery, diagnostics | replay or projection has no durable source | G1, G13; fact replay tests |
| `StreamEvent` | live stream value | best-effort live progress output from the active run | phase/tool/plugin output, stream sink | durable truth, protocol replay source, commit ownership | live stream is treated as replayable truth | G10, G13; stream/projection tests |
| `StreamSink` | live stream output port | current connection delivery for `StreamEvent` | runtime execution, caller/server connection | commit authority, protocol mapping, durable replay truth | sink failure mutates runtime truth or is mistaken for durable loss | G1, G10, G13; sink failure tests |
| `EventDraft` | durable event draft role | neutral event candidate staged with related state/facts | phase/tool/plugin output, durable event stager, commit coordinator | direct live delivery, product event naming | durable replay observes an event for a failed commit | G1, G13; durable event staging tests |
| `EventRecord` | committed event record | neutral event record visible only after commit succeeds | event drafts, committed facts, commit coordinator | public protocol status, independent durable truth | projection has no committed event source | G10, G13; event projection tests |
| `DurableEventSink` | event capture adapter | tee live stream output into durable event drafts for commit staging | stream sink, normalizer, durable event stager | commit authority, protocol naming, durable visibility | capture path treats live output as committed before checkpoint | G1, G10, G13; durable capture tests |
| `EventReader` / `EventSubscriber` | durable event read/subscription port | read or subscribe to committed durable event records for downstream projection | committed event records, cursors, replay/projection adapter | live stream delivery, commit authority, protocol naming | downstream observes an event before its commit | G1, G10, G13; durable event subscription tests |
| `ContinuationGuard` | replay-sensitive decision hook | decide whether natural-end execution continues, waits, or concludes | committed context, selected config, recorded verdicts | product outcome semantics, re-grading during replay | replay diverges from original continuation decision | G11; continuation replay tests |
| `ScheduledAction` | committed deferred-work request | typed request for future runtime work, validated and committed before any durable wake; carries a correlation/idempotency key, run/thread binding, and descriptor fingerprint | hook/tool output, `StateCommand`, `ThreadCommit`, durable ingress; contributed by a runtime extension `Plugin` (first-party bundle: `awaken-ext-builtin-tools`) | timer durability, direct process spawn, product job status, public DTO | background work becomes untracked truth, or a result resumes the wrong run | G1, G5, G13; committed-request, correlation, and wake/idempotency tests |
| `ContextCompaction` | context projection role | append-only summary facts with lineage | committed messages/facts, selected policy | deleting source facts, product UI summary state | replay or audit cannot reconstruct context | G1, G13; compaction lineage tests |

## Cancellation And Stop Policies

Cancellation and stop behavior are runtime commands with typed terminal reasons.

| Case | Rule |
|---|---|
| Client cancel | Enter through ingress, mark runtime intent, commit terminal result |
| Tool timeout | Convert to typed tool/backend failure or `Indeterminate` |
| Stop policy hit | Stop at a safe step boundary and commit the reason |
| Max attempts / no progress | Record typed diagnosis and expose read-only projection |
| User or operator pause | Commit `RunDisposition::Awaiting(ResumeTicket)`, then resume through ingress |

Stop policies are configuration and runtime logic. Public error strings, HTTP
status codes, and UI copy are adapter concerns.

## Scheduled And Background Work

Scheduled actions, reminders, and deferred work are runtime extension requests
with server-owned durability. The runtime-facing mechanism is `ScheduledAction`,
contributed by a runtime extension `Plugin` (the first-party
bundle lives in `awaken-ext-builtin-tools`). It is not a thread, task runner,
delivery lease, product job, or background process:

```text
hook/tool output
  -> StateCommand { scheduled action request }
  -> runtime validates action kind, descriptor fingerprint, and idempotency key
  -> ThreadCommit stages the request and any ResumeTicket atomically
  -> CommitCoordinator commits; work is visible only after commit succeeds
  -> durable ingress observes the committed request and schedules the wake
  -> server resumes runtime through RunIngress / LiveRunControl
  -> runtime validates correlation, run/thread/snapshot, and fingerprint
  -> runtime consumes the result at a safe boundary and commits the outcome
```

The request identity is runtime-domain data: a stable correlation/idempotency
key generated before commit (deduplicates retry, recovery, and duplicate wakes),
a `run_id`/`thread_id` binding the work to one aggregate boundary, the snapshot
id and catalog fingerprint proving the result belongs to the executable
configuration that requested it, and a deadline/expiry that lets runtime reject
stale resumes without using product status names.

Ownership split: the runtime defines the action payload, commits the request, and
validates resume semantics; dispatch/server owns durable timers, wake delivery, retries, and duplicate wake reconciliation (the latter
read from the committed request, never an uncommitted one). A tool may start
deferred work only by returning a typed scheduled effect; it must not spawn an
untracked durable side process. Direct ingress may support only immediate
in-process execution and must return a typed unsupported error for durable
deferred behavior; durable ingress may buffer, recover after a crash, and
reconcile duplicate wakes from committed state.

## Context And Compaction

Context building is runtime behavior over committed state and selected
configuration. Compaction is an append-only transformation:

- source messages/facts remain authoritative;
- compacted summaries are committed with lineage;
- replay can choose the same committed summary instead of re-summarizing;
- context window optimization is policy over the committed context graph.

Prompt hot-tuning and behavior configuration are config/admin/product operations
that publish selected config; runtime consumes the pinned result.

## Observability, Datasets, And Eval

Runtime may emit diagnostic events and trace spans, but analytics are projections.
Trace persistence, dataset capture, eval runs, experiment routing, and admin
review consume committed runtime facts and event streams. They must not become
the authoritative run state.

Eval execution can call the runtime through the same ports as normal execution.
Mock providers, judges, and dataset scripts are test/eval adapters, not special
runtime modes.

## First Vertical Slices

| Requirement | First slice |
|---|---|
| New state/effect feature | key + scope + merge policy + command patch + commit staging + persisted split + snapshot rebuild test |
| New plugin | manifest + capability bound + resolve to one phase hook + bound-check + no-bypass test |
| State-machine workflow | state key + action + effect + guard + replay test |
| Cancellation/stop policy | ingress command + typed terminal reason + projection test |
| Scheduled action/reminder/deferred work | `ScheduledAction` effect + committed request + durable wake + idempotent resume test |
| Context compaction | committed summary fact + lineage + replay reuse test |
| Runtime event | live event + durable draft staging + commit ordering + sink failure or replay test |
| Observability/eval | trace projection or dataset capture from committed facts |

## Non-Goals

- No public protocol status names in runtime phases.
- No plugin hook that bypasses capability, permission, or commit rules.
- No background process as durable truth.
- No `ScheduledAction` result accepted without a committed request and matching correlation key.
- No eval or trace store as authoritative runtime state.
- No live pre-commit runtime event treated as durable or public replay truth.
- No `StreamSink` or durable event subscription failure that mutates committed runtime facts.
- No hidden last-write-wins state merge for parallel tools.
- No live `StateStore` treated as durable truth.
- No ordinary effect handler used as a durable replay or recovery guarantee.
- No shared/product resource state hidden inside runtime extension state.

## Guardrails

G1, G2, G8, G9, G10, G11, G13, and G14 in [INVARIANTS](../INVARIANTS.md).
