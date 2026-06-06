---
title: "State & Storage"
description: "Build-time guidance for deciding what an agent remembers, resumes, shares, and persists."
---

State & Storage is the second Build Agents step: read it after
[Build an Agent](/awaken/how-to/build-an-agent/) and before
[Serve & Integrate](/awaken/serve-and-integrate/). It turns a runnable runtime
into one with explicit memory, recovery, and distribution boundaries.

## Purpose

State and storage belong in Build Agents because storage choices define the
agent's runtime contract before it is operated. Wire these boundaries before
exposing the agent to operators: managed config, mailbox dispatch, thread
state, profile state, and event history all depend on stores that code must
register.

## Use this section to decide

- where thread and run data should live
- where runtime config, mailbox jobs, and profile/shared state should live
- which state belongs in `StateKey`, `Thread` state, `ProfileKey`, or external storage
- which context should be injected through plugins instead of hard-coded into prompts
- how state is keyed and merged
- how much context should reach the model each turn
- how to model parent–child threads when sub-agents create their own threads

## Thread hierarchy

Threads carry an optional `parent_thread_id`. The runtime sets it on a child
thread the first time a sub-agent run materializes the thread, taking the
value from `RunActivationSnapshot.trace.parent_thread_id` (or legacy
`RunRequestSnapshot.parent_thread_id`). `ThreadStore` exposes
`list_child_threads`, `validate_thread_hierarchy`, and
`delete_thread_with_strategy(reject | detach | cascade)` so callers can pick a
child-handling policy explicitly. The default `Detach` strategy preserves
children with `parent_thread_id` cleared. The default
`delete_thread_with_strategy` implementation is not atomic across child writes
and the final delete; production stores with concurrent writers should
override it. The file, PostgreSQL, and NATS-buffered backends ship native
overrides.

Pagination: `list_threads_query(&ThreadQuery)` supports `parent_filter`
(`Any`, `Root`, or `Parent(parent_id)`) and `resource_id` filters with cursor
tokens that are validated against the original query shape on decode.
`list_message_records(thread_id, &MessageQuery)` paginates messages with
sequence-number windows, `asc`/`desc` ordering, visibility filters, and
producing-run filters.

## Recommended order

1. Start from [Build an Agent](/awaken/how-to/build-an-agent/) so the runtime, provider, tools, and plugins are known.
2. Read [State Management](/awaken/explanation/state-management/) to choose run, thread, shared, or profile state.
3. Read [State and Snapshot Model](/awaken/explanation/state-and-snapshot-model/) to understand plugin-managed `PhaseContext`, `StateCommand`, and snapshot mutation.
4. Use [File Store](/awaken/how-to/use-file-store/), [Postgres Store](/awaken/how-to/use-postgres-store/), or [NATS Stores](/awaken/how-to/use-nats-stores/) to choose a persistence backend.
5. Use [Shared State](/awaken/how-to/use-shared-state/) to share persistent state across threads and agent types.
6. Continue to [Serve & Integrate](/awaken/serve-and-integrate/) once mailbox, config, profile, trace/eval, and event durability boundaries are clear.

Current built-in stores cover memory, file, PostgreSQL, SQLite mailbox, and
NATS JetStream. Use the smallest backend that covers the durability boundary
you need:

| Capability | Memory | File | PostgreSQL | SQLite | NATS |
|---|---|---|---|---|---|
| Thread/run projections | yes | yes | yes | no | via `NatsBufferedThreadStore` decorator |
| Managed config | yes | yes | yes | no | no |
| Profile/shared state | yes | yes | no | no | no |
| Canonical events | yes | no | yes | no | no |
| Protocol replay log | yes | no | yes | no | no |
| Outbox/checkpoint repair | yes | no | yes | no | no |
| Stream checkpoints | yes | no | yes | no | no |
| Versioned registry | yes | yes | yes | no | no |
| Mailbox jobs | yes | no | no | single-node durable | distributed durable |

`NatsBufferedThreadStore` can wrap any thread/run backend to coalesce
checkpoint writes through a JetStream WAL.

## Storage boundaries

Awaken separates runtime execution state from the server control plane. Runtime
development can use the in-process `AgentRuntime` with a commit coordinator and
profile/shared state stores. Server development adds mailbox dispatch, canonical
events, protocol replay, config versioning, audit, and eval/trace persistence
around that runtime.

| Data | Contract | Runtime-only use | Server use |
|---|---|---|---|
| Thread and run projections | `ThreadRunStore` plus `CommitCoordinator` | Checkpoint read/write boundary for `AgentRuntime` | Same projections, usually committed through a server staged coordinator |
| Pending user input and dispatch lifecycle | `MailboxStore` | Not required unless the app builds its own queue | Durable background runs, resume, cancel, interrupt, HITL, protocol delivery |
| Canonical events | `EventStore` | Not required for basic in-process runs | Durable event list/SSE resume and protocol replay |
| Outbox/staged ids | `StagedCommitCoordinator` / `ThreadCommitStagedOutcome` | Runtime does not observe event/outbox ids | Server/store implementations publish event and outbox ids after commit |
| Managed registry config | `ConfigStore`, `ConfigRuntimeManager` | Optional; code can build registries directly | `/v1/config/*`, admin console edits, audit restore, hot publication |
| Admin audit | `AuditLogStore` | Optional | Required for version history, restore, and operator accountability |
| Profile/shared state | `ProfileStore`, shared-state store | Cross-run memory and learned priors | Same stores, usually shared by all served runs |
| Trace/eval data | trace store, eval stores | Optional test/operator tooling | Admin trace views, trace-to-fixture curation, eval datasets/runs |

The runtime commit outcome is intentionally narrow: `ThreadCommitOutcome`
represents runtime commit success/failure only. Server-side implementations
that need canonical event ids, server event ids, or outbox ids should use the
server-contract staged outcome.

## When building a custom store

Implement only the boundary you need, and keep runtime writes behind the
coordinator that owns the same backing data:

- `ThreadRunStore` for thread messages, run records, projections, and checkpoint reads.
- `CommitCoordinator` for durable runtime writes; do not write runtime checkpoints through an unrelated handle.
- `ConfigStore` and `VersionedRegistryStore` for managed config, publication, restore, and pinned registry replay.
- `ProfileStore` or shared-state stores for cross-run memory.
- `MailboxStore` plus `PendingMessageStore` for resumable dispatch, HITL, and pending input steering.

Use existing stores and tests as the contract examples before adding a new
backend: `crates/awaken-doctest/examples/thread_store_trait.rs`,
`crates/awaken-stores/src/memory/`, `crates/awaken-stores/src/postgres.rs`,
`crates/awaken-stores/src/pending_message_store.rs`, and
`crates/awaken-stores/tests/`.

## Mailbox backend choice

Mailbox jobs are run-dispatch control-plane records. They are separate from
the thread/run checkpoint store, so a deployment can combine, for example,
PostgreSQL thread storage with a NATS mailbox.

Mailbox dispatch status is a delivery lifecycle. `Acked` means the dispatch was
accepted or consumed; execution success is represented by the related
`RunRecord.status`, termination reason, and canonical events.

| Backend | Use when | Boundary |
| --- | --- | --- |
| `InMemoryMailboxStore` | Tests, local development, and embedded single-process runs. | Process-local only; queued dispatches are lost when the process exits. |
| `SqliteMailboxStore` | A single-node server needs durable mailbox jobs without running NATS. | Durable on local storage, but not the horizontally-scaled mailbox backend. |
| `NatsMailboxStore` | Multiple server instances need shared dispatch ownership, wakeups, and lease recovery. | Requires JetStream and KV; all instances must share the same stream, buckets, and durable consumer. |

See [Use NATS Stores](/awaken/how-to/use-nats-stores/) for distributed mailbox
configuration and operations.

## Related internals

- [State and Snapshot Model](/awaken/explanation/state-and-snapshot-model/)
- [Run Lifecycle and Phases](/awaken/explanation/run-lifecycle-and-phases/)
