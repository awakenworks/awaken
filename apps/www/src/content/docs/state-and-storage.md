---
title: "State & Storage"
description: "This path is for teams moving beyond stateless demos."
---

This path is for teams moving beyond stateless demos.

## Use this section to decide

- where thread and run data should live
- where runtime config, mailbox jobs, and profile/shared state should live
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

1. [Use File Store](/awaken/how-to/use-file-store/) or [Use Postgres Store](/awaken/how-to/use-postgres-store/) to choose a persistence backend.
2. [State Keys](/awaken/reference/state-keys/) and [Thread Model](/awaken/reference/thread-model/) to understand state layout and lifecycle.
3. [Optimize Context Window](/awaken/how-to/optimize-context-window/) when context size starts to matter.

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
