# ADR-0034: Server CQRS-lite EventStore, ProtocolReplayLog, and Outbox

- **Status**: ✅ Accepted
- **Date**: 2026-05-20
- **Depends on**: ADR-0012, ADR-0018, ADR-0019, ADR-0022, ADR-0030

## Context

Awaken already separates runtime execution from transport encoding:
`AgentEvent` is the runtime stream, and protocol encoders turn that stream
into AI SDK, AG-UI, A2A, MCP, or other wire formats. The server storage
boundary is similarly state-oriented: `ThreadRunStore` owns current thread,
message, run, waiting, and checkpoint state. ADR-0030 added `TraceStore`, but
that store is observability-oriented: it records sampled metrics and spans, not
the durable event ledger a protocol stream can replay exactly.

Production server deployments need a different capability set:

- Reconnectable event streams need a durable cursor and replay source.
- Multi-protocol serving needs one canonical runtime event source rather than a
  separate event log per protocol.
- Multi-process deployments need reliable fanout to a different server process,
  worker, webhook relay, evaluator, or scheduler.
- Wire replay must remain stable across encoder upgrades; clients that already
  saw a wire event must receive the same wire event after reconnect.
- Debug and eval flows sometimes require full runtime deltas, while the default
  server path must avoid writing every streamed token or partial tool argument
  to storage.

The server should not solve this by making `ThreadRunStore` event-sourced.
Threads, messages, runs, waiting tickets, and checkpoints remain state
aggregates. The missing piece is a CQRS-lite event infrastructure next to those
aggregates:

```text
ThreadRunStore      current state and checkpoint truth
EventStore          canonical event truth
ProtocolReplayLog   protocol wire replay truth
Outbox              cross-process delivery truth
Protocol adapter    wire-event mapping
```

## Non-Goals

- Replacing `ThreadRunStore` or changing threads/runs into pure event-sourced
  aggregates.
- Making `awaken-runtime` depend on a concrete storage backend.
- Storing Anthropic, AI SDK, AG-UI, or other protocol wire events as the
  canonical server truth.
- Replacing `TraceStore` or OTLP observability. Trace data and event-ledger data
  have different sampling, replay, and correctness contracts.
- Providing a webhook-specific system. The outbox is a generic reliable delivery
  primitive for any cross-process consumer.
- Requiring protocol replay persistence to run as a separate service,
  database, or deployment unit. `EventStore`, `ProtocolReplayLog`, and
  `OutboxStore` are logical substrates that server processes may call through
  traits directly.
- Collapsing canonical event rows and protocol replay rows into one physical
  table. Co-location is allowed, but row ownership remains separate.

## Decision

### D1: Introduce a protocol-neutral canonical event envelope

`awaken-contract` defines a canonical event envelope that can carry runtime,
domain, and control events without naming any protocol adapter:

```text
event_id
scopes
cursors_by_scope
event_kind
payload
thread_id
run_id
causation_id
correlation_id
origin
visibility
schema_version
created_at
```

Field semantics are part of the contract:

| Field | Semantics |
|---|---|
| `event_id` | Stable canonical event identifier, shared by every scope index for the same event |
| `scopes` | Non-empty set of scopes where the event is queryable |
| `cursors_by_scope` | Per-scope cursor assigned during append; cursors from different scopes are not comparable |
| `thread_id` | Optional denormalized routing field derived from the `Thread(...)` scope or payload |
| `run_id` | Optional denormalized routing field derived from the `Run(...)` scope or payload |
| `causation_id` | Immediate upstream canonical event id or external input id that directly caused this event; absent for root domain facts |
| `correlation_id` | Request/run correlation key for tracing and diagnostics; when W3C trace context exists, this carries or references the trace id |
| `origin` | Open, lower-kebab source label such as `native`, `server`, `ai-sdk`, `ag-ui`, `a2a`, `mcp`, or `extension:<id>` |
| `visibility` | Protocol replay and redaction hint such as `public`, `internal`, `audit`, or `sensitive`; it is not an authorization decision |

`scopes` is the query and ordering truth. The denormalized `thread_id` and
`run_id` fields exist for routing, filtering, and operator readability. Append
rejects events whose denormalized IDs contradict explicit scope membership.

The canonical payload uses Awaken contract shapes or domain-specific envelopes,
not wire events from downstream protocols. Sensitive values are either redacted
or stored through a payload reference with an integrity hash; the event envelope
must not become a raw credential store.

### D2: Classify events by durability fidelity

The server records different event families at different fidelity levels:

| Class | Meaning | Default server persistence |
|---|---|---|
| `ObservedRuntimeEvent` | Streaming observations such as deltas or snapshots | Only in full-fidelity mode |
| `CommittedRuntimeEvent` | Runtime events with stable replay semantics | Yes |
| `DomainEvent` | State changes outside the runtime stream | Yes |
| `ControlEvent` | External inputs that control execution | Yes |

The default server mode is compacted: persist committed, domain, and control
events; skip streaming deltas unless full-fidelity recording is enabled.

The initial `AgentEvent` mapping is explicit so storage backends do not diverge:

| `AgentEvent` variant | Fidelity class | Compacted persistence | Notes |
|---|---|---:|---|
| `RunStart` | `DomainEvent` | yes | Translated to canonical lifecycle facts such as `RunStarted` or `RunResumed`; no separate untyped run-start fact |
| `RunFinish` | `DomainEvent` | yes | Translated by termination reason to canonical lifecycle facts; no separate untyped run-finish fact |
| `TextDelta` | `ObservedRuntimeEvent` | no | Persist only in full-fidelity mode |
| `ReasoningDelta` | `ObservedRuntimeEvent` | no | Persist only in full-fidelity mode |
| `ReasoningEncryptedValue` | `ObservedRuntimeEvent` | no | Persist only in full-fidelity mode |
| `ToolCallStart` | `ObservedRuntimeEvent` | no | Streaming detail for tool start |
| `ToolCallDelta` | `ObservedRuntimeEvent` | no | Partial tool arguments |
| `ToolCallReady` | `CommittedRuntimeEvent` | yes | Tool input is complete |
| `ToolCallDone` | `CommittedRuntimeEvent` | yes | Tool result is complete |
| `ToolCallStreamDelta` | `ObservedRuntimeEvent` | no | Persist only in full-fidelity mode |
| `ToolCallResumed` | `ControlEvent` | yes | Resume marker for waiting work |
| `ToolCallCancel` | `CommittedRuntimeEvent` | yes | Visible cancellation boundary |
| `StreamReset` | `CommittedRuntimeEvent` | yes | Affects replay consistency |
| `StepStart` | `ObservedRuntimeEvent` | no | Span/debug boundary, persisted only in full-fidelity mode |
| `StepEnd` | `ObservedRuntimeEvent` | no | Step boundary, persisted only in full-fidelity mode |
| `InferenceComplete` | `CommittedRuntimeEvent` | yes | Model request completion and usage |
| `MessagesSnapshot` | `ObservedRuntimeEvent` | no | Full-fidelity/debug only |
| `ActivitySnapshot` | `ObservedRuntimeEvent` | no | UI/debug observation |
| `ActivityDelta` | `ObservedRuntimeEvent` | no | UI/debug observation |
| `StateSnapshot` | `ObservedRuntimeEvent` | no | Internal/debug observation |
| `StateDelta` | `ObservedRuntimeEvent` | no | Internal/debug observation |
| `Error` | `CommittedRuntimeEvent` | yes | Maps to `ErrorRecorded`; may also cause `RunErrored` if it terminates the run |

A committed event is a semantic boundary, not necessarily a run boundary. A
single run may commit several messages, tool calls, tool results, and model
request completions before `RunFinish`.

### D3: Support three fidelity modes

Server wiring exposes three durability modes:

| Mode | Behavior |
|---|---|
| `Disabled` | No runtime event persistence; suitable for simple embedded usage |
| `Compacted` | Persist committed, domain, and control events; server default |
| `FullFidelity` | Persist observed runtime deltas and snapshots as well |

This keeps the production default safe for storage throughput and first-token
latency while preserving a high-detail mode for debug, incident replay, and
eval evidence.

Replay promises follow the fidelity mode. In full-fidelity mode, every live wire
event that carries a replay cursor must first be stored as a protocol replay row.
In compacted mode, only committed, domain, and control wire events are
replayable. Observed deltas may still be emitted as live-only frames, but a
live-only frame does not carry a protocol replay cursor and is not promised after
reconnect. A protocol adapter must not expose a cursor for a wire event that was
not written to `ProtocolReplayLog`. A live-only frame must not advance the
client's durable `Last-Event-ID`. If a transport requires a transient frame id
for local ordering, that id is transport-local and is not accepted as a
`ProtocolReplayCursor`.

### D4: Split EventStore traits by capability

`awaken-contract` defines separate traits instead of one monolithic store:

```text
EventWriter      append
EventReader      list, count
EventSubscriber  subscribe
EventStore       EventWriter + EventReader + EventSubscriber
```

The split keeps transaction coordination scoped to `EventWriter`, lets admin
and replay tools depend only on `EventReader`, and allows a backend to provide
live subscription via another component such as an outbox relay or message bus.

### D5: Use opaque cursors and scoped ordering

`EventCursor` is opaque on public surfaces. Callers may pass it back to
`list` or `subscribe`; they must not decode it, compare it, or derive sequence
numbers from it.

The store guarantees total order only within an `EventScope`:

```text
Thread(thread_id)
Run(run_id)
```

A canonical event may belong to several scopes. Append accepts a non-empty scope
set and returns one cursor per scope:

```text
append(scopes, event, options) -> { event_id, cursors_by_scope }
```

Append options include `writer_id`, an optional idempotency key, and an
optional expected prior cursor per scope. The idempotency identity is:

```text
writer_id + idempotency_key
```

Repeating the same identity with byte-identical append input returns the
original `event_id` and cursors instead of creating a duplicate event. The
append input equality basis is the scope set, `event_kind`, canonical payload
hash, visibility, causation id, and correlation id. Reusing the same identity
with a different equality basis returns `IdempotencyConflict`. Idempotency
records have their own retention policy; after that retention expires, a retry
may create a new event.

Expected-cursor checks are backend-enforced guards for writers that need
compare-and-append behavior. A mismatch fails the append without creating an
event.

The event body is stored once. Each scope receives an index row such as:

```text
event_scope_index(scope_type, scope_id, sequence, event_id)
```

`list(scope)` returns events indexed into that scope, ordered by that scope's
sequence, without duplicating the same `event_id` inside the page. Scope
membership is explicit. A child-thread runtime event commonly indexes into
`Thread` and `Run` scopes at the same append.

`Thread.resource_id` and `parent_thread_id` remain thread-store metadata for
membership and hierarchy. They are not canonical `EventScope` families. A
protocol that exposes a session/resource-wide ordered wire feed uses a
ProtocolReplayLog `stream_id` for that feed instead of asking EventStore to
produce cross-thread total order. Canonical EventStore readers that need a
single-thread stream read `Thread(thread_id)`, and readers that need run-level
diagnostics read `Run(run_id)`.

The standard scope family is intentionally small. Current writers index at most
one `Thread` and one `Run` scope for a single event. Adding another standard
scope type changes write amplification, index shape, and replay semantics; it
requires an ADR update or follow-up ADR.

If server internals need ordered cursor comparison, that comparison is exposed
as an internal helper tied to a scope. It is not a public cursor-format
contract.

### D6: Define subscription start semantics with a high-water mark

Live subscription supports:

```text
FromStart
FromCursor(cursor)
FromNow
```

`FromNow` captures a high-water cursor atomically with subscription creation and
returns it with the stream:

```text
SubscribeHandle { start_cursor, stream }
```

This prevents the race where a client lists the current tail, then subscribes,
and an event lands between the two operations. Backends that cannot atomically
subscribe and capture the high-water mark implement `FromNow` by capturing the
high-water cursor in storage, attaching the live subscriber, replaying events
after the high-water cursor that landed during attachment, and then continuing
with live delivery.

### D7: Connect runtime via DurableEventSink, not runtime API changes

`awaken-runtime` does not need a new run API for this infrastructure. Compatible
server wiring wraps the existing stream sink:

```text
AgentRuntime::run(request, DurableEventSink(inner_sink))
```

This is an additive durable path, not an immediate replacement for the existing
live stream path. Existing server streams may continue to use:

```text
AgentEvent
  -> ReconnectableEventSink
  -> mpsc channel
  -> wire_sse_relay
  -> EventReplayBuffer
  -> client
```

For each incoming runtime event, the wrapper:

1. assigns the fidelity class,
2. writes or buffers the canonical event according to the durability mode,
3. emits to the inner live/protocol sink.

The ordering is durable-first: append to `EventStore` before emitting to the
live sink. If durable append succeeds and live emit fails, the client can
reconnect and replay. If live emit succeeded before a failed durable append, the
client would have observed an event that cannot be replayed; that ordering is
forbidden.

Durable append failures are explicit. ADR-0036 defines the fallible entry point
as `CommitCoordinator::commit_checkpoint`, not `EventSink::emit`; runtime tee
for durable variants stages drafts into a per-run buffer and surfaces append
failures through the commit boundary. `EventSink::emit` therefore remains
infallible for transient wire emission, where failure has no durable
consequence to surface. Disabled mode does not install the durable wrapper.

Existing suspension and reconnect behavior must remain in the stream path. A
server that already uses a suspension-aware sink should wrap the durable sink
inside that detection layer:

```text
SuspensionAwareSink
  -> DurableEventSink
      -> ReconnectableEventSink
```

Client-visible `AgentEvent`s authored by server or mailbox code must use the
same sink path when durable wiring is enabled. Sending synthetic terminal or
error events directly to the transport channel would let a client observe an
event that `EventStore` cannot replay.

In server production, the inner sink of `DurableEventSink` must not emit
client-facing protocol wire events unless it first writes the corresponding
`ProtocolReplayLog` row. The inner sink is either a non-client diagnostic/live
cache or a protocol-projector-aware sink that enforces:

```text
canonical append -> protocol replay append -> wire emit
```

The forbidden production path is:

```text
canonical append -> client wire emit
```

`EventReplayBuffer` remains an in-process live cache for active-stream resume
until a specific protocol moves to `ProtocolReplayLog`-backed replay. Its
numeric SSE ids are not `ProtocolReplayCursor`s and must not be mixed with
protocol replay cursors.

### D7a: Allow non-runtime server writers

Not every canonical event originates from `awaken-runtime`. Runtime events use:

```text
awaken-runtime -> DurableEventSink -> EventWriter
```

Server, control-plane, scheduler, evaluator, and protocol-adapter code may write
canonical events directly through `EventWriter` and, when the same writer owns the
client-visible wire event, write protocol replay rows directly through the
protocol replay writer in the same transaction.

Typical non-runtime sources include:

| Source | Example canonical facts | Writer path |
|---|---|---|
| control-plane lifecycle | session created, session cancelled, run scheduled | domain transaction -> `EventWriter` |
| external user input | user message accepted, tool result submitted | domain transaction -> `EventWriter` + optional inline protocol replay |
| scheduler lifecycle | runtime bound, sandbox bound, reschedule requested | scheduler transaction -> `EventWriter` |
| evaluator output | outcome recorded, eval completed | evaluator transaction -> `EventWriter` |
| protocol adapter generated events | public lifecycle/status frames not emitted by runtime | adapter transaction -> `EventWriter` + `ProtocolReplayLog` |

These writers must still obey the same invariants: canonical rows are not wire
payloads, public wire events are replayable only after a `ProtocolReplayLog` row
exists, and cross-process notifications go through `OutboxStore`.

### D8: ProtocolReplayLog is the wire replay truth

`EventStore` is the canonical domain/debug truth. `ProtocolReplayLog` is the wire
replay truth for protocol streams. It is a logical component, not necessarily a
separate service or database. The default Postgres implementation keeps it
co-resident with `EventStore` in the same database but uses a separate
`protocol_replay_log` table instead of mixing canonical and protocol rows in one
table. This preserves separate ownership, retention, indexing, and constraints
without adding another deployment component. A future event-service wrapper is an
optional deployment evolution, not a requirement of this ADR.

A protocol replay row stores the literal wire event emitted to a client:

```text
protocol_replay_id
protocol
protocol_version
projector_version
stream_id
wire_event_id
wire_event_type
wire_payload_bytes
wire_payload_json_optional
source_event_ids
source_event_cursors
protocol_replay_cursor
redaction_state
expires_at
created_at
```

If a protocol requires byte-exact replay, the log must store the serialized wire
payload or complete wire frame as emitted (`wire_payload_bytes`). JSON/JSONB may
be stored as an auxiliary debug or indexing representation, but JSONB alone is
not a byte-stability guarantee because serialization details can change.

Replay after reconnect reads stored protocol replay rows. It does not re-run the
current projector against old canonical events, because projector changes would
alter payloads or IDs clients already observed. `EventCursor` is used for
canonical event replay; `ProtocolReplayCursor` and `wire_event_id` are used for
protocol stream replay. Protocol clients do not see canonical cursors.

Protocol replay rows are append-only. Projector upgrades affect newly written
protocol replay rows.
Older rows remain replayable with their stored payload. If a cursor refers to a
protocol replay row that should still be within retention but is missing, the
server surfaces an integrity error instead of silently re-running the projector.
If the cursor is outside retention, the server returns a cursor-expired
response.

Redaction must preserve cursor continuity while a protocol replay row remains
within retention. A redacted replay row returns a redacted or tombstone wire
payload, or an explicit redacted response, instead of disappearing as a missing
row. Removing the row entirely is allowed only through retention expiry.

Protocol projector and SDK compatibility is forward-compatible by default: new
wire formats should add fields rather than remove or reinterpret existing fields.
Breaking wire changes require a protocol version bump or a distinct wire-event
id namespace. Replay returns the stored payload as-is, including its
`projector_version`; clients that reconnect to supported history must tolerate
older projector versions for the advertised protocol version.

Protocol projectors write independent protocol replay rows from the same canonical
event. `ProtocolReplayLog` lists by `(stream_id, protocol, protocol_version)` and
orders by `protocol_replay_cursor`. Different protocols may share
`source_event_ids` while producing different `wire_event_id` values and payloads.

Only protocol-visible wire events are written to `ProtocolReplayLog`. Internal
lifecycle details, heartbeats, delivery attempts, metrics, and traces remain in
canonical/internal stores or observability systems unless a protocol explicitly
exposes them as public wire events.

| Signal | `ProtocolReplayLog` |
|---|---:|
| public protocol event emitted to a client or webhook | yes |
| runtime heartbeat | no |
| sandbox heartbeat | no |
| outbox retry / delivery attempt | no |
| webhook delivery attempt | no |
| trace span / metric sample | no |
| internal scheduler diagnostic | no |

### D9: Generate protocol replay inline or after canonical commit

The canonical write path is the consistency boundary for server-owned
control-plane and domain writes:

```text
BEGIN
  update ThreadRunStore / config / domain state
  append canonical EventStore row
  insert canonical outbox row
COMMIT
```

Runtime `AgentEvent` tee atomicity is defined by ADR-0036, which folds runtime
tee writes for durable variants into the `CommitCoordinator::commit_checkpoint`
transaction at checkpoint cadence. The pre-0036 exemption — runtime tee may
append canonical events outside the `ThreadRunStore` transaction — is removed
by ADR-0036 in a single hard cut; no non-atomic fallback coordinator exists.

Protocol replay generation has two valid paths.

Async projection is allowed when canonical and wire ownership are separate:

```text
canonical outbox -> protocol projector -> ProtocolReplayLog
ProtocolReplayLog -> protocol-replay outbox -> protocol fanout
```

Inline generation is allowed when the writer owns both the canonical fact and the
public wire event, such as gateway/control/scheduler events:

```text
BEGIN
  update domain row
  append canonical EventStore row
  write ProtocolReplayLog row
  enqueue canonical and/or protocol-replay outbox work
COMMIT
```

Inline writers must store the protocol replay row before any client-visible wire
emit. Async projectors must write the `ProtocolReplayLog` row and the
protocol-replay outbox work in the same transaction before fanout. Therefore the
common emission invariant is:

```text
canonical append -> protocol replay append -> wire emit
```

The default Postgres design uses one outbox table with a `lane` and `target`
column, not separate schemas per consumer. The logical lanes are:

| Lane | Written with | Typical target |
|---|---|---|
| `canonical` | canonical event transaction | protocol projector, evaluator, scheduler |
| `protocol_replay` | protocol replay row transaction, or inline writer transaction | SSE fanout, webhook relay, protocol bus |

An implementation may physically split lanes for operations, but the contract is
one `OutboxStore` abstraction with lane and target routing.

A protocol replay generation failure does not erase the canonical event in the
async path; the protocol projector retries, records metrics, and exposes replay
delay until the protocol replay row exists. In an inline writer transaction,
`ProtocolReplayLog` append failure rolls back the transaction and the writer must
not emit the public wire event.

### D10: Use an outbox for every cross-process push

Outbox is required whenever events must be pushed to a separate process:

- another server replica serving a live stream,
- webhook delivery,
- evaluator workers,
- scheduler workers,
- message-bus consumers.

The outbox uses at-least-once delivery. Consumers deduplicate by canonical
`event_id` or protocol `wire_event_id`. Relay failures retry with backoff and
move exhausted entries to a dead-letter state. Canonical consumers should read
canonical events from `EventStore`; protocol fanout consumers should read
protocol replay rows from `ProtocolReplayLog` so they never observe a canonical
event before its wire protocol replay row exists.

`MailboxStore` and `OutboxStore` are intentionally separate. `MailboxStore`
delivers run work and control inputs to runtime workers. `OutboxStore` delivers
notifications that committed canonical events or protocol replay rows are ready
for independent consumers. A mailbox dispatch may cause canonical events, but
protocol fanout and replay must not consume mailbox records as their event
source.

### D11: Add coarse lifecycle, tool, and mailbox events

Existing streaming detail events are not enough to explain why a run is waiting
or slow. The event model adds coarse state events:

| Event | Trigger | Fidelity class |
|---|---|---|
| `RunQueued` | A durable dispatch record is created for a run | `DomainEvent` |
| `RunSubmitted` | A dispatch is delivered to a runtime worker | `DomainEvent` |
| `RunStarted` | Runtime execution begins | `DomainEvent` |
| `RunSuspended` | The run persists a waiting state | `DomainEvent` |
| `RunResumed` | A waiting run continues after a decision or input | `DomainEvent` |
| `RunFinished` | The run reaches a normal terminal outcome | `DomainEvent` |
| `RunCancelled` | An explicit cancel request stops the run | `DomainEvent` |
| `RunInterrupted` | A user interrupt stops the current execution | `DomainEvent` |
| `RunRescheduled` | Retry, lease, or backoff logic schedules another activation | `DomainEvent` |
| `RunTerminated` | Platform policy ends the run without another activation | `DomainEvent` |
| `RunErrored` | The run reaches a terminal error outcome | `DomainEvent` |
| `ToolPermissionRequested` | Tool gate requests an approval decision | `DomainEvent` |
| `ToolPermissionResolved` | Approval or denial is recorded | `ControlEvent` |
| `ToolCallSuspended` | Tool execution waits for external input or callback | `DomainEvent` |
| `ToolCallResumed` | A suspended tool receives the required input or callback | `ControlEvent` |
| `ToolCallTimedOut` | Tool execution exceeds its configured deadline | `DomainEvent` |
| `ToolCallCancelled` | Runtime or caller cancels the tool call | `DomainEvent` |
| `ToolCallRejected` | Policy, quota, or permission denial prevents execution | `DomainEvent` |
| `MailboxSubmitFailed` | The server cannot enqueue or deliver a dispatch | `DomainEvent` |
| `MailboxDecisionReceived` | A decision or custom result is accepted by mailbox control | `ControlEvent` |
| `MailboxResumeFailed` | A resume dispatch cannot be created or delivered | `DomainEvent` |
| `MailboxTimeout` | A server-managed configurable wait deadline expires | `DomainEvent` |

Compacted mode also needs committed message facts that are independent of token
deltas. Message commits are emitted from message append or checkpoint
boundaries, not reconstructed from `TextDelta` history:

| Event | Trigger | Fidelity class |
|---|---|---|
| `MessageCommitted` | A stable message record is durably appended to the thread log; `message_kind` identifies assistant output, tool result, system/internal, or other producer categories | `DomainEvent` |
| `ThreadMessagesCheckpointed` | A checkpoint records the message range made durable for a run | `DomainEvent` |

These events do not replace `ToolCallStart`, `ToolCallDelta`, `ToolCallReady`,
or `ToolCallDone`. They give operators and protocol adapters stable state
boundaries for reconnect, diagnosis, and control surfaces.

## Consistency contract

EventStore implementations must guarantee:

- total order within a single scope,
- visibility to `list` and `subscribe` after a successful append,
- no committed-event replay gap within a scope after append returns,
- stable opaque cursors,
- `FromCursor` replay that does not lose events,
- retention behavior that returns explicit cursor-expired errors,
- idempotent retry for successful appends that supplied an idempotency key.

They may:

- provide no total order across scopes,
- batch writes,
- skip observed runtime events in compacted mode,
- use backend-specific subscription mechanisms,
- lose pre-commit events on crash.

## Failure mode decisions

| Failure | Decision |
|---|---|
| EventStore append fails | ADR-0036 routes runtime tee through `CommitCoordinator::commit_checkpoint`; append failure rolls back the checkpoint transaction and the run is marked failed at the next dispatch. Inline writers see the failure synchronously |
| Append idempotency identity reused with different input | Return `IdempotencyConflict`; do not append |
| Append expected prior cursor mismatch | Return conflict; do not append; caller may retry after reading the current cursor |
| Live sink fails after EventStore append | Accept; reconnect can replay from EventStore/ProtocolReplayLog |
| Live sink emits before append succeeds | Forbidden ordering |
| ProtocolReplayLog append fails in async projection | Keep canonical event; retry protocol replay generation; protocol replay is delayed |
| ProtocolReplayLog append fails in inline writer transaction | Roll back the transaction; do not emit the public wire event |
| Protocol replay row missing inside retention | Integrity error and metric; do not silently re-run the projector |
| Protocol replay row redacted inside retention | Preserve cursor continuity with a redacted or tombstone wire payload, or an explicit redacted response |
| Protocol replay cursor outside retention | Cursor expired / replay unavailable |
| Outbox insert fails in canonical transaction | Roll back the transaction |
| Outbox relay publish fails | Retry with backoff, then dead-letter |
| Projector bug | Mark protocol replay generation failure, expose metric, keep canonical event unchanged |
| Message bus or fanout outage | Outbox backlog grows; relay retries |
| Storage backpressure | Compacted mode remains default; full-fidelity mode can be throttled or disabled |

## Consistency boundaries

| Boundary | Consistency |
|---|---|
| ThreadRunStore and canonical EventStore | Strong when written in the same backend transaction |
| Canonical EventStore and canonical Outbox | Strong in the same backend transaction |
| EventStore and ProtocolReplayLog | Strong when written inline in one transaction; eventually consistent when generated by async projector |
| ProtocolReplayLog and client replay | Strong once protocol replay rows exist |
| Outbox and message bus/webhook | Eventually consistent, at-least-once |
| Cross-replica live visibility | Depends on outbox relay or message-bus tailing |
| TraceStore and EventStore | Independent systems with no ordering guarantee |

## Backend compatibility and consistency profiles

The contract layer remains backend-agnostic. `ThreadRunStore`, `EventStore`,
`ProtocolReplayLog`, and `OutboxStore` are separate traits and may be backed by
different implementations in embedded or test usage. The server consistency
contract is selected by deployment wiring, not by forcing every trait to share a
concrete type.

Server production wiring defines two consistency groups for the async projection
path:

```text
Group A: ThreadRunStore + EventStore + canonical outbox lane
Group B: ProtocolReplayLog + protocol-replay outbox lane
```

Inline protocol replay writers may combine Group A and Group B in one transaction
when the writer owns domain state, canonical event, wire event, and outbox work.
For strict replay and audit semantics, every used group must have one
transactional backend boundary. The default Postgres server shape satisfies this
by keeping the thread/run tables, canonical event tables, protocol replay table,
and outbox table in the same database while preserving separate logical traits
and tables.

| Usage | Backend requirement | Semantics |
|---|---|---|
| Embedded or disabled event persistence | No shared backend required | Runtime streaming only; no durable replay promise |
| Eventual-consistency server wiring | Mixed backends allowed | Requires idempotency, reconciliation, and integrity checks; cannot claim strong state/event/outbox atomicity |
| Strict production server wiring | Group A and Group B each use one transactional backend boundary; inline writers may use one combined boundary | Strong state/event/outbox commit within each group; protocol replay is strong for inline rows and eventually consistent for async projection |

Using different backends across a consistency group is allowed only as an
explicit eventual-consistency choice. For example, pairing a NATS-buffered
`ThreadRunStore` WAL with a direct Postgres `EventStore` does not provide a
single atomic commit for thread checkpoint and canonical event append. A strict
configuration should either use direct Postgres `ThreadRunStore` with Postgres
`EventStore` and canonical outbox, or extend the WAL entry to carry both the
thread checkpoint and canonical event draft so the flusher materializes them
together. That WAL-based design is a separate backend design, not the default
shape.

`TraceStore` remains outside these consistency groups. It is observability
storage with independent retention and failure semantics, not a substitute for
canonical event or protocol replay storage.

## Retention and capacity

This ADR defines correctness semantics, not deployment quotas. EventStore,
ProtocolReplayLog, and Outbox implementations must expose retention and capacity
configuration separately for compacted events, full-fidelity observed events,
protocol replay rows, and delivery attempts. Cursor-expired behavior is part of this
ADR; concrete retention durations, storage budgets, and sizing guidance belong
in operator documentation for each backend.

## Delivery shape

The compatible server slice should land as separable changes that do not change
existing public constructor shapes or stream defaults:

1. Contract shapes: event envelope, scopes, cursors, fidelity classes, and split
   store traits.
2. Shared conformance tests for EventStore behavior.
3. `InMemoryEventStore` for tests and embedded use.
4. AgentEvent-to-canonical normalizer with compacted mode filtering.
5. `DurableEventSink` as an optional wrapper around existing live sinks.
6. Server-authored client-visible events routed through the same sink path.
7. `PostgresEventStore` for server use.
8. Server list and stream-resume endpoints backed by EventStore cursors.

Protocol replay and fanout build on the compatible slice:

1. ProtocolReplayLog for stable wire replay.
2. ProtocolReplayLog conformance tests.
3. One protocol projector moved to ProtocolReplayLog-backed replay.
4. Outbox relay for cross-process delivery.
5. Coarse run, tool, and mailbox lifecycle events.
6. Thread-group, context-compaction, memory/resource, and eval event families.

ProtocolReplayLog conformance covers append-then-list by protocol stream,
byte-stable payload replay, protocol-version isolation, `wire_event_id`
uniqueness, missing rows inside retention as integrity errors, expired cursors
as cursor-expired responses, and live-only frames never producing durable replay
cursors.

Initial wiring must be opt-in and builder-based. It must not add required fields
to `AppState::new`, `Mailbox::new`, `ServerConfig`, or `EventSink`.

Runtime transaction-scope changes are addressed by ADR-0036, which adds a
`CommitCoordinator` abstraction at the contract layer and routes runtime tee
through a checkpoint-batched commit boundary. The initial server wiring
described above is unchanged; ADR-0036 layers the coordinator on top via the
builder surface without touching `AppState::new`, `Mailbox::new`,
`ServerConfig`, or `EventSink`.

## Implementation notes

### Draft vs persisted canonical events

Canonical event types are storage contract types. They are not replacements for
the runtime `AgentEvent` stream:

```text
AgentEvent / domain fact / control input
        -> CanonicalEventDraft
        -> EventStore.append(...)
        -> CanonicalEvent
        -> protocol projector
        -> ProtocolReplayLog row
```

`AgentEvent` remains the awaken-runtime execution stream. It describes what the
loop observed or completed during a run, and it can contain transient streaming
details such as token deltas or partial tool arguments. It does not own durable
identity, scoped replay cursors, idempotency, retention, or protocol replay
semantics.

`CanonicalEventDraft` is the EventStore write input. It represents a
protocol-neutral fact that a writer wants to persist after classifying,
normalizing, and assigning query scopes to a runtime, domain, or control event.
The draft intentionally excludes store-assigned fields.

`CanonicalEvent` is the EventStore output after a successful append. It is the
same canonical fact with backend-assigned identity, per-scope cursors, and
timestamp. Readers, subscribers, outbox consumers, and protocol projectors
consume this persisted shape.

Contract types should therefore separate caller input from store-assigned
output:

```text
CanonicalEventDraft
  scopes
  event_kind
  payload
  causation_id
  correlation_id
  origin
  visibility
  schema_version

CanonicalEvent
  event_id
  cursors_by_scope
  created_at
  plus the accepted draft fields
```

Writers submit drafts. Stores assign `event_id`, per-scope cursors, and
`created_at` atomically during append. This avoids requiring producers to invent
cursors before the backend has sequenced the event.

For example, `AgentEvent::RunStart` is normalized into a
`CanonicalEventDraft { event_kind: RunStarted, scopes: ..., payload: ... }`.
After append, the returned `CanonicalEvent` carries the canonical `event_id`,
the cursor for each indexed scope, and the accepted `RunStarted` payload.

### Runtime event mapping into canonical lifecycle events

Existing runtime stream variants should not create duplicate lifecycle facts.
They map into canonical lifecycle events:

```text
AgentEvent::RunStart, initial activation        -> RunStarted
AgentEvent::RunStart, after a waiting boundary  -> RunResumed
AgentEvent::RunFinish(NaturalEnd)               -> RunFinished
AgentEvent::RunFinish(BehaviorRequested)        -> RunFinished
AgentEvent::RunFinish(Suspended)                -> RunSuspended
AgentEvent::RunFinish(Cancelled)                -> RunCancelled
AgentEvent::RunFinish(Error)                    -> RunErrored
AgentEvent::RunFinish(Stopped / policy-denied)  -> RunTerminated
AgentEvent::Error                               -> ErrorRecorded
```

The `RunFinish` mapping depends on `TerminationReason`. `AgentEvent::Error`
records the error fact. If that same failure boundary also terminates the run,
the normalizer may emit `RunErrored`, but it must avoid duplicate terminal facts
for the same failure boundary. A protocol projector may still render
protocol-specific status events, but the canonical event stream should not
contain both an untyped `RunStart` fact and a second `RunStarted` fact for the
same boundary.

### Default Postgres physical shape

The default implementation should keep canonical and protocol replay data in the same
Postgres backend but in different tables:

```text
event                  canonical event body
event_scope_index      canonical per-scope ordering
protocol_replay_log    protocol wire replay rows
outbox                 delivery work items
```

A minimal `protocol_replay_log` row contains:

```text
protocol_replay_id
stream_id
protocol
protocol_version
projector_version
protocol_replay_seq
protocol_replay_cursor
wire_event_id
wire_event_type
wire_payload_bytes
wire_payload_json_optional
source_event_ids
source_event_cursors
redaction_state
created_at
expires_at
```

Recommended uniqueness constraints are `(stream_id, protocol,
protocol_version, protocol_replay_seq)` and `(protocol, protocol_version,
wire_event_id)`. A deployment may later split the table or backend for scale,
but this ADR does not require that.

### ProtocolReplayLog stream identity

Protocol replay cursors are scoped to a protocol stream id, not to every
canonical scope at once. A protocol replay row belongs to exactly one
`(stream_id, protocol, protocol_version)` stream and receives one
`protocol_replay_cursor` for that stream. Stream ids are protocol/server routing
keys such as `thread:<thread_id>` or `session:<session_id>`; they are not
canonical `EventScope` values.

If a projector needs the same wire payload to appear in several protocol
streams, it writes separate protocol replay rows. Each stream has its own
cursor. Protocol clients must not infer ordering across different protocol
replay streams.

### Message commit payload shape

Compacted mode needs a stable message fact independent of token deltas. The
minimal payload shape is:

```text
MessageCommitted
  thread_id
  run_id
  message_id
  role
  content_blocks
  message_kind
  parent_message_id
  created_at
```

`message_kind` distinguishes assistant output, tool result, system/internal
message, and other producer-specific message categories. Specialized names such
as assistant-message or tool-result commits may exist as code-level helpers, but
the default canonical fact is `MessageCommitted`. Protocol projectors must be
able to produce final wire messages from the compacted message payload without
reconstructing token deltas.

## Protocol adapter behavior

Adapters consume canonical events and protocol replay rows. Most adapters use
asynchronous projection:

```text
Canonical EventStore -> protocol projector -> ProtocolReplayLog -> stream/list
```

Adapters that author both the canonical fact and the public wire event may write
the protocol replay row inline and then stream/list from `ProtocolReplayLog`.

AI SDK, AG-UI, A2A, MCP, and future compatibility protocols should not maintain
their own canonical event stores. They may maintain protocol replay rows for
wire replay. This keeps protocol-specific schemas out of core runtime and store
contracts while allowing each adapter to preserve exact replay semantics.

## Migration notes for existing server streams

Existing server stream paths can migrate without replacing their current live
broadcast mechanism immediately:

1. Keep the existing `AgentEvent` live stream, `wire_sse_relay`, and
   `EventReplayBuffer` path active.
2. Write canonical events through `DurableEventSink` while continuing the
   current stream path.
3. Route server-authored client-visible `AgentEvent`s through the same durable
   sink path.
4. Compare EventStore replay with current stream/list output in tests.
5. Switch canonical list/replay reads to EventStore.
6. Add ProtocolReplayLog for one protocol's wire replay stability.
7. Move additional protocol replay paths from in-process buffers to
   ProtocolReplayLog one at a time.
8. Keep the current in-process broadcast as a live cache for protocols that have
   not moved to ProtocolReplayLog-backed replay.
9. Replace cross-process fanout with outbox/message-bus delivery.
10. Remove any protocol-specific event log once protocol replay is stable.

## Consequences

### Positive

- Multiple protocol adapters share the same canonical event source.
- Reconnect and replay semantics become backend-tested instead of
  protocol-specific.
- Crash recovery improves: committed events can be listed and projected after
  process restart.
- Multi-replica server deployments have a reliable fanout path.
- Protocol adapters become thinner because durability, replay, and delivery
  are server infrastructure concerns.
- Debug and eval can opt into full-fidelity event capture without forcing that
  cost onto the default server path.
- TraceStore keeps observability semantics while EventStore owns replay
  semantics; neither store has to impersonate the other.

### Negative

- The server architecture becomes more complex: EventStore, ProtocolReplayLog,
  and Outbox have separate correctness contracts.
- Protocol replay generation is eventually consistent with canonical event append
  unless a deployment chooses inline protocol replay generation.
- Full-fidelity mode can produce high write volume and needs explicit operator
  controls.
- Store implementations must pass conformance tests across cursor, retention,
  and subscription behavior.
- Multi-process delivery becomes at-least-once; consumers must deduplicate.

## Alternatives considered

### Alternative A: Make ThreadRunStore event-sourced

Rejected. Threads, messages, run records, waiting tickets, and checkpoints are
current-state aggregates in the existing architecture. Rebuilding them from an
event stream would be a larger architectural change, would complicate runtime
checkpointing, and is not needed to solve replay and fanout.

### Alternative B: Store protocol wire events as the canonical truth

Rejected. It would make the core store depend on individual protocol schemas and
would duplicate the same runtime facts across adapters. It would also make
cross-protocol replay and debug harder because the canonical representation
would already be lossy and protocol-shaped.

### Alternative C: Use TraceStore as EventStore

Rejected. TraceStore is observability-focused, may be sampled, and has
best-effort behavior. Runtime replay and stream recovery require a lossless
committed-event contract for the scopes that are persisted.

### Alternative D: Emit live events first, persist later

Rejected. A client could observe an event that is absent from replay after
reconnect. Durable-first ordering is required for stream correctness.

### Alternative E: Re-run protocol projectors during replay

Rejected. Projector upgrades can change wire payloads or IDs. ProtocolReplayLog
keeps already-emitted wire events stable.

### Alternative F: Store canonical events and protocol replay rows in one table

Rejected for the default implementation. A single physical table with a
`kind = canonical | protocol_replay` discriminator appears smaller but blurs two
different truths: canonical facts and already-emitted protocol frames. The two
row families have different schemas, indexes, retention policies, and integrity
constraints. The default Postgres backend therefore uses the same database but
separate tables.
