---
title: "Use File Store"
description: "Use this when you need file-based persistence for threads, runs, and messages without an external database."
---

Use this when you need file-based persistence for threads, runs, and messages without an external database.

## Prerequisites

- `awaken-stores` crate with the `file` feature enabled

## Steps

1. Add the dependency.

```toml
[dependencies]
awaken-stores = { version = "0.5", features = ["file"] }
```

Or, if using the `awaken` facade crate (which re-exports `awaken-stores`), add `awaken-stores` directly for the feature flag:

```toml
[dependencies]
awaken = { version = "0.5" }
awaken-stores = { version = "0.5", features = ["file"] }
```

2. Create a FileStore.

```rust
use std::sync::Arc;
use awaken::stores::FileStore;

let store = Arc::new(FileStore::new("./data"));
```

The directory is created automatically on first write. The layout is:

```text
./data/
  threads/<thread_id>.json
  messages/<thread_id>.json
  runs/<run_id>.json
```

3. Wire it into the runtime.

```rust
use std::sync::Arc;
use awaken::AgentRuntimeBuilder;
use awaken::engine::GenaiExecutor;
use awaken::registry_spec::ModelSpec;

let runtime = AgentRuntimeBuilder::new()
    .with_thread_run_store(store)
    .with_agent_spec(spec)
    .with_provider("anthropic", Arc::new(GenaiExecutor::new()))
    .with_model(ModelSpec::new("claude-sonnet", "anthropic", "claude-sonnet-4-20250514"))
    .build()?;
```

4. Use an absolute path for production.

```rust
use std::path::PathBuf;

let data_dir = PathBuf::from("/var/lib/myapp/awaken");
let store = Arc::new(FileStore::new(data_dir));
```

## Verify

Run the agent, then inspect the data directory. You should see JSON files under `threads/`, `messages/`, and `runs/` corresponding to the thread and run IDs used.

## Common Errors

| Error | Cause | Fix |
|---|---|---|
| `StorageError::Io` | Permission denied on the data directory | Ensure the process has read/write access to the path |
| `StorageError::Io` with empty ID | Thread or run ID contains invalid characters (`/`, `\`, `..`) | Use simple alphanumeric or UUID-style IDs |
| Missing data after restart | Using a relative path that resolved differently | Use an absolute path |

## Related Example

`crates/awaken-stores/src/file.rs` -- `FileStore` implementation with filesystem layout details.

## Key Files

- `crates/awaken-stores/Cargo.toml` -- feature flag definition
- `crates/awaken-stores/src/file.rs` -- `FileStore`
- `crates/awaken-stores/src/lib.rs` -- conditional re-export

## Related

- [Build an Agent](/awaken/how-to/build-an-agent/)
- [Use Postgres Store](/awaken/how-to/use-postgres-store/)
