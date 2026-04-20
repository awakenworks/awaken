# State & Storage

This path is for teams moving beyond stateless demos.

## Use this section to decide

- where thread and run data should live
- where runtime config, mailbox jobs, and profile/shared state should live
- how state is keyed and merged
- how much context should reach the model each turn

## Recommended order

1. [Use File Store](./how-to/use-file-store.md) or [Use Postgres Store](./how-to/use-postgres-store.md) to choose a persistence backend.
2. [State Keys](./reference/state-keys.md) and [Thread Model](./reference/thread-model.md) to understand state layout and lifecycle.
3. [Optimize Context Window](./how-to/optimize-context-window.md) when context size starts to matter.

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

See [Use NATS Stores](./how-to/use-nats-stores.md) for distributed mailbox
configuration and operations.

## Related internals

- [State and Snapshot Model](./explanation/state-and-snapshot-model.md)
- [Run Lifecycle and Phases](./explanation/run-lifecycle-and-phases.md)
