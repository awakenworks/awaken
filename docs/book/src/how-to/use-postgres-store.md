# Use Postgres Store

Use this when you need durable, multi-instance persistence backed by PostgreSQL.

## Prerequisites

- `awaken-stores` crate with the `postgres` feature enabled
- A running PostgreSQL instance
- `sqlx` runtime dependencies (tokio)

## Steps

1. Add the dependency.

```toml
[dependencies]
awaken-stores = { version = "...", features = ["postgres"] }
```

2. Create a connection pool.

```rust,ignore
use sqlx::PgPool;

let pool = PgPool::connect("postgres://user:pass@localhost:5432/mydb").await?;
```

3. Create a PostgresStore.

```rust,ignore
use std::sync::Arc;
use awaken::stores::PostgresStore;

let store = Arc::new(PostgresStore::new(pool));
```

This uses default table names: `awaken_threads`, `awaken_runs`, `awaken_messages`.

4. Use a custom table prefix.

```rust,ignore
let store = Arc::new(PostgresStore::with_prefix(pool, "myapp"));
```

This creates tables named `myapp_threads`, `myapp_runs`, `myapp_messages`.

5. Wire it into the runtime.

```rust,ignore
use awaken::AgentRuntimeBuilder;

let runtime = AgentRuntimeBuilder::new()
    .with_thread_run_store(store)
    .with_agent_spec(spec)
    .with_provider("anthropic", Arc::new(provider))
    .build()?;
```

6. Schema creation.

   Tables are auto-created on first access via `ensure_schema()`. Each table uses:

- `id TEXT PRIMARY KEY`
- `data JSONB NOT NULL`
- `updated_at TIMESTAMPTZ NOT NULL DEFAULT now()`

No manual migration is required for initial setup.

## Verify

After running the agent, query the database:

```sql
SELECT id, updated_at FROM awaken_threads;
SELECT id, updated_at FROM awaken_runs;
```

You should see rows corresponding to the threads and runs created during execution.

## Common Errors

| Error | Cause | Fix |
|---|---|---|
| `sqlx::Error` connection refused | PostgreSQL is not running or the connection string is wrong | Verify the `DATABASE_URL` and that the database is accepting connections |
| `StorageError` on first write | Insufficient database privileges | Grant `CREATE TABLE` and `INSERT` permissions to the database user |
| Table name collision | Another application uses the same default table names | Use `PostgresStore::with_prefix` to namespace tables |

## Related Example

`crates/awaken-stores/src/postgres.rs` -- `PostgresStore` implementation with schema auto-creation.

## Key Files

- `crates/awaken-stores/Cargo.toml` -- `postgres` feature flag and `sqlx` dependency
- `crates/awaken-stores/src/postgres.rs` -- `PostgresStore`
- `crates/awaken-stores/src/lib.rs` -- conditional re-export

## Related

- [Build an Agent](./build-an-agent.md)
- [Use File Store](./use-file-store.md)
