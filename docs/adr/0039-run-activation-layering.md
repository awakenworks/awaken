# ADR-0039: `RunActivation` — Replacing the `RunRequest` God-Struct

- **Status**: Accepted
- **Date**: 2026-05-22
- **Depends on**: ADR-0011, ADR-0038
- **Updates**: ADR-0011 D3/D6, ADR-0022 D3
- **Breaking**: yes (0.6.0)

## Context

`RunRequest` has accumulated 20+ optional public fields that mix four
concerns:

| Concern | Example fields |
|---|---|
| User intent | `messages`, `agent_id`, `thread_id`, `overrides`, `frontend_tools` |
| Dispatch / transport | `origin`, `adapter`, `dispatch_id`, `session_id`, `transport_request_id` |
| Run identity hints | `continue_run_id`, `run_id_hint`, `dispatch_id_hint` |
| Runtime wiring | `pending_boundary`, `commit_coordinator_override`, `pinned_registry_set`, `run_resolver` |

`RunRequestSnapshot` lives in the contract layer, while executable
`RunRequest` lives in `awaken-runtime`. Runtime-only handles leak into a
public request object, and the persisted projection is defined by fields
that were omitted rather than by fields that are safe to replay.

## Decision

Replace `RunRequest` with an owned `RunActivation` split into
contract-layer data and runtime-layer execution resources. The activation
is owned so it can cross `.await`, be queued by mailbox dispatch, and move
between tasks without borrowing stack-local context.

### D1: Contract-layer types

In `awaken-runtime-contract::contract::run`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunIntent {
    pub agent_id: Option<String>,
    pub thread_id: String,
    pub kind: RunKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RunKind {
    /// Start a new run for a new user or system intent.
    NewIntent,
    /// Resume the same durable run that is waiting for human/tool input.
    HitlResume { run_id: String },
    /// Start a fresh activation using a previous run as continuation context.
    ContinuationFromRun { run_id: String },
}
```

The names are intentionally explicit. `HitlResume` is the same durable run
being re-dispatched after an interrupt or decision. `ContinuationFromRun`
creates a new activation whose prompt/state is derived from an earlier run;
it is not the HITL resume path.

Runtime input may contain message bodies, but persisted input never does:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RunInput {
    NewMessages(Vec<Message>),
    AlreadyPersisted(RunInputSnapshot),
}

/// Durable projection of the thread message slice consumed by a run.
/// Shape intentionally mirrors existing RunMessageInput.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunInputSnapshot {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<MessageSeqRange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trigger_message_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selected_message_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacted_snapshot_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunOptions {
    pub overrides: Option<InferenceOverride>,
    pub frontend_tools: Vec<ToolDescriptor>,
}
```

`MessageSeqRange` is the existing thread-log watermark. ADR-0039 does not
introduce a new `MessageWatermark` type.

Runtime resolution scope lives in `awaken-runtime` and names server-owned
resolved registry snapshots by opaque id:

```rust
pub enum RegistryResolutionScope {
    Live,
    Pinned(String),
}
```

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunTraceContext {
    pub parent_run_id: Option<String>,
    pub parent_thread_id: Option<String>,
    pub origin: RunOrigin,
    pub adapter: AdapterKind,
    pub run_mode: RunMode,
    pub dispatch_id: Option<String>,
    pub session_id: Option<String>,
    pub transport_request_id: Option<String>,
    pub correlation_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunActivationSnapshot {
    pub intent: RunIntent,
    pub input: RunInputSnapshot,
    pub options: RunOptions,
    pub trace: RunTraceContext,
    pub seeded_decisions: Vec<(String, ToolCallResume)>,
    pub resolution_id: Option<String>,
}
```

A persisted snapshot is replay-safe by construction: its
`resolution_id` is an opaque reference to the server-owned resolved registry
snapshot. `awaken-server-contract` re-exports these runtime-facing types for
server/store crates. `awaken-contract` is only the compatibility re-export
facade for older import paths.

### D2: Runtime-layer type is owned

In `awaken-runtime::run`:

```rust
pub struct RunActivation {
    pub intent: RunIntent,
    pub input: RunInput,
    pub options: RunOptions,
    pub trace: RunTraceContext,
    pub control: RunControl,
    /// Thread-context fast-path. Orthogonal to user intent and trace metadata.
    pub capture: CaptureWiring,
    /// Submit-side persistence facts the runtime must honour for
    /// idempotency and id stability.
    pub persistence: PersistenceHints,
    /// Pinned resolver objects inherited from the parent for sub-run
    /// scope continuity.
    pub inherited: ResolverInheritance,
}

pub struct RunControl {
    pub cancellation_token: Option<CancellationToken>,
    pub decision_rx: Option<mpsc::UnboundedReceiver<Vec<(String, ToolCallResume)>>>,
    /// Owned inbox pair. The activation must hold the `sender` half so
    /// background tasks and extensions can deliver messages into this
    /// run; a bare `InboxReceiver` is not enough.
    pub inbox: Option<RunInbox>,
    pub pending_boundary: Option<Arc<dyn PendingBoundaryHandler>>,
    pub seeded_decisions: Vec<(String, ToolCallResume)>,
    /// Optional per-run commit coordinator override. Server dispatch supplies a
    /// staging coordinator that folds canonical event drafts into checkpoint
    /// commits, so the runtime never observes the event buffer.
    pub commit_coordinator_override: Option<Arc<dyn CommitCoordinator>>,
}

pub struct RunInbox {
    pub sender: InboxSender,
    pub receiver: InboxReceiver,
}

pub struct CaptureWiring {
    /// Optional caller-side fast path; absent means the runtime loads
    /// thread context from the store.
    pub thread_context_cache: Option<Arc<ThreadContextSnapshot>>,
}

pub struct PersistenceHints {
    /// True when the activation continues a prior run (resume / handoff).
    pub is_continuation: bool,
    /// Set by submit paths that have already appended new messages to the
    /// thread log; prevents the runtime from re-persisting `NewMessages`.
    pub messages_already_persisted: bool,
    /// Mailbox-allocated identifiers that the runtime must adopt instead
    /// of minting new ones (preserves the dispatch ↔ run ↔ event chain).
    pub run_id_hint: Option<String>,
    pub dispatch_id_hint: Option<String>,
}

pub struct ResolverInheritance {
    /// Frozen resolver objects inherited from a pinned root run. Sub-runs
    /// use this to resolve against the same registry the parent ran under,
    /// independent of the live registry snapshot.
    pub pinned_registry_set: Option<RegistrySet>,
    pub run_resolver: Option<Arc<dyn Resolver>>,
}
```

No lifetime parameter appears on `RunActivation`. The mailbox can enqueue
and later dispatch the activation without extending stack borrows.
`ThreadContextSnapshot` is either absent or owned through `Arc`.

Canonical runtime capture is not represented in `CaptureWiring`. ADR-0038
wires capture by wrapping the live sink with `DurableEventSink` and installing
a per-run `StagingCommitCoordinator` through
`control.commit_coordinator_override`.

The three split structs each represent a distinct boundary participation
— **runtime control**, **persistence idempotency / id injection**, and
**sub-run resolver inheritance** — instead of a single
`RunExecutionWiring` god-struct with seven mixed-intent fields. Each
consumer can take the sub-struct it cares about (runtime control sees
`RunControl`; mailbox submit sees `PersistenceHints`; nested resolution sees
`ResolverInheritance`) so the type signature reveals which boundary is
participating.

### D3: Snapshot creation happens after message persistence

Snapshot construction does not infer durable input references from message
bodies and does not mutate runtime resolution scope. The caller must first
append `RunInput::NewMessages` to the thread message log, resolve a replayable
run plan through ADR-0040, and pass both durable results explicitly:

```rust
pub fn run_activation_snapshot(
    activation: &RunActivation,
    persisted_input: RunInputSnapshot,
    resolution_id: Option<String>,
) -> RunActivationSnapshot {
    RunActivationSnapshot {
        intent: activation.intent.clone(),
        input: persisted_input,
        options: activation.options.clone(),
        trace: activation.trace.clone(),
        seeded_decisions: activation.control.seeded_decisions.clone(),
        resolution_id,
    }
}
```

The snapshot stores the opaque `resolution_id` from
`ReplayableResolvedRun::artifact`. Dispatch later resolves with
`RegistryResolutionScope::Pinned(resolution_id)`.

Mailbox/server submit paths own the order:

1. normalize and append new messages to the thread log;
2. build the `RunInputSnapshot` from the assigned `MessageSeqRange` and
   trigger message ids;
3. resolve `RunActivation` through ADR-0040 with
   `ResolutionPolicy::PersistentServer` and require
   `ResolvedRun<ReplayableScope>`;
4. pass `plan.artifact.resolution_id` into the snapshot;
5. persist `RunActivationSnapshot` and `RunRecord.resolution_id`;
6. discard the live execution handles from this pre-dispatch resolution
   unless the run is executed immediately in the same task.

Embedded non-persistent callers may run with `RegistryResolutionScope::Live`,
but they cannot produce a replay-safe snapshot because they do not have a
`resolution_id`.

### D4: Runtime execution receives a resolved plan

`AgentRuntime` execution takes a resolved plan instead of resolving
implicitly inside `run_*`. A resolved plan contains live handles
(`LlmExecutor`, backend instances, plugin environment) and is not durable
queue data. Durable queues store only `RunActivationSnapshot`, including
the opaque `resolution_id` from D3, never `ResolvedRunPlan`.

Persistent server/mailbox flows therefore have two resolution moments:

1. submit-time resolution pins the registry scope so the run record is
   replay-safe; only `plan.artifact.resolution_id` is persisted;
2. dispatch-time resolution rebuilds live execution handles from
   `RegistryResolutionScope::Pinned(resolution_id)`, and the resulting plan is
   passed to `run_replayable`.

Immediate non-queued execution may reuse the fresh plan from submit-time
resolution if no durable queue boundary is crossed. The execution entry
points take both the owned activation and a resolved plan:

```rust
impl AgentRuntime {
    pub async fn resolve_activation(
        &self,
        activation: &RunActivation,
        policy: ResolutionPolicy,
    ) -> Result<ResolvedRunPlan, ResolveError>;

    pub async fn run_replayable(
        &self,
        activation: RunActivation,
        plan: ReplayableResolvedRun,
        live_sink: Arc<dyn EventSink>,
    ) -> Result<RunOutcome, RuntimeError>;

    pub async fn run_live(
        &self,
        activation: RunActivation,
        plan: ResolvedRunPlan,
        live_sink: Arc<dyn EventSink>,
    ) -> Result<RunOutcome, RuntimeError>;
}
```

Mailbox/server persistent dispatch calls `resolve_activation(...,
ResolutionPolicy::PersistentServer)` against the snapshot's pinned scope and
rejects `ResolvedRunPlan::LiveOnly` before any execution side effect.
Embedded callers may call `run_live` with a live-only plan.

`AgentRuntime` holds an `Arc<dyn Resolver>` because `resolve_activation` is
the same entry used for root dispatch and for sub-run resolution mid-execution
(`ResolutionTarget::Delegate`, `ResolutionTarget::Handoff` — see ADR-0040 D7).
Holding the resolver on the runtime does not weaken the dispatch-entry
contract: persistent paths still reject `LiveOnly` plans at every resolution
moment, root or nested, by the same type boundary.

`live_sink` is the live emission destination. If canonical runtime capture
is enabled, server dispatch wraps `live_sink` in ADR-0038's
`DurableEventSink` before execution and installs `StagingCommitCoordinator`
through `activation.control.commit_coordinator_override`. The runtime does not
receive the event buffer.

### D5: `RunRequestExtras` is deleted

`RunRequestExtras` was a JSON escape hatch for fields that did not fit the
snapshot schema. With typed `RunIntent`, `RunInputSnapshot`, `RunOptions`,
`RunTraceContext`, `seeded_decisions`, and `resolution_id`, every persisted
field has an explicit home. `RunRequestExtras` is removed in 0.6.0.

### D6: `RunRequest` is deleted

0.6.0 is source-breaking. The `RunRequest` god-struct is deleted outright;
no migration wrapper is retained. Callers construct `RunActivation`
directly. Keeping a `#[deprecated]` shell would preserve a parallel request
model in the public surface for no real callers; deleting it is simpler
than maintaining a wrapper that delegates straight into the new boundary.

`RunRequestSnapshot` remains as a legacy serde struct for one release. It
keeps the old wire fields, including `request_extras`, and implements
`TryFrom<RunRequestSnapshot> for RunActivationSnapshot` once the caller has
provided persisted input and a `resolution_id`. It is **not** a type alias
to `RunActivationSnapshot`, because old JSON would not match the new field
shape. New writes use `RunActivationSnapshot`.

## Migration

| 0.5.x | 0.6.0 |
|---|---|
| `RunRequest::new(thread_id, messages).with_agent_id(a).with_dispatch_id(d)` | construct `RunActivation { intent, input, options, trace, control, capture, persistence, inherited }` |
| `continue_run_id` for HITL resume | `RunKind::HitlResume { run_id }` |
| `continue_run_id` for continuation context | `RunKind::ContinuationFromRun { run_id }` |
| `RunRequestExtras` | typed fields on `RunActivationSnapshot` |
| `RunRequestSnapshot` | legacy serde struct + `TryFrom` adapter; new writes use `RunActivationSnapshot` |
| `runtime.run(request, sink)` | `resolve_activation(...)` then `run_replayable(...)` or `run_live(...)` |

## Risks

- Every request construction site changes at 0.6.0. The benefit is that
  runtime-only handles no longer leak into persisted/public request data.
- The `RunKind` rename forces callers to choose HITL resume vs continuation
  explicitly; ambiguous old uses of `continue_run_id` must be inspected.

## Test Plan

1. `RunActivation` can be moved across async tasks without lifetime bounds.
2. Snapshot creation requires caller-supplied `RunInputSnapshot` and
   `resolution_id`; attempting to snapshot raw `NewMessages` into a replayable
   record without a resolved id is impossible through server APIs.
3. Persistent dispatch rejects `ResolvedRunPlan::LiveOnly` before execution.
4. Old `RunRequestSnapshot` fixtures deserialize through the legacy struct
   and convert to `RunActivationSnapshot` only after a `resolution_id` is
   provided.
5. The sink wrapper and `StagingCommitCoordinator` share the same
   `EventBuffer` when runtime capture is enabled.
6. Durable mailbox records persist the `resolution_id` but never serialize or
   cache `ResolvedRunPlan` live handles.

## Non-Goals

- Splitting `RunOutcome`.
- Replacing `MessageSeqRange` or `RunMessageInput`; ADR-0039 reuses the
  existing message-log reference model.
