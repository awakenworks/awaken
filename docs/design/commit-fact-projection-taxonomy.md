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

## Ordering

Ordering is by commit visibility first, then adapter projection order. A live
stream event may arrive before durable replay sees the matching fact, but replay
must converge to the committed order.

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
