# ADR-0036: Runtime Commit Atomicity and Event Buffer

- **Status**: Accepted
- **Date**: 2026-05-21
- **Depends on**: ADR-0012, ADR-0018, ADR-0030, ADR-0034

## Context

ADR-0034 established the canonical `EventStore`, `ProtocolReplayLog`, and
`Outbox` next to `ThreadRunStore`. Its D9 wrote the inline writer template:

```text
BEGIN
  update ThreadRunStore / config / domain state
  append canonical EventStore row
  insert canonical outbox row
COMMIT
```

That template applies to **server-side inline writers** — control plane,
gateway, scheduler. ADR-0034 §739–742 and §481–484 explicitly excluded the
runtime `AgentEvent` tee from this transaction:

> Runtime `AgentEvent` tee writes may append canonical events outside the
> `ThreadRunStore` checkpoint transaction until a future runtime transaction
> ADR exists.

The same deferral appears in §316–322 and in the failure mode table
(§624): an `EventStore::append` failure can be recorded but not abort the
run, because `EventSink::emit` is infallible.

Two consequences leak into production:

- A checkpoint can be durable while one or more of its events are missing
  from the canonical store, leaving replay incomplete.
- Phase code that wants to react to a failed canonical append has no
  fallible entry point; it can only observe a diagnostic after the fact.

This ADR closes both gaps by introducing a commit-atomicity boundary
between `ThreadRunStore` and `EventStore` and a server-dispatch-owned event
buffer that defers durable event drafts to that boundary. The atomicity
guarantee is **checkpoint-batched**: every canonical event a phase
produces between checkpoints is committed atomically with the
`ThreadRunStore` writes for that checkpoint. The fallible sink contract
ADR-0034 deferred is folded into the new commit entry point and is not
issued as a separate ADR.

## Non-Goals

- Per-event atomicity. Canonical events are not committed one-by-one
  with runtime state; they are batched at checkpoint boundaries.
- Cross-backend two-phase commit. Mixed `ThreadRunStore` + `EventStore`
  backends are rejected at builder time; no 2PC abstraction is added.
- Changing the wire-emit ordering of streaming events. Transient
  AgentEvents continue to reach clients before any canonical commit.
- Re-shaping `AgentEvent` or `CanonicalEventDraft`. Both retain their
  ADR-0034 shapes.
- Cross-protocol `ProtocolReplayLog` schema constraints. Tracked
  separately as a follow-up ADR.

## Decisions

### D1: Checkpoint-batched atomicity

The atomicity unit is the **checkpoint**. A phase between two checkpoints
may produce zero or more `CanonicalEventDraft` values; all of them, plus
the `ThreadRunStore` writes for the next checkpoint, plus the outbox rows
they imply, commit inside a single backend transaction. A crash before
commit drops the entire batch; the next run dispatch replays from the
last successful checkpoint.

Per-phase or per-event atomicity is explicitly rejected. Per-phase
requires re-aligning checkpoint cadence with phase boundaries and gains
little durability for significant complexity. Per-event requires the
runtime to hold backend-specific transaction handles, which collides with
ADR-0034 D5.

### D2: `CommitCoordinator` owns the cross-store transaction

A runtime-facing contract trait `CommitCoordinator` is introduced in
`awaken-runtime-contract`. Store/server crates may import the same runtime
contract through `awaken-server-contract`. It is the only abstraction that
observes both `ThreadRunStore` and `EventStore` writes. Conceptual shape:

```rust
pub trait CommitCoordinator: Send + Sync {
    fn scope(&self) -> TransactionScopeId;

    async fn commit_checkpoint(
        &self,
        plan: ThreadCommit,
    ) -> Result<ThreadCommitOutcome, CommitError>;
}

pub struct ThreadCommit {
    pub thread_id: String,
    pub message_delta: Vec<Message>,
    pub expected_message_count: Option<u64>,
    pub run_projection: RunRecord,
    pub thread_state_snapshot: Option<PersistedState>,
}
```

Server/store-owned event and outbox writes are not fields on the
runtime-facing `ThreadCommit`. They are carried by
`ThreadCommitStagedWrites` and `StagedCommitCoordinator` in
`awaken-server-contract`.

`ThreadRunStore` and `EventStore` contracts remain backend-agnostic and
do not grow transaction parameters. The coordinator is the only crate
that holds a concrete backend handle (`PgPool`, in-memory `Mutex`) and
the only place where ordering of the three writes is encoded.

Two production-shaped implementations are required by this ADR:

- `PgCommitCoordinator` — opens a `BEGIN`, drives the `ThreadRunStore`
  and `EventStore` writes through the same connection, inserts the
  outbox rows, and commits. On any error inside the block, the
  connection rolls back and the error propagates as `CommitError`.
- `MemoryCommitCoordinator` — wraps the in-memory stores behind an
  async `Mutex`. The mutex is held across the conceptual transaction so
  that the multi-store update is atomic to other observers. A panic or
  early-return inside the critical section reverts staged in-memory
  buffers before the mutex is released; observers never see partial
  state.

No non-atomic fallback is provided. The only sanctioned coordinators
are `PgCommitCoordinator` and `MemoryCommitCoordinator`. Removing the
non-atomic path is a deliberate choice over a deprecation cycle: every
caller that previously held a `ThreadRunStore` plus an `EventWriter`
must switch to a `CommitCoordinator` in the same change set, and no
production deployment can run in a silently non-atomic mode.

### D3: Strict scope match at builder time

Mixed-backend deployments — for example in-memory `ThreadRunStore` with
Postgres `EventStore` — cannot share a transaction. They are rejected,
not silently degraded. `TransactionScopeId` is an opaque equality marker
exposed by every coordinator implementation. The `RuntimeBuilder` (and
the equivalent `AppState`/`Mailbox` builders that own these stores)
validate that the supplied coordinator can drive both stores it was
constructed from before `build()` returns. A mismatch produces a build
error whose message names both backends.

`MemoryCommitCoordinator` reports a `Memory(instance_id)` scope keyed on
the pair of underlying store instances it was constructed with. Two
memory stores belonging to different test fixtures do not share scope
even though both are in-process.

This rule formalises ADR-0034 D5's "strong consistency when both writes
share the same backend transaction" by making any other configuration
unbuildable.

### D4: Event durability is a typed property of `AgentEvent`

`AgentEvent` gains a typed durability classification:

```rust
pub enum EventDurability {
    Transient,
    Durable,
}

impl AgentEvent {
    pub fn durability(&self) -> EventDurability { /* match per variant */ }
}
```

Transient variants (token deltas, partial tool arguments, phase-progress
markers) are wire-only. The runtime tees them to subscribers and never
stages them into the canonical buffer.

Durable variants (message appended, tool call completed, run status
changed, checkpoint reached, and other variants currently mapped to
canonical kinds by ADR-0034 D8) are wire-tee'd immediately **and** staged
as `CanonicalEventDraft` in the per-run `EventBuffer`. The server dispatch
path owns that buffer and drains it through a `StagingCommitCoordinator` at
the next checkpoint commit.

The classification lives next to the enum, not at emission sites. Adding
a new `AgentEvent` variant requires choosing its durability at definition
time; the compiler enforces exhaustiveness.

### D5: Wire-emit order remains live-first for runtime tee

ADR-0034 D9 stated the inline writer invariant:

```text
canonical append -> protocol replay append -> wire emit
```

That invariant continues to hold for **inline writers** (control plane,
gateway, scheduler). It does **not** apply to the runtime tee path. The
runtime emits durable events on the live wire as soon as they are staged
into the buffer; canonical commit happens later at checkpoint. The
replay path remains canonical-first because the protocol projector reads
canonical rows.

This is sound under one explicit client-side invariant:

> Canonical is the single source of truth. Any live wire event without a
> corresponding canonical row on replay must be treated by the client as
> "did not happen".

Reconciling clients (admin UI, AI SDK replay, AG-UI replay, A2A
adapters) must drop live-received durable events that fail to appear in
canonical replay after reconnect. Protocol adapter documentation owns
this contract. Transient events are exempt because they never enter
canonical.

### D6: Failure surface — fallible commit replaces fallible sink

`CommitCoordinator::commit_checkpoint` is fallible. Any of these
conditions returns `CommitError`:

| Failure | `CommitError` variant |
|---|---|
| `ThreadRunStore` write failure | `StoreWrite(ThreadRunStoreError)` |
| `EventStore::append` validation/idempotency/cursor conflict | `EventAppend(EventStoreError)` |
| Outbox insert failure | `OutboxInsert(OutboxError)` |
| Transaction commit failure (network, backend disconnect) | `Commit(BackendError)` |

The runtime treats any `CommitError` as terminal for the current run:

1. The run transitions to `Failed { reason: CheckpointCommitFailed { cause } }`
   in in-memory state. No further `commit_checkpoint` is attempted for
   this run; the in-memory failure is observed by the next dispatch.
2. The staging coordinator drains and discards the `EventBuffer`. Drafts
   staged but not committed are gone; they will not be replayed.
3. Live wire emit that has already occurred for the dropped drafts is
   the client's reconciliation problem per D5.

This subsumes ADR-0034's deferred "fallible sink" contract: the entry
point is now `commit_checkpoint`, not `EventSink::emit`, so the
infallible-emit limitation no longer applies. `EventSink::emit` remains
infallible for transient wire emission, where failure has no durable
consequence to surface.

### D7: Dedup at existing layers, not at the buffer

Buffer staging does not introduce a new idempotency mechanism. Two
existing layers already cover the relevant cases:

- `AgentEventNormalizer` (today `ScopedAgentEventNormalizer`) carries
  per-variant dedup state for variants whose canonical mapping is
  intrinsically once-per-run, such as `RunStart` → `RunStarted` /
  `RunResumed` (started_runs set) and `RunFinish` → terminal kinds
  (terminal_runs set). Re-emitting the same `AgentEvent` inside one
  run does not yield duplicate canonical drafts for these variants.
- `EventStore::append` enforces backend-level idempotency through
  `AppendOptions::idempotency_key` (ADR-0034 D9,
  `IdempotencyConflict`). A draft that survives staging and reaches
  the coordinator is deduplicated by the canonical store on append.

The buffer therefore passes drafts through unchanged. Hook retries
that re-emit an `AgentEvent` for the same canonical fact will produce
multiple staged drafts only when the normalizer admits multiple
drafts; in that case the append-time idempotency key (assigned by the
normalizer or the coordinator at flush) is the dedup boundary.

### D8: Mandatory coordinator selection

`RuntimeBuilder::build()` requires a `CommitCoordinator`. The previous
`with_thread_run_store(…)` entry point is removed in the same change
set that introduces the coordinator; all callers switch to
`with_commit_coordinator(…)` at once.

`AppState::new`, `Mailbox::new`, `ServerConfig`, and `EventSink` keep
their existing signatures. Coordinator wiring is added through the
builder surface only, satisfying ADR-0034 §735–736.

There is no production default coordinator. A `RuntimeBuilder` without a
coordinator returns a build error naming the sanctioned production choices
(`PgCommitCoordinator`, `MemoryCommitCoordinator`).

Release mailbox construction also fails closed when the executor exposes no
coordinator. Debug/test builds may construct a checkpoint-only
`MailboxRunStoreCoordinator` for embedded callers that do not publish
canonical runtime events. `FileCommitCoordinator` is dev/local only: release
builds require explicit `AWAKEN_ALLOW_DEV_FILE_COORDINATOR=true`, and callers
needing strict cross-store atomicity use `PgCommitCoordinator`.

### D9: Server-owned `EventBuffer` and `DurableEventSink` reshape

The buffer is a server-dispatch-owned, per-run structure. A minimal `stage`
trait (`CanonicalEventStager` or equivalent, exposing only
`fn stage(&self, draft: CanonicalEventDraft)`) is defined in
`awaken-runtime-contract`. The runtime never receives the concrete buffer;
server dispatch passes it to the sink as a stage-only port and to
`StagingCommitCoordinator` as the drain owner.

`DurableEventSink` is reshaped in place rather than retired:

- The `writer: Arc<dyn EventWriter>` field is replaced by
  `stager: Arc<dyn CanonicalEventStager>`.
- `emit` is reordered to forward to the inner wire sink first
  (live emission, satisfying D5), then normalize and stage drafts
  into the buffer. The previous "durable-first" forwarding gate is
  removed because durable persistence is no longer co-located with
  wire emission.
- `last_error` / `has_failed` are removed. Staging is infallible;
  the only failure surface is `CommitCoordinator::commit_checkpoint`
  per D6.

`AgentEventNormalizer` remains the single source of truth for
`AgentEvent` → `CanonicalEventDraft` mapping. Phase code and hooks
**do not** stage drafts directly; they emit `AgentEvent` through the
existing `EventSink` surface and the reshaped sink stages on their
behalf. No new `PhaseContext` API is required.

The buffer is drained by `StagingCommitCoordinator`, which wraps the
server/store `StagedCommitCoordinator`. The server installs this wrapper
with `RunActivation::with_commit_coordinator_override`, so the runtime calls
only `CommitCoordinator::commit_checkpoint(plan)`. No hook, phase code, or
runtime activation field observes the buffer concrete type.

Server-side inline writers (per ADR-0034 D9) do not go through this
buffer. They call `StagedCommitCoordinator::commit_checkpoint_staged`
with a `ThreadCommit` plus `ThreadCommitStagedWrites` built from the facts
they are publishing.

## Failure mode decisions

| Failure | Decision |
|---|---|
| `EventStore::append` fails | `CommitError::EventAppend`; transaction rolls back; run marked `Failed` per D6 |
| `ThreadRunStore` write fails | `CommitError::StoreWrite`; same handling |
| Outbox insert fails | `CommitError::OutboxInsert`; same handling |
| Transaction commit fails | `CommitError::Commit`; same handling |
| Run suspends with drafts staged | Suspension is itself a checkpoint commit; drafts flush atomically with the waiting-ticket persistence. The buffer is empty across the suspension boundary |
| Builder receives mismatched-scope stores | `RuntimeBuilder::build()` returns error naming both backends |
| Duplicate `AgentEvent` re-emitted in one run | Normalizer dedup (e.g. `started_runs`) or backend `IdempotencyConflict` at flush handles it; buffer does not dedup |

## Rollout

The change set is a single hard cut, sequenced internally as ordered
steps so each is independently reviewable but all land together:

1. Land `CommitCoordinator` in `awaken-runtime-contract`, and land
   `MemoryCommitCoordinator` / `PgCommitCoordinator` in `awaken-stores`.
2. Replace `RuntimeBuilder::with_thread_run_store` with
   `with_commit_coordinator`. The old method is removed, not deprecated.
3. Add the `EventBuffer` (and its `CanonicalEventStager` trait) and
   wire it through the server dispatch sink plus `StagingCommitCoordinator`.
   Reshape `DurableEventSink` in place:
   replace its `EventWriter` field with a `CanonicalEventStager`,
   flip `emit` to wire-first + stage, and remove `last_error` /
   `has_failed`. No new `PhaseContext` API is added; phase code
   continues to emit `AgentEvent` through the existing sink.
4. Switch `complete_step` / `persist_checkpoint` to call
   `coordinator.commit_checkpoint`. Direct
   `ThreadRunStore::record_checkpoint` calls move into the coordinator.
5. Convert every server-side construction site (`AppState`, `Mailbox`,
   tests) to supply a `PgCommitCoordinator` or
   `MemoryCommitCoordinator`. Build will not compile until all sites
   are converted.

## Testing

| Test | Coordinator | Asserts |
|---|---|---|
| `memory_commit_atomicity` | `MemoryCommitCoordinator` | Injecting `EventStore::append` failure leaves checkpoint un-advanced and staged drafts drained |
| `pg_commit_atomicity` | `PgCommitCoordinator` (testcontainer) | Same property under real Postgres transactions |
| `scope_mismatch_rejected` | builder unit test | Memory `ThreadRunStore` + Postgres `EventStore` fails `build()` with both backend names in the error |
| `phase_crash_replay` | `MemoryCommitCoordinator` | Panic mid-phase causes replay from the prior checkpoint; uncommitted durable drafts are absent on replay |
| `transient_not_persisted` | `MemoryCommitCoordinator` | A transient `AgentEvent` reaches subscribers but produces no canonical row |
| `normalizer_dedup_preserved` | `MemoryCommitCoordinator` | Re-emitting `RunStart` for the same run still produces one `RunStarted` canonical row (normalizer-level dedup survives the reshape) |

Memory coordinator conformance lives alongside ADR-0034's existing
`memory_event_store_conformance` suite.

## Relation to ADR-0034

- §316–322 (deferred fallible sink contract): superseded by D6. The
  fallible entry point is `commit_checkpoint`, not `EventSink::emit`.
- §481–484 (runtime tee outside `ThreadRunStore` transaction): superseded
  by D1+D2. Runtime tee for durable variants now commits inside the
  coordinator transaction at checkpoint cadence.
- §624 (failure table row "EventStore append fails"): superseded by D6.
- §738–742 ("future runtime transaction ADR"): satisfied by this ADR.
- ADR-0034 D5 (backend-agnostic contract layer): preserved. Neither
  `ThreadRunStore` nor `EventStore` grows transaction parameters; the
  coordinator is the only backend-aware piece.
- ADR-0034 D9 inline-writer invariant: unchanged. Runtime tee carves out
  an explicit exception under D5 of this ADR, justified by the
  reconciling-client invariant.

## Alternatives considered

### Alternative A: Per-event atomicity via runtime-held transaction

Each `AgentEvent` would be committed in its own backend transaction held
by the runtime. Rejected: violates ADR-0034 D5 by forcing the runtime to
hold a concrete backend handle, and per-event transaction overhead is
not justified by any observable consistency gain over checkpoint-batched
atomicity.

### Alternative B: Pass `&mut Tx` through both store traits

`ThreadRunStore::begin() -> Tx` plus
`EventStore::append_in_tx(&mut Tx, …)`. Rejected: the `Tx` associated
type must be either backend-concrete (leaking `sqlx::Transaction` into
the contract crate) or trait-object (downcast-heavy, poor async-trait
ergonomics). Both options re-couple contracts to backends.

### Alternative C: `ThreadRunStore` absorbs `EventStore`

`ThreadRunStore::record_checkpoint(…, events: &[CanonicalEventDraft])`
would internally write to the event table. Rejected: undoes ADR-0034
D5's separation. `ThreadRunStore` would need to understand cursor
allocation, scope rules, and replay constraints that belong to
`EventStore`.

### Alternative D: Cross-backend 2PC

A `TwoPhaseParticipant` trait letting the coordinator drive prepare /
commit across heterogeneous backends. Rejected: in-memory stores cannot
genuinely 2PC; Rust ecosystem lacks a usable abstraction; the practical
end state is identical to D3's "reject mismatched scope".

### Alternative E: Best-effort with degradation warning

Permit mismatched backends and log a degradation metric. Rejected in
favour of D3 after weighing test ergonomics against the cost of carrying
a silently-non-atomic production path. The strict-mode decision is in
the brainstorming record; the trade-off is that test fixtures must use
`MemoryCommitCoordinator` on both sides or a testcontainer Postgres
instead of mixing memory and Postgres stores.
