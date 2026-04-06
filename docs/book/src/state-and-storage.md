# State & Storage

This path is for teams moving beyond stateless demos.

## Use this section to decide

- where thread and run data should live
- how state is keyed and merged
- how much context should reach the model each turn

## Recommended order

1. [Use File Store](./how-to/use-file-store.md) or [Use Postgres Store](./how-to/use-postgres-store.md) to choose a persistence backend.
2. [State Keys](./reference/state-keys.md) and [Thread Model](./reference/thread-model.md) to understand state layout and lifecycle.
3. [Optimize Context Window](./how-to/optimize-context-window.md) when context size starts to matter.

## Related internals

- [State and Snapshot Model](./explanation/state-and-snapshot-model.md)
- [Run Lifecycle and Phases](./explanation/run-lifecycle-and-phases.md)
