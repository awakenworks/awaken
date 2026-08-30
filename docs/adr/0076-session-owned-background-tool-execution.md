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
of ordinary tool calls.

The model-facing management surface is closed and uniform:
run_in_background, list_background_tasks, get_background_task, and
cancel_background_task. The wrapper schema is generated from its typed Rust
argument shape and then narrowed with the configured canonical tool-id enum.
MCP and A2A require no special wrapper or task store.

### D2: the extension owns the aggregate; Runtime owns only generic State

`awaken-ext-background-task` owns the typed aggregate, transition algebra,
configuration, and four model-facing tools. A tool receives the immutable State
view already captured at executor entry and returns ordinary `StateCommand`
values on `ToolOutput`. Runtime applies and commits those commands with the same
ToolBatch and ThreadCommit path used by every stateful tool.

Runtime has no BackgroundTask enum, operation classifier, service, repository,
lease, worker, polling, or database concept. The only generic seam added to the
contract is read-only tool access to the current materialized State; it cannot
write or reach persistence. This dependency direction is checked by crate
fitness rules.

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

MCP and A2A native task support is an optional executor enhancement, not another
lifecycle. A remote wait carries the closed protocol kind, stable server
binding, and non-empty remote task id together. Reclaim transfers authority by
a strictly increasing epoch, so a stale executor cannot publish.

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
`awaken-runtime-contract`; the reverse dependency is forbidden. The only new
contract operation is the generic read-only State context available to a
stateful dynamic `RawTool`. Runtime projects that view through the owning
manifest state-key bound and rejects any returned command outside the same
bound. An MCP or other plugin with deny-all State authority therefore receives
an empty view and cannot smuggle a write. The view is held behind an `Arc`, so
repeated reads clone only the shared handle rather than the complete map.

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
- lease epochs and revisions strictly increase or fail before mutation;
- cancellation and terminal states never reopen;
- a committed cancellation absorbs a racing success;
- malformed persisted JSON never resets or disappears as an empty task set;
- management results never expose invocation arguments;
- Runtime and runtime-contract contain no BackgroundTask service or repository.

Cause-graph and decision-table tests cover typed schema generation, configuration
partitions, deterministic replay, Runtime ThreadCommit integration, list/get/
cancel behavior, shape drift, claim competition, lease boundaries, stale fences,
remote waits, cancellation races, indeterminate recovery, overflow atomicity,
and the absence of persistence dependencies. Kani proves cancellation
monotonicity and terminal absorption. Architecture and feature-ledger checkers
make the dependency and evidence claims executable.

## Product composition

`SharedHost` installs the extension in the canonical plugin catalog and mounts
one post-commit observer when a Native Agent selects a non-empty
`background_task.tools` configuration. The observer reads only committed Thread
State, uses the same prepared Runtime catalog/gates/placement as foreground
tools, suppresses duplicate terminal delivery through the process supervisor,
honors the exact persisted concurrency/resource claim, and retains the exact
Session Environment generation through `BackgroundRuns`.

The supervisor holds one mutually exclusive process slot per task: `Active`
atomically becomes `Completed` under one lock. It does not synchronize parallel
active/completed maps. A completion whose fence no longer matches durable Thread
State is retired before the current attempt is reconciled, so stale local
evidence cannot suppress a newer post-commit launch.

Completion is intentionally not a post-terminal database write. It remains a
fenced process-local projection until the extension's `StepStart` hook folds it
into the next ordinary Run commit. A process loss therefore leaves the durable
Running lease authoritative: the next Run either reclaims a replayable attempt
with a higher epoch or ends a non-replayable attempt as indeterminate. A remote
Worker uses the same mechanism against its claim-updated recovery projection;
it owns no SQL connection and sends no plugin-specific payload to Coordinator.

ACP backends fail Session construction when this plugin is selected. Their tool
execution authority lives in the external ACP process and cannot yet provide the
identity-bound prepared executor required by this decision; failing before tool
advertisement prevents a recorded-but-never-executed feature. Native MCP tools
are ordinary dynamic Runtime tools and need no special background adapter.

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

## Consequences

- The capability remains one explicit bounded context and does not weaken the
  ADR-0003 ban on a generic asynchronous-work umbrella.
- Runtime remains storage-, worker-, and lease-neutral.
- SQLite and Postgres differences stay behind existing Thread persistence.
- Ordinary tools require no background adapter; MCP/A2A task APIs remain
  optional executor optimizations.
- Removing the task-specific table/service avoids a second Session truth and a
  second backend conformance matrix.
