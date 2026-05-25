# ADR-0038: Runtime Commit Boundary — Single Durable Write Entry

- **Status**: Accepted
- **Date**: 2026-05-22
- **Depends on**: ADR-0034, ADR-0036
- **Updates**: ADR-0034 D7/D9, ADR-0036 D8/D9
- **Breaking**: yes (0.6.0)

## Context

ADR-0036 introduced `CommitCoordinator` as the cross-store atomic write
boundary for `ThreadRunStore + EventStore + OutboxStore`. The rollout
landed the coordinator and event buffer, but left a legacy inline append
branch in `runtime_event_capture`:

```rust
// runtime_event_capture.rs (current)
if let Some(buffer) = event_buffer {
    Arc::new(BufferedDurableEventSink::new(/* coordinator-bound */))
} else {
    Arc::new(DurableEventSink::new(inner, capture.writer, /* legacy inline */))
}
```

That shape has two failure modes:

- A deployment can wire `runtime_event_capture` without a coordinator and
  silently append canonical runtime events outside the matching
  `ThreadRunStore` checkpoint transaction.
- Two sink wrappers (`DurableEventSink`, `BufferedDurableEventSink`) encode
  the same runtime tee concept with different ordering and error behavior.

`runtime_event_capture` also writes mailbox-authored events such as
`RunRescheduled` and `MailboxResumeFailed`. Those are not runtime tee
facts. They are server canonical facts with their own consistency tier.
Mixing runtime tee, server canonical events, and diagnostics behind one
`writer` field makes the durability contract ambiguous.

## Decision

Runtime tee persistence has one durable write entry:
`CommitCoordinator::commit_checkpoint`. A reshaped `DurableEventSink`
forwards live events, normalizes them, and stages canonical drafts. It
never appends to `EventStore` itself. Server-authored canonical events and
diagnostics use distinct publisher types.

### D1: Reshape `DurableEventSink` (no `writer` field)

```rust
pub struct DurableEventSink {
    inner: Arc<dyn EventSink>,
    stager: Arc<dyn CanonicalEventStager>,
    normalizer: Arc<dyn AgentEventNormalizer>,
    mode: RuntimeEventDurability,
}

impl DurableEventSink {
    pub fn new(
        inner: Arc<dyn EventSink>,
        stager: Arc<dyn CanonicalEventStager>,
        normalizer: Arc<dyn AgentEventNormalizer>,
        mode: RuntimeEventDurability,
    ) -> Self { /* ... */ }
}

#[async_trait]
impl EventSink for DurableEventSink {
    async fn emit(&self, event: AgentEvent) {
        // 1. forward the live event to `inner`;
        // 2. normalize into zero or more CanonicalEventDrafts;
        // 3. stage each selected draft through CanonicalEventStager.
    }
}
```

0.6.0 deletes the legacy constructor
`DurableEventSink::new(inner, writer, normalizer, mode)`. It also deletes
`BufferedDurableEventSink`; its live-first staging behavior is folded into
`DurableEventSink`.

`RuntimeEventDurability` remains the selection policy for which normalized
fidelity classes are staged:

| Mode | Runtime capture config | Stager required | Effect |
|---|---|---:|---|
| `Disabled` | Not installed | No | The composition root returns the live sink unchanged. |
| `Compacted` | Installed | Yes | Stage committed/domain facts; skip observed token deltas. |
| `FullFidelity` | Installed | Yes | Stage every normalizable runtime tee fact. |

There is no no-op stager in the persistent capture path. `Disabled` means
no durable runtime wrapper is constructed.

### D2: `CanonicalEventStager` is a narrow crate-boundary port

`DurableEventSink` stays in `awaken-contract` because it owns the
contract-level mapping from `AgentEvent` to `CanonicalEventDraft`. The
concrete `EventBuffer` stays in `awaken-runtime` because it owns runtime
checkpoint lifecycle (`drain`, buffer disposal, and per-dispatch sharing).
That split avoids both unwanted dependencies:

- `awaken-contract` must not depend on `awaken-runtime` to name
  `EventBuffer`.
- The runtime checkpoint code must not expose `drain` through a public
  contract trait that arbitrary emitters can call.

The contract-facing port is intentionally stage-only:

```rust
/// Crate-boundary port for runtime event staging. A single runtime
/// implementation is expected; this trait exists to keep contract types
/// from depending on the runtime-owned EventBuffer.
pub trait CanonicalEventStager: Send + Sync {
    fn stage(&self, draft: CanonicalEventDraft);
}
```

`EventBuffer` is the expected runtime implementation, but the trait is not
introduced for substitutability. It is a one-method boundary that lets a
contract-level sink write into runtime-owned staging without making the
buffer type part of the contract crate.

### D3: Explicit buffer sharing and drain ownership

The composition root constructs one concrete `Arc<EventBuffer>` per run
activation when runtime event capture is enabled. That same allocation is
passed through two different views:

```rust
let buffer: Arc<EventBuffer> = Arc::new(EventBuffer::new());
let stager: Arc<dyn CanonicalEventStager> = buffer.clone();
let sink = Arc::new(DurableEventSink::new(live_sink, stager, normalizer, mode));
let activation = activation.with_event_buffer(buffer);
```

Only runtime checkpoint code receives the concrete `Arc<EventBuffer>` and
therefore can call `drain()`. The sink receives only
`Arc<dyn CanonicalEventStager>` and can only call `stage(...)`. A runtime
activation with capture enabled is invalid unless both views reference the
same `Arc<EventBuffer>`.

### D4: Server canonical events are not runtime tee sinks

Server-authored events use two explicit APIs. The committed tier is
attached to the same checkpoint plan that writes the state transition:

```rust
pub struct ServerCanonicalEvent {
    pub draft: CanonicalEventDraft,
    pub options: AppendOptions,
}

impl CheckpointCommitPlan {
    pub fn with_server_events(
        self,
        events: Vec<ServerCanonicalEvent>,
    ) -> Self { /* append in the coordinator transaction */ }
}
```

The enqueued/advisory tier is a long-lived module dependency:

```rust
#[async_trait]
pub trait OutboxServerEventPublisher: Send + Sync {
    async fn publish(
        &self,
        draft: CanonicalEventDraft,
        options: AppendOptions,
    ) -> Result<ServerEventPublishOutcome, EventPublishError>;
}

pub enum ServerEventPublishOutcome {
    Enqueued { dedupe_key: String },
}

pub enum EventPublishError {
    Validation(String),
    Enqueue(OutboxError),
    Serialization(String),
}
```

Committed publication does not have a separate async `publish` call: the
coordinator appends `ServerCanonicalEvent` entries inside
`CommitCoordinator::commit_checkpoint`, and failures surface as
`CommitError` for the whole checkpoint. Enqueued publication failures return
`EventPublishError`; the caller must either propagate the error when the
event gates correctness or log-and-continue for advisory notifications.

ADR-0038 does **not** create a config-store transaction boundary. Config
admin/audit events that must be atomic with `ConfigStore` mutation remain
outside this ADR unless their storage backend exposes a future config
transaction boundary. Until then they use the enqueued/advisory tier or a
future config transaction ADR.

### D5: `DiagnosticEventPublisher` for telemetry

```rust
pub trait DiagnosticEventPublisher: Send + Sync {
    /// Fire-and-forget. Failures are logged and never affect replay.
    fn record(&self, event: DiagnosticEvent);
}
```

Diagnostic events are non-transactional, not replayed as canonical events,
and have their own schema and retention policy.

### D6: Runtime event capture has one composition path

```rust
fn wrap_runtime_event_sink(
    &self,
    inner: Arc<dyn EventSink>,
    /* thread/run/correlation context */,
    stager: Arc<dyn CanonicalEventStager>,
) -> Arc<dyn EventSink> {
    Arc::new(DurableEventSink::new(inner, stager, normalizer, capture.mode))
}
```

`Mailbox::with_runtime_event_capture(writer, mode, origin)` is deleted.
The replacement `with_runtime_event_capture(mode, origin)` only records
capture policy and origin. At dispatch time, if capture is enabled, the
mailbox/runtime mints the per-run `EventBuffer` described in D3.

`executor.has_commit_coordinator()` is a construction precondition for
enabled runtime event capture. A mailbox or server state that enables
capture with an executor that lacks a coordinator is rejected at build time.
There is no runtime fallback to inline append.

### D7: Checkpoint writes become coordinator-owned

`ThreadRunStore::checkpoint` is no longer a general runtime/server write
entry. Store implementations still need an internal primitive that the
coordinator can call, but production runtime/server code writes through
`CommitCoordinator::commit_checkpoint`.

This supersedes ADR-0036 D8's build-time pairing check as the primary
write-boundary guarantee. The D8 check may remain as a defensive migration
error, but the steady-state contract is compile-time access to the write
primitive only through coordinator-owned internals.

0.6.0 enforces this by API shape rather than by a grep allowlist:

- read paths use read-side traits (`ThreadReader`, `RunReader`, or
  read-only adapters over existing stores);
- the checkpoint write primitive is moved behind a sealed/internal trait
  implemented by store crates and consumed by coordinator implementations;
- `ThreadRunStore::checkpoint` remains only for store conformance tests
  and downstream compatibility during the 0.6 line, is deprecated, and is
  not re-exported from server/runtime composition surfaces.

A CI grep may exist as a supplemental guard, but it is not the primary
contract. Renaming files must not be able to bypass the boundary.

## Migration

- Delete:
  - `DurableEventSink::new(inner, writer, normalizer, mode)`.
  - `BufferedDurableEventSink`.
  - the `runtime_event_capture` `if event_buffer / else` selector.
  - `Mailbox::with_runtime_event_capture(writer, mode, origin)`.
- Add:
  - `DurableEventSink::new(inner, stager, normalizer, mode)`.
  - runtime-owned `EventBuffer` with `drain()` not exposed by
    `CanonicalEventStager`.
  - `CheckpointCommitPlan::with_server_events(...)` for committed server facts.
  - `OutboxServerEventPublisher` and `DiagnosticEventPublisher`.
  - `Mailbox::with_runtime_event_capture(mode, origin)`.
- Migrate mailbox/thread-run state transitions that currently call
  `.checkpoint(...)` to `CommitCoordinator::commit_checkpoint` with
  attached server canonical drafts when atomic publication is required.
- Migrate advisory mailbox lifecycle fanout to `OutboxServerEventPublisher`.

## Risks

- Deleting inline runtime append before wiring committed server events into
  checkpoint plans and advisory events into the outbox publisher would leave
  mailbox lifecycle events without a canonical path. The runtime tee reshape
  and server event API changes land in the same change set.
- Backends without coordinator-backed checkpoint commits can only publish
  advisory server-authored events through the outbox. They must not document
  those events as atomic with run/thread state changes.

## Test Plan

1. Building runtime event capture without a coordinator returns a build
   error.
2. A staged runtime event is not visible from `EventStore::list` if the
   checkpoint commit fails.
3. The same `Arc<EventBuffer>` is shared between `DurableEventSink` staging
   and runtime checkpoint draining.
4. Server canonical facts attached through `CheckpointCommitPlan` roll back
   with the checkpoint; advisory outbox publication reports `Enqueued` or an
   `EventPublishError`.
5. Runtime/server production crates do not call the checkpoint write
   primitive directly.

## Non-Goals

- Adding a config-store transaction coordinator.
- Refactoring outbox dispatch or protocol projector internals beyond the
  publisher boundary defined here.
