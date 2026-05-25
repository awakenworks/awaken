# ADR-0042: Thread Message Pending Zone and Consumption Boundary

- **Status**: 🚧 Proposed
- **Date**: 2026-05-25
- **Depends on**: ADR-0019, ADR-0034, ADR-0036, ADR-0038, ADR-0039
- **Breaking**: yes (0.6.0)

## Context

Thread messages are persisted as a whole-list value per thread and written
with last-writer-wins overwrite. `Mailbox::prepare_run_for_dispatch` performs a
non-atomic read-modify-write (`load_messages → append → checkpoint`), and run
finalization commits the whole in-memory list through `CommitCoordinator::commit_checkpoint`
(`CheckpointCommitPlan.messages` is the entire list). Two consequences:

- **Lost-update race**: concurrent same-thread writers overwrite each other.
  Two confirmed triggers — concurrent submits (auto-repair reusing a thread) and
  a run's finalization overlapping a queued submit's prepare — drop a message a
  run snapshot still references, surfacing later as
  `message '…' not found for run '…'` and a `permanent_error`. An in-process
  striped lock mitigates the single-instance case but cannot serialize writers
  across instances, because the overwrite write model is not multi-instance safe.

- **Eager commit prevents editing**: a submitted message is written into the
  shared committed list at prepare time, so there is no zone where a not-yet-read
  message can be edited, retracted, or reordered.

The product goal is Claude-Code-style behavior: **messages not yet consumed by
the model are mutable (edit / retract / reorder); once read into a prompt they
are immutable.** Consumption granularity must be selectable **per delivered
message** — consume one message per turn (queue) or coalesce a batch into one
turn.

Today six overlapping paths move messages into a run: eager
`prepare_run_for_dispatch` write, the `inbox` live channel (`send_messages` /
`inbox_payload_messages`), three submit entry points (`submit`,
`submit_background`, `submit_live`), `reusable_waiting_run_id` plus the
`<background-tasks-updated />` wake and `recover_orphaned_background_task_waits`,
and `dedupe_key` coalescing. They duplicate the same concept with divergent
semantics.

## Decision

Model a thread's messages as two zones split by a consumption watermark, drive
consumption through a single policy-parameterized boundary, and reuse the
existing mailbox fencing and commit primitives. The dispatch mailbox remains the
scheduling layer and is not changed into a message store.

### D1 — Two zones split by a consumption watermark

- **Committed** (`seq ≤ consumed_seq`): append-only, immutable history the model
  has read. Removal is expressed as a visibility tombstone, never an in-place
  rewrite or delete.
- **Pending** (`seq` unassigned): mutable staging for delivered-but-unread
  messages. Ordered by a mutable position.
- **`consumed_seq`** (per thread): the watermark, advanced atomically when a run
  reads messages into a prompt. `seq` is a per-thread monotonic order assigned at
  consumption time (the existing `MessageSeqRange` / `RunInputSnapshot.range`
  remain the reference shape).

### D2 — Mutability is consumption-gated

`edit` / `retract` / `reorder` operate only on pending entries. The operation
checks, under the per-thread fence, that the target is still pending; if it has
already been consumed it is rejected with an explicit `AlreadyConsumed` error
(the client renders it as already sent). Optimistic version/cursor checks surface
edit conflicts.

### D3 — `DeliveryMode` carried per message controls consumption granularity

Each delivered message declares how it wants to be consumed:

- `Coalesce` — folded with other pending into one turn (default conversation).
- `Sequential` — consumed alone, one turn per message (queue).
- `Steer` — delivered into the in-flight consumption if one exists, else
  `Coalesce`.
- `Barrier` — flush all prior pending first, then consume this alone.

Granularity is a property of the message, not a global switch; adding a mode is
one enum arm, not a new scheduling path.

### D4 — `freeze` is the single consumption boundary

When a run builds an LLM prompt it calls `freeze(thread)`: under the per-thread
fence it selects pending per `DeliveryMode` (all for `Coalesce`, the head one for
`Sequential`, up to the barrier for `Barrier`), appends the selected messages to
the committed log with assigned `seq`, advances `consumed_seq`, and removes them
from pending — all in one `CommitCoordinator::commit_checkpoint` transaction
together with the run record and canonical events. Consumption count decouples
from dispatch count: one run may drain pending over several turns.

### D5 — Distributed single-writer via existing fences

`freeze` and committed appends execute only on the node holding the thread's
current `dispatch_epoch` and lease; a stale epoch is rejected. Pending edits and
committed appends use conditional writes (row lock / transaction, or the event
store's expected-cursor CAS, or KV revision) so correctness does not depend on
in-process state. In-process state (the striped append lock, the inbox, the
worker thread-context cache) is a per-node fast path and cache only, invalidated
on lease loss or epoch bump.

### D6 — Removal is append-only

`strip_unpaired_tool_calls` and similar pruning become read-time view filters for
prompt construction, or durable visibility tombstones appended to the log. The
whole-list overwrite write model is removed.

### D7 — Collapse the six delivery paths into one

Inbound delivery is one operation `deliver(thread, messages, DeliveryMode)`
appending to pending, plus one scheduling rule: *pending non-empty ⇒ ensure a
consume dispatch*. `submit` / `submit_background` / `submit_live` become thin
wrappers (foreground interrupt becomes a flag, live steer becomes `Steer`); the
`inbox` becomes the in-memory notification over durable pending;
`reusable_waiting_run_id`, the background-tasks wake, and
`recover_orphaned_background_task_waits` collapse into the single scheduling rule;
`dedupe_key` coalescing is subsumed by `Coalesce`.

### D8 — Mailbox stays the scheduling layer

`RunDispatch` still represents a unit of work (claim / lease / epoch / retry /
dead-letter) and does not carry message bodies. Pending retraction reuses the
existing `cancel` (which already applies only to `Queued` dispatches); pending
bodies live in the run activation snapshot (ADR-0039) or a pending-flagged
message entry, reusing existing tables.

## Consequences

Invariants established:

- **I1** committed messages are immutable (append / tombstone only).
- **I2** pending messages are mutable until consumed.
- **I3** consumption (select + append + advance watermark) is atomic per thread.
- **I4** all thread-message writes are serialized per thread by the mailbox fence;
  different threads proceed in parallel.
- **I5** run snapshots reference committed (immutable) ids, so reconstruction
  cannot fail on a clobbered message.

Effects:

- The lost-update race and the finalization-overlap race are both removed,
  including across instances, because committed history only grows and pending
  mutation plus freeze are fenced and conditional.
- Message editing / retraction / reordering before consumption becomes a
  first-class capability with explicit `AlreadyConsumed` semantics.
- Six delivery paths reduce to one delivery operation, one pending log, one
  scheduling rule, and one consumption boundary — a net reduction in surface.
- This aligns with the CQRS-lite direction (ADR-0034) and the commit boundary
  (ADR-0036/0038): canonical events provide append ordering and conditional
  writes; the committed message list becomes a projection rather than an
  overwrite target.

Costs:

- Pruning that relied on overwrite must move to tombstone or read-time filtering.
- The canonical event store has retention; durable message history therefore
  remains in a retained projection, written append-only / conditionally rather
  than by overwrite.

## Rollout

Reused as-is: mailbox `dispatch_epoch` / `claim` / `lease` fencing,
`CommitCoordinator` atomic transaction, the canonical event store
(append-only, per-thread cursor, expected-cursor CAS, existing `MessageCommitted`
event), `cancel` for `Queued` dispatches, and the recovery / projector pipelines.

Increment A (no new table): introduce `deliver` + the pending log; make the
committed write conditional (row lock / transaction / cursor CAS) with
reload-merge retry; remove the eager whole-list overwrite from
`prepare_run_for_dispatch`. Closes the lost-update and finalization-overlap races
across instances. The in-process striped lock remains as a fast path.

Increment B: introduce `DeliveryMode` and the `freeze` boundary
(`Coalesce` / `Sequential` first); collapse `inbox`, waiting reuse, background
wake, and recovery wake into the single scheduling rule; wire `cancel` / edit on
pending.

Increment C (optional): move the committed projection to per-row append-only
storage with stored `seq` and visibility, removing whole-list reload-merge.

## Alternatives considered

- **In-process lock only** (striped per-thread async lock): mitigates the
  single-instance race but is not multi-instance safe; retained as the
  fast path within Increment A, not as the correctness mechanism.
- **Pure append-only rewrite up front**: most thorough but an ADR-level schema
  change across every backend and it breaks the overwrite-based pruning paths;
  deferred to the optional Increment C rather than required first.
- **Overwrite plus a global lock**: serializes unrelated threads and does not
  hold across instances; rejected.
