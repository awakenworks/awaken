# Use File Store

Use `FileStore` for local development and small single-node deployments.

## Prerequisites

- Writable local directory for thread files.
- Single-writer assumptions or low write contention.

## Steps

1. Create file store.

```rust,ignore
use std::sync::Arc;
use tirea_store_adapters::FileStore;

let store = Arc::new(FileStore::new("./threads"));
```

2. Inject into `AgentOsBuilder`.

```rust,ignore
use tirea::composition::{tool_map, AgentDefinition, AgentDefinitionSpec, AgentOsBuilder};

let os = AgentOsBuilder::new()
    .with_tools(tool_map([MyTool]))
    .with_agent_spec(AgentDefinitionSpec::local_with_id(
        "assistant",
        AgentDefinition::new("gpt-4o-mini"),
    ))
    .with_agent_state_store(store.clone())
    .build()?;
```

3. Run once, then inspect persisted files under `./threads`.

## Verify

- After one run, a thread JSON file exists.
- Reloading the same thread id returns persisted messages and state.
- Version preconditions reject conflicting concurrent appends.

## Common Errors

- Directory permission denied.
- Multiple writers on same files causing frequent conflicts.
- Assuming file store is suitable for horizontally scaled production.

## Related Example

- `examples/ai-sdk-starter/README.md` and `examples/copilotkit-starter/README.md` both default to local file-backed storage for their starter backends

## Key Files

- `crates/tirea-store-adapters/src/file_store.rs`
- `crates/tirea-store-adapters/src/file_run_store.rs`
- `examples/src/lib.rs`
- `examples/src/starter_backend/mod.rs`

## Related

- [Use Postgres Store](./use-postgres-store.md)
- [Thread Model](../reference/thread-model.md)
