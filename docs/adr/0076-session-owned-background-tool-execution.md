# ADR-0076: Session-Owned Background Tool Execution

- Status: Accepted
- Date: 2026-08-25
- Amends: ADR-0003 D1/D3
- Preserves: ADR-0007 runtime-owned tool admission, ADR-0030 permission policy,
  ADR-0055 typed state, ADR-0059 neutral-core boundaries

## Context

An agent sometimes needs to start an already-registered tool, end the current
Run step immediately, and observe or cancel the operation from a later Run in
the same Session. This is not a delayed phase of the same Run, a suspended Run,
a child agent Run, or Environment placement. Those existing mechanisms cannot
provide a Session-owned task identifier without changing their aggregate
meaning.

The capability must work for native, MCP, A2A, shell, and future tools without a
parallel registry or a per-tool background adapter. MCP/A2A native task APIs may
improve status and cancellation, but cannot create a second lifecycle.

## Decision

### D1: one narrow aggregate, not a deferred-work umbrella

BackgroundTask means exactly “a detached invocation of one canonical ordinary
tool”. It cannot contain ScheduledAction, ResumeTicket, RunDispatch, Session
Work, delegation, or an arbitrary application job.

External configuration is deliberately binary: an ordinary canonical tool id
is either absent (foreground only) or listed as eligible for
`run_in_background`. An empty configuration contributes no tools or state
namespace. There is no second direct-call syntax and no implicit interception
of ordinary tool calls. A listed target remains in the one executable catalog
but is removed from eager, deferred-search, and forged direct-call surfaces;
the generated `run_in_background` schema is its only model-visible invocation
path. Listing every target in a publication therefore gives the Agent exactly
the four BackgroundTask tools without creating a filtered executor registry.

The model-facing management surface is closed and uniform:
run_in_background, list_background_tasks, get_background_task, and
cancel_background_task. The wrapper schema is generated from its typed Rust
argument shape and then narrowed with the configured canonical tool-id enum.
MCP Tasks require no special wrapper or task store. A remote A2A Agent remains
an ordinary Run/attempt backend with its own relationship and Run lifecycle; it
is not converted into a BackgroundTask merely because the wire calls it a
task.

### D2: the extension owns the aggregate; Runtime owns only generic State

`awaken-ext-background-task` owns the typed aggregate, transition algebra,
configuration, and four model-facing tools. A tool receives the immutable State
view already captured at executor entry and returns ordinary `StateCommand`
values on `ToolOutput`. Runtime applies and commits those commands with the same
ToolBatch and ThreadCommit path used by every stateful tool.

Runtime has no BackgroundTask enum, operation classifier, service, repository,
lease, worker, polling policy, or database concept. Runtime exposes only
protocol-neutral tool continuation and execution-admission seams alongside
read-only access to the current materialized State; none can write or reach
persistence. This dependency direction is checked by crate fitness rules.

The deterministic task id is derived from trusted Thread, Run, and Runtime
operation identity, never from model arguments. Replaying the same committed
operation returns the same receipt and emits no duplicate state command. List,
get, and cancel scan only the declared Thread-state namespace.

### D3: execution is an outward commit reaction

The state command creates an already-fenced `Running` aggregate atomically with
the foreground tool result. Runtime's immutable execution-facts context resolves
the target's publication-pinned recovery policy, executable capability,
concurrency/resource claims, and placement before that command is staged; a
model cannot author or widen them. Only after that ThreadCommit is durable does
the product terminal observer schedule execution through the existing Runtime
tool confluence and Host/Worker placement boundary. Runtime never calls the
driver and the plugin never imports an application service.

The aggregate carries owner plus epoch fencing. `Requested` is an internal
pre-claim construction state and is never committed by `run_in_background`:
Runtime resolves the pinned ordinary tool and `start` freezes those trusted
facts into the first attempt before the foreground commit;
`reclaim` can only reuse that persisted policy and enforces its attempt budget.
Start, reclaim, heartbeat, wait, cancel, and finish are atomic transitions: an
error leaves the value unchanged. Expired non-replayable ownership returns the explicit
`TaskClaim::EndedIndeterminate` decision, rather than mutating and returning an
error that a caller might discard. Cancellation wins over a racing completion,
and terminal states are absorbing.

MCP Tasks and any future durable ordinary-tool protocol are optional executor
enhancements, not another lifecycle. Generic Runtime task ports start, poll,
and cancel one opaque `ToolTaskHandle`; the adapter owns capability negotiation
and protocol mapping. BackgroundTask persists that exact protocol-neutral
handle rather than copying its fields into an extension-owned continuation.
The handle carries an opaque non-empty adapter owner, stable binding, non-empty
task id, and an optional positive poll interval together. These coordinates
never enter the Agent projection. Adding another durable-tool adapter therefore
does not change the BackgroundTask aggregate or Runtime Host.

Standard MCP task creation does not guarantee client-stable idempotency. The
initial `tools/call` therefore stays `NeverReplay`: loss before the returned
task id enters ThreadCommit becomes `Indeterminate`, never a second start. Once
the exact handle is committed as `Waiting` or `Cancelling`, reclaim reconnects
to that task id without replaying creation, retains the lifecycle phase, and
transfers authority by a strictly increasing epoch. A stale executor cannot
poll, cancel, heartbeat, or publish completion.

### D4: existing Thread persistence is the sole durable authority

There is no `background_task_item` table, BackgroundTask repository/service, or
SQLite/Postgres adapter. Backend portability comes from the existing
ThreadCommit persistence contract: both databases already commit and replay the
same typed state commands under the same Thread revision boundary. A plugin
therefore cannot introduce schema migrations or observe backend-specific SQL.

Each task occupies one exclusive Thread-state cell under
`background_task/{task_id}`. The typed deserializer checks aggregate invariants;
unknown fields, empty identities, impossible
revision/lifecycle pairs, invalid attempts, and incomplete remote continuations
fail closed. Management views expose status and identity but never invocation
arguments.

Concurrency and resource admission remain properties of the eventual ordinary
tool invocation and the outward driver. They must reuse the canonical
`ToolConcurrency` algebra rather than add background-specific parallel, serial,
or resource-lock rules.

### D5: contract changes stay generic and minimal

The stable runtime contract contains no BackgroundTask types, management ids,
repository ports, or extension configuration. The extension depends inward on
`awaken-runtime-contract`; the reverse dependency is forbidden. Its generic
seams are read-only State context for a dynamic `RawTool`, protocol-neutral
task start/poll/cancel values, descriptor relations that hide configured
targets, and a host-owned tool-execution admission port. None knows task ids,
Session attention, MCP methods, or persistence. Runtime projects State through
the owning manifest bound and rejects returned commands outside it. An MCP or
other plugin with deny-all State authority therefore receives an empty view and
cannot smuggle a write. The view is held behind an `Arc`, so repeated reads
clone only the shared handle rather than the complete map.

`RuntimeToolOperation` contains only operations intrinsically implemented by
Runtime. Background management ids are extension-owned `RawTool` identities and
flow through the ordinary dynamic-tool registry. Metadata-derived crate fitness
checks and explicit forbidden-symbol checks prevent the old dependency from
returning.

## Invariants and verification

- one `(Thread, Run, operation_id)` produces one deterministic task identity;
- task creation and its model-visible receipt enter one ordinary ThreadCommit;
- an unconfigured plugin advertises no background tools;
- tools can read an immutable State snapshot but can only stage commands;
- only the current owner plus epoch may heartbeat, wait, or finish;
- every failed transition leaves the aggregate unchanged;
- a remote wait always carries complete reattachment coordinates;
- the aggregate persists Runtime's one opaque `ToolTaskHandle` and contains no
  MCP/A2A protocol enum or copied continuation DTO;
- lease epochs and revisions strictly increase or fail before mutation;
- cancellation and terminal states never reopen;
- a committed cancellation absorbs a racing success;
- malformed persisted JSON never resets or disappears as an empty task set;
- management results never expose invocation arguments, remote handles, lease
  coordinates, worker identity, recovery policy, or poll intervals;
- completion attention uses one fence-derived idempotent Session command and
  carries no result, progress, invocation, credential, or remote-id payload;
- repeated `Working` observations are silent; a changed `InputRequired`, lease
  checkpoint, durable-wait transition, and terminal candidate use distinct
  deterministic attention identities;
- only an explicit remote `Failed` or `Cancelled` status creates that terminal
  result; observation or attempt exhaustion becomes `Indeterminate`;
- `StepStart` reconciliation precedes the attention Run's inference;
- configured targets are absent from model presentation and direct dispatch but
  remain executable through the canonical prepared executor;
- foreground and detached calls in one Session Environment generation share the
  canonical `ToolConcurrency` admission;
- Runtime and runtime-contract contain no BackgroundTask service or repository.

Cause-graph and decision-table tests cover typed schema generation, configuration
partitions, deterministic replay, Runtime ThreadCommit integration, list/get/
cancel behavior, shape drift, claim competition, lease boundaries, stale fences,
remote waits, cancellation races, indeterminate recovery, overflow atomicity,
completion publication retry, same-Session attention, pre-inference folding,
fail-closed remote placement, committed-handle reconnect, start-response loss,
poll/cancel exhaustion, status-change attention, shared foreground/background
admission, hidden target projection, and the absence of persistence
dependencies. Kani proves cancellation monotonicity, terminal absorption, and
the durable-continuation exception to NeverReplay reclaim. Architecture and
feature-ledger checkers make the dependency and evidence claims executable.

## Product composition

`SharedHost` installs the extension in the canonical plugin catalog and mounts
one post-commit observer when a Native Agent selects a non-empty
`background_task.tools` configuration. The observer reads only committed Thread
State, uses the same prepared Runtime catalog/gates/placement as foreground
tools, suppresses duplicate terminal delivery through the process supervisor,
honors the exact persisted concurrency/resource claim, and retains the exact
Session Environment generation through `BackgroundRuns`.

The supervisor holds one mutually exclusive process slot per task: `Active`,
`Waiting(candidate)`, or `Completed`. Transitions occur under one lock; there
are no parallel active/waiting/completed maps. A terminal candidate dominates a
wait candidate for the same fence. A candidate whose fence no longer matches
durable Thread State is retired before the current attempt is reconciled, so
stale local evidence cannot suppress a newer post-commit launch.

Completion is intentionally not a post-terminal database write. It remains a
fenced process-local projection until the extension's `StepStart` hook folds it
into the next ordinary Run commit. A process loss therefore leaves the durable
Running lease authoritative: the next Run either reclaims a replayable attempt
with a higher epoch or ends a non-replayable attempt as indeterminate.

Durable-wait, changed input-required, watchdog, and terminal candidates publish
deterministic **attention Runs** through the existing
`SessionRunBackgroundApplication`. They change no task truth: each is an
ordinary same-Thread Session Run admitted through the canonical reservation,
Session activity receipt, and dispatch path. `SharedHost` retains only a weak
composition edge to that application, so the wiring neither creates an
ownership cycle nor becomes a second admission owner. Operation and Message
identities are derived from `(Thread, task id, worker, epoch, candidate kind or
monotone change)`. An ambiguous application failure retries the exact command
with short bounded backoff; `BadRequest` is definitive. Session admission
idempotency therefore covers the response-loss window without a notification
table or retry ledger.

The attention input is `Role::System` and contains only the task id, the abstract
reason for waking, and an instruction to call `get_background_task`. It never
embeds invocation arguments, credentials, progress, remote ids, or result
content. `StepStart` remains the lifecycle owner and runs before inference, so a
matching wait, heartbeat, or completion is first folded into the current Run
view and the Agent then reads that authoritative projection. The fold becomes
durable at the next ordinary ThreadCommit: before any ensuing tool effect, or at
the terminal boundary for a text-only response. `BeforeInference` is not used:
it would turn task reconciliation into request-only context decoration and
require an unrelated ContextMessages capability. A stale candidate may still
cause a harmless reminder, but its fence can never alter a newer attempt. The
Agent decides whether to incorporate a result, cancel related work, or continue;
it does not delete lifecycle truth.

`Working` is polled silently at the negotiated interval. A changed
`InputRequired` wakes the Agent once per stable message fingerprint. A watchdog
wakes at half the current execution lease so `StepStart` can commit a fenced
heartbeat and launch the next observation interval; it is recovery liveness,
not a user timeout and not an Agent-visible `watch(task_id)` tool. Explicit
remote completion fetches the result before producing a terminal candidate.
Explicit remote failure/cancellation maps to the matching aggregate terminal;
transport ambiguity, invalid committed coordinates, or exhausted observation
budget maps to `Indeterminate`.

The attention Run is the only direct Session effect. The detached tool itself
does not mutate the Session aggregate. While attention executes, the existing
Session activity makes the Session `Running`; normal settlement returns it to
`Idle` or its ordinary terminal state. Child-Agent reports keep their distinct
`SessionAgentCoordination` and Outbox/Inbox path because they cross Thread
ownership and carry relationship provenance; BackgroundTask completion is a
same-Thread state wake and does not reuse Managed coordination message ingress.

Foreground and detached tool effects for the same `(Session, Environment
generation)` acquire one host-owned execution admission at Runtime's canonical
executor boundary. It consumes the existing `ToolConcurrency` resource algebra
and rechecks the Run/attempt fence after waiting. Different generations remain
isolated. The admission is a process guard only: it owns no task status, queue,
lease, or durable lock record.

A database-less Worker currently fails closed before Environment, MCP, tool, or
commit realization when BackgroundTask is selected. Its completion projection
is process-local, and there is not yet a claim-fenced Worker-to-Coordinator
attention command that guarantees the follow-up Run returns to that process.
Pretending to support that placement could strand completion or present an
unreconciled reminder. Distributed enablement requires extending the existing
claimed Session control transport, not routing through the generic cross-Thread
message service or adding a BackgroundTask repository.

ACP backends fail Session construction when this plugin is selected. Their tool
execution authority lives in the external ACP process and cannot yet provide the
identity-bound prepared executor required by this decision; failing before tool
advertisement prevents a recorded-but-never-executed feature. Native MCP tools
are ordinary dynamic Runtime tools and need no special background adapter.

A2A Tasks remain owned by `A2aRunExecutor` and the committed Run/attempt
lifecycle. Multi-Agent delivery remains owned by `SessionAgentCoordination` and
its relationship-aware Outbox/Inbox path. Neither is routed through
BackgroundTask; the three bounded contexts meet only at the existing RunIngress,
ThreadCommit, Session activity, and execution-admission boundaries.

Tools with a Runtime State capability currently fail the pre-claim target
resolution for detached execution. Their commands may only be applied by their
own plugin capability on an ordinary Runtime commit; silently re-attributing
those commands to `background_task` would violate D5. Stateless Native, sandbox,
and MCP tools require no implementation change.

Detached invocation deliberately does not fire the target tool's `AfterTool`
hooks. The foreground `run_in_background` call is the state-machine event; firing
the wrapped tool as a second event would advance workflow state outside the Run
commit that owns that state. The prepared executor still reuses the canonical
catalog, authorization, placement, recovery, concurrency, cancellation, panic
isolation, and output-spill paths.

There is no BackgroundTask TTL, deletion command, or progress stream in this
decision. Terminal values remain ordinary Thread State and follow the Thread's
existing retention/erasure policy. The execution lease is not a user timeout:
it fences ownership and enables recovery. Tool-specific deadlines remain part
of the canonical ordinary tool. A future terminal-compaction policy may replace
old terminal payloads with tombstones at a Thread-owned commit boundary, but it
must not let the Agent erase active tasks, add a task-local GC clock, or create
a parallel result store. Progress should be added only when a canonical tool
can expose monotone, bounded, non-secret progress with a stable cursor; it is not
required for the completion decision because the Agent reads terminal state.

## Consequences

- The capability remains one explicit bounded context and does not weaken the
  ADR-0003 ban on a generic asynchronous-work umbrella.
- Runtime remains storage-, worker-, and lease-neutral.
- SQLite and Postgres differences stay behind existing Thread persistence.
- Ordinary tools require no background adapter; MCP/A2A task APIs remain
  optional executor optimizations.
- Removing the task-specific table/service avoids a second Session truth and a
  second backend conformance matrix.
