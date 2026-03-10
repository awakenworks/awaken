# Use Postgres Store

Use `PostgresStore` when you need shared durable storage across instances.

## Prerequisites

- `tirea-store-adapters` is enabled with feature `postgres`.
- A reachable PostgreSQL DSN is available.
- Tables are created via `ensure_table()` before serving traffic.

## Steps

1. Add dependencies.

```toml
[dependencies]
tirea-store-adapters = { version = "0.3.0-alpha.3", features = ["postgres"] }
sqlx = { version = "0.8", features = ["runtime-tokio", "postgres"], default-features = false }
```

2. Connect pool and initialize store.

```rust,ignore
use std::sync::Arc;
use tirea_store_adapters::PostgresStore;

let dsn = std::env::var("DATABASE_URL")?;
let pool = sqlx::PgPool::connect(&dsn).await?;
let store = Arc::new(PostgresStore::new(pool));
store.ensure_table().await?;
```

3. Inject into `AgentOsBuilder`.

```rust,ignore
let os = AgentOsBuilder::new()
    .with_tools(tool_map([MyTool]))
    .with_agent("assistant", AgentDefinition::with_id("assistant", "gpt-4o-mini"))
    .with_agent_state_store(store.clone())
    .build()?;
```

4. Run and load persisted thread.

```rust,ignore
let _ = os.run_stream(run_request).await?;
let loaded = store.load_agent_state("thread-1").await?;
```

## Verify

- `load_agent_state("thread-1")` returns `Some(Thread)` after a run.
- `load_messages` returns stored messages in expected order.
- Concurrent write conflicts surface as `VersionConflict` (not silent overwrite).

## Common Errors

- Missing tables:
  Call `ensure_table()` once during startup.
- DSN/auth failures:
  Validate `DATABASE_URL` and database permissions.
- Feature not enabled:
  Confirm `postgres` feature is enabled on `tirea-store-adapters`.

## Related Example

- No dedicated starter ships with Postgres prewired; the closest full integration fixture is `crates/tirea-agentos-server/tests/e2e_nats_postgres.rs`

## Key Files

- `crates/tirea-store-adapters/src/postgres_store.rs`
- `crates/tirea-agentos-server/tests/e2e_nats_postgres.rs`
- `crates/tirea-agentos-server/src/main.rs`

## Related

- [Use File Store](./use-file-store.md)
- [Thread Model](../reference/thread-model.md)