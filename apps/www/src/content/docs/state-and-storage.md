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

Current built-in stores cover memory, file, and PostgreSQL for thread/run data;
memory, file, and PostgreSQL for config; memory and file for profile/shared
state; and memory, SQLite, or NATS JetStream for mailbox jobs. A
`NatsBufferedThreadStore` decorator can also wrap any thread/run backend to
coalesce checkpoint writes through a JetStream WAL.

## Mailbox backend choice

Mailbox jobs are run-dispatch control-plane records. They are separate from
the thread/run checkpoint store, so a deployment can combine, for example,
PostgreSQL thread storage with a NATS mailbox.

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
