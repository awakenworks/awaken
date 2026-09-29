# Commit, Fact, And Projection Taxonomy

This document separates live execution output, durable runtime truth, protocol
replay, and public projections. It exists to prevent adapters, observability, and
webhooks from becoming side-write sources of runtime truth.

## Truth Levels

| Level | Owner | Durable? | May drive replay? | Rule |
|---|---|---|---|---|
| live stream item | `StreamSink` caller/runtime | no | no | latency-only output; may be lost |
| event draft | runtime event staging | no | no | staged until commit |
| `ThreadCommit` | runtime and store contract | pending atomic write | yes after commit | single write plan for messages, state, events, and facts |
| committed fact/event | store contract | yes | yes | authoritative runtime truth |
| protocol replay row | adapter/server projection | yes when committed with source or derived after commit | yes for public replay only | public protocol cursor, not runtime truth |
| public event | adapter | maybe | public replay only if backed by replay row/fact | DTO projection of committed truth |
| webhook/dataset row | analytics/product sink | yes in its sink | no for runtime | consumer projection |

Runtime truth is the committed fact/event/message/state record. Everything else is
either pending, live-only, or a projection.

## Commit Boundary

```text
execution output
  -> state commands and event drafts
  -> ThreadCommit
  -> CommitCoordinator
  -> committed facts/events/messages/state
  -> projection, replay, webhook, dataset, eval
```

Adapters must not emit durable public status before the commit that makes the
source fact visible. Live streaming is allowed only as best-effort output and
must be reconciled by committed history.

Managed SSE uses that same committed projection for local broadcast wakes,
silent-period reconciliation and lag recovery. One delivery comparison owns new
identities and the sole legal enrichment, `processed_at: null -> known` under an
unchanged immutable payload. Exact repeats and delayed empty observations
stutter; conflicting payload or known-time changes fail before cache publication.
The existing Event page index retains only each id's latest delivery change and
provides bounded catch-up pages, including completion behind a positional cursor.
It stores no second payload history. A prior complete prefix remains readable
but cannot prove the latest activity terminal; SSE reobserves before closing.
Preview frames remain connection-local and cannot starve committed reconciliation.

## Runtime Call, Staging, And Commit Layers

The hook/tool/model path is not one execution-only axis. It crosses execution,
state/event staging, commit, and projection. The boundary is clear when each
layer is named by the value it may produce.

| Layer | Existing roles | May do | Must not do |
|---|---|---|---|
| Runtime call | `PhaseHook`, `ToolGateHook`, `Tool`, `LlmExecutor`, backend executor | perform runtime work and return live output, `ToolOutput`, `StateCommand`, `EventDraft`, or effect candidates | write the event store, append messages, emit protocol replay truth |
| Staging | `StateCommand`, `ToolOutput.command`, `EventDraft`, `DurableEventDraft`, `DurableEventStager`, `StagedDurableEvent` | collect candidate state, event, fact, effect, or outbox changes | expose committed records or claim durability |
| Live state apply | `MutationBatch`, `StateStore`, `Snapshot` | validate registered keys and advance the active run's live revisioned projection | claim durable replay visibility or write the event/fact store |
| Commit plan | `ThreadCommit` | group message delta, run projection, state export, staged events/facts, and outbox intent where supported | execute tools/hooks or project public protocol DTOs |
| Commit boundary | `CommitCoordinator` | atomically make the checkpoint visible or reject it | call tools/hooks, resolve config, map public protocol names |
| Read/projection | `RuntimeResumeStore`, `EventReader`, `EventSubscriber`, protocol replay/outbox adapters | consume committed records and project downstream views | mutate runtime truth or observe uncommitted drafts |

A successful hook, tool, model call, or durable-event staging call means only
that a candidate value exists. `DurableEventStager` is a staging helper, not a
write boundary. `ThreadCommit` is a pending commit plan, not the durable result.
`StateStore` apply is a live execution boundary, not durable truth.
`CommitCoordinator` is the durable write boundary; projection starts after its
commit is visible.

## Projection Rules

1. A public event has an owning adapter.
2. The source is a committed fact/event, or a protocol replay row written with
   the same transaction as the source commit.
3. Public ids and replay cursors are adapter-owned projections.
4. Replaying public history does not mutate runtime truth.
5. Projection failure is retried or marked as projection failure; it does not
   roll back an already committed runtime fact unless the store transaction has
   not committed.

Managed committed reads use the same private candidate reducer and publication
CAS for warm and cold projections. A source conflict discards the candidate and
waits 10 ms before rereading; one refresh has a two-second elapsed budget,
including source I/O. A normal turn can advance more than four Thread/Session
versions, so a fixed four-attempt cutoff is not a valid availability boundary.
Unchanged prefixes still complete immediately, dependency errors propagate without
retry, and timeout cancels the read without publishing a partial candidate or
replaying any command. A continuously changing source or a source read exceeding
the budget remains unavailable; this is bounded convergence, not an unbounded
liveness guarantee. Debug diagnostics identify the conflicting source/CAS fence
without recording messages or credential material.

After a Session lifecycle recovery scan, the supervisor may coalesce trailing
external dependency hints, capped by its already scheduled recovery deadline
and by one second. Only a scan that a hint itself reached and that settled no
durable work arms that window, and it arms it for no longer than that scan's
own elapsed cost. A scan reached by the scheduled deadline answered no hint,
and a hint-driven scan that settled work is the live progress path whose next
hint is the Session's next step rather than a duplicate; neither makes the
following hint wait. Charging an empty scan its own duration holds hint-driven
scanning under half the supervisor's time however fast hints arrive, while the
latency a later hint can inherit stays proportional to what a scan costs
instead of a fixed second of wall clock. Tokio `Notify` still retains one dirty
permit while a scan runs, so committed work cannot be lost; a burst joins one
following scan instead of forcing a back-to-back global Session scan per
arrival. The existing supervisor remains the sole scheduler and the 30–300
second retry backoff remains unchanged. This removes load-amplifying duplicate
scans without adding a queue, leader, durable cursor or second reconciliation
path, and without taxing an interactive Session a fixed wait per step.

The repository's existing atomic reconciliation scan also derives three
request-local scheduling hints: incomplete Event batches, a possibly due
Environment continuation, and a possibly local realization. A false hint skips
only that phase's redundant root reload; it cannot authorize or complete work.
Every true Event hint still performs one fresh root read, now reused as the
first input of the bounded Event loop, and every Continuation or Realization
effect still begins from its ordinary fresh root/authority checks. The hints
carry no Session payload, receipt or cursor. A transition after the scan prefix
is handled by the retained wake permit or scheduled recovery, so this removes
duplicate reads without creating a recovery registry or weakening effect-time
validation.

Within one Managed candidate, the initial coordination query borrows its already
observed atomic Root Thread snapshot. The Runtime Host's existing relationship
projector and lifecycle-watermark enrichment remain the sole implementation;
the ordinary query uses the same Thread recovery reader to obtain a fresh prefix.
A supplied snapshot for another root is rejected before reading sources. No
relationship cache or second recovery algorithm is added. Command admission,
usage counters, final relationship validation and the final Root Thread read
still observe current truth. Reusing an initial prefix therefore removes one
repeated recovery read without weakening the closing consistency fences. The
query-policy counter and old-prefix/new-commit decision table verify this scope;
existing publication proofs do not prove query performance or SQL isolation.

Usage aggregation and lifecycle classification consume the candidate's already
observed atomic Thread snapshots. Usage retains the Session application's one
aggregation rule and root-owned active clock; lifecycle shares the checkpoint
reader's canonical classifier and cursor codec. Admission and history projection
therefore do not reopen a database-wide lifecycle feed. Root and coordinated
children contribute only their own retained audit facts, merged in durable event
order. Ordinary accounting and model admission still discover current links and
include the executing Thread exactly once.

Agent-boundary settlement derives its terminal observation from the exact Run
recovery snapshot it already validated. The redundant Session-level lifecycle
forwarding port is removed; the generic store feed remains available to genuine
partition-wide consumers. No new store, cache, or durable cursor is introduced.
Full source-prefix validation and publication CAS remain mandatory. The read-cut
test covers 32 actual read/failure/competing-writer schedules after removing the
independent usage and lifecycle reads. Bounded independent trace enumeration,
scope/corruption tests and the four-backend recovery conformance suite cover the
pure projection; they do not establish database isolation or unbounded liveness.

Event pagination uses derived ID positions and primary action references owned
by the published `SessionRecord`. The constructor creates its initial index;
the sole successful cache publisher rebuilds it from final Event order and
visibility owners before installing the entire record. Candidate clones share
the immutable prior index cheaply, and failed, stale or unchanged candidates
cannot replace it. This is separate from the canonicalizer's temporary joins
over the still-unordered candidate, and stores no second Event payload.
The existing Thread perspective/filter kernel validates an indexed cursor before
scanning its suffix in the requested direction. One page plus one matching
lookahead feeds the existing paginator; hidden/filtered cursors still fail.
The shared normalization policy also governs the prior-prefix observation check.
This removes repeated whole-history pagination scans, while the preceding
committed-source refresh retains its existing independent consistency work.

The same published `EventPageIndex` is also the candidate reducer's Event-id
membership authority. A cloned candidate extends its copy-on-write index only
over Events appended since the published prefix; deterministic append-once
paths no longer rescan the complete history, and Outcome and coordination
projectors no longer build private duplicate id sets. Any interval replacement
that removes Events immediately rebuilds the index from the sole candidate
vector, and successful publication rebuilds it again after canonical ordering.
The index remains disposable and derived: it cannot create, remove or authorize
an Event independently of the candidate vector and its source checkpoint.

Session inbound visibility and processing completion are distinct observations
of the same retained command. While a User Run is Running, the Session application
may anchor its exact committed input Message under the existing root CAS. This
does not set `processed` or its observation time, release the next User Run, or
create another replay store. Once retained, the anchor is immutable through
processing and terminal cleanup. Missing input remains pending; conflicting input
or invalid commit coordinates fail closed. The Managed projector continues to
enforce its existing unanchored-command barrier, so a later queued unanchored
command can still withhold Runtime-derived history; anchoring one Running input
alone is not a claim that this broader queued-history limitation is resolved.

## Time observations

Wall time is display evidence attached to the existing durable owner, never a
commit identity, sequence, or recovery trigger. Session application commands use
the shared `awaken-agent-contract::clock::Clock` port; ingress reexports that port.
Create compiles its idempotency receipt before observing root creation time.
Root CAS persists update time. Its shared application kernel normalizes only the
existing update field; the typed repository receipt accepts a candidate only on
Applied, reloads durable truth on Replayed, and rejects Conflict/Mismatch.
Fresh low-level replacements must also preserve the current durable creation
observation, including historical unknown, and retain any known update without
regression. Both SQL adapters enforce the same contract guard in their existing
transaction after receipt/owner/revision checks, before row, receipt and outbox
publication; callers cannot bypass this by constructing a complete candidate.
Inbound processing persists its first observation,
and realization failure retains its own time so later cleanup cannot rewrite it.
Resource binding times belong to each existing resolved input: manifest replacement
preserves creation time, updates only changed bindings, and does not stamp a no-op.

Thread SQL stores expose their existing `committed_at` column in the same recovery
transaction as messages, state, and lifecycle facts. The filesystem log records the
observation beside the existing commit/operation payload; replay passes that value
to its in-memory read model. It never observes restart time. Duplicate operations
return the original receipt and do not append a second timestamp. No schema,
parallel event store, or timestamp-based ordering is introduced.

Managed canonicalization attaches the owning commit observation to the existing
public event after resolving its immutable order. Millisecond precision survives
formatting; backwards clocks cannot reorder events. The shared UTC codec accepts
1970 through year 9999; display projection saturates at the last representable
millisecond. Persisted values and lease/deadline arithmetic retain their wider
SQL domain. Malformed UTC strings, including an empty fractional part, are
rejected rather than normalized into a valid observation. Legacy records without time
remain unknown. Required string DTOs and the historical inbound `processed_at`
completion marker retain the legacy `2026-01-01T00:00:00Z` compatibility sentinel;
that sentinel is not a measured time. Optional historical runtime times stay null.
New root, binding, processed inbound, and commit facts record actual observations.
Compatibility is reader-first: new readers accept historical rows, but older
`PersistedSession` readers reject unknown root fields. Upgrade all root readers
before enabling these writers; no historical timestamp backfill is inferred.

## Ordering

Ordering is by commit visibility first, then adapter projection order. A live
stream event may arrive before durable replay sees the matching fact, but replay
must converge to the committed order.

An Outcome's completed report makes its evaluation span trios listable at the
terminal state commit. Their list position uses that terminal commit and the
report's iteration order; their public `processed_at` still uses each evaluation
command's commit time. A late report therefore cannot insert historical spans
before a previously issued event cursor. Without the exact terminal and
evaluation commit anchors, the projector withholds those spans.

Assistant response coordinates use the neutral `MessageId` decoder, which also
owns the existing Run-scoped classifiers. It reads the rightmost assistant
delimiter and numeric suffix once; Managed then checks the decoded owner against
the committed Run set. Run names may contain delimiters, so splitting at the
first delimiter would change historical ownership. Response ordinals still fold
in transcript order: partials advance the next ordinal by maximum, full responses
use that ordinal, and repeated message identities retain the last projection.
For bounded identity lengths, selection takes expected O(R + M) work rather than
sorting R Runs and trying every Run for each of M messages. This is a read-side
algorithm change with exhaustive bounded equivalence and codec decision-table
tests; it does not change publication fences or establish a distributed latency
guarantee.

Runtime Host coordination reconstruction consumes that same decoder and builds
one Run-id index for the root snapshot. It no longer retains its former loop
that tried every Run for each Assistant message. Truncated, malformed, foreign
and non-Assistant messages stay excluded; duplicate selected Run records retain
the former fail-closed ambiguity error. Thus both Managed event projection and
Host relationship reconstruction share one identity grammar while keeping their
separate domain outputs. A 10,000-unrelated-Run integration case covers the
indexed membership boundary; it is algorithmic test evidence, not a latency or
allocator proof.

Managed coordination ordering also consumes the canonicalizer's existing
`MessageEventIndex`. The index decodes the sole candidate Event vector once and
joins a tool occurrence by its encoded public Thread and committed Message id.
The removed path rescanned every Event for every root/child snapshot and matched
only the Message id, discarding the Thread owner already present in the public
identity. Snapshot work is now proportional to its messages, content blocks and
exact matching tool occurrences after the one Event-index build. A qualified
Event from another Thread cannot acquire a coordination operation merely by
reusing a source id. This is disposable request-local join state; the Event
vector and committed Thread snapshots remain the only payload and source truth.

For tool waits and external results:

```text
pending wait fact committed
  -> adapter projects public tool-use event
  -> public result arrives
  -> resume command validates pending fact
  -> runtime consumes result
  -> result fact commits
  -> adapter projects public result/continuation
```

## First Vertical Slice

1. Stage a message, state change, and event draft in one `ThreadCommit`.
2. Commit atomically.
3. Project the committed event into one public protocol event.
4. Rebuild public replay from committed facts.
5. Inject a projection failure and prove runtime truth remains committed and
   retryable.

## Guardrails

G1, G10, G13, G23, G25, and G26 in [INVARIANTS](../INVARIANTS.md). Stable commit
and event roles remain in [runtime-interface-boundaries.md](runtime-interface-boundaries.md#role-catalog)
and [runtime-behavior.md](runtime-behavior.md#runtime-behavior-role-catalog).
