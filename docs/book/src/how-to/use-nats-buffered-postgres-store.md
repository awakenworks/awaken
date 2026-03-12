# Use NATS Buffered Postgres Store

Use this for high-write runs: checkpoint deltas are buffered in NATS JetStream and flushed to Postgres at run end.

## Prerequisites

- `tirea-store-adapters` with `nats` and `postgres` features.
- Reachable PostgreSQL and NATS JetStream.

## Steps

1. Create Postgres durable store.

```rust,ignore
use std::sync::Arc;
use tirea_store_adapters::PostgresStore;

let pool = sqlx::PgPool::connect(&std::env::var("DATABASE_URL")?).await?;
let postgres = Arc::new(PostgresStore::new(pool));
postgres.ensure_table().await?;
```

2. Wrap writer with NATS JetStream buffer.

```rust,ignore
use tirea::contracts::storage::ThreadStore;
use tirea_store_adapters::NatsBufferedThreadWriter;

let nats = async_nats::connect(std::env::var("NATS_URL")?).await?;
let jetstream = async_nats::jetstream::new(nats);

let durable: Arc<dyn ThreadStore> = postgres.clone();
let buffered = Arc::new(NatsBufferedThreadWriter::new(durable, jetstream).await?);
```

3. Recover pending deltas on startup.

```rust,ignore
let recovered = buffered.recover().await?;
eprintln!("recovered {} buffered deltas", recovered);
```

4. Wire buffered writer for runtime commits, Postgres for reads.

```rust,ignore
use tirea::contracts::storage::ThreadReader;

let os = AgentOsBuilder::new()
    .with_agent_state_store(buffered.clone())
    .with_agent("assistant", AgentDefinition::new("deepseek-chat"))
    .build()?;

let read_store: Arc<dyn ThreadReader> = postgres.clone();
```

## Semantics

- During run: deltas are published to JetStream (`thread.<thread_id>.deltas`).
- On run-finished checkpoint: buffered deltas are materialized and persisted to Postgres.
- Query APIs read Postgres snapshot (CQRS), so they may lag active in-flight deltas.

## Verify

- Active runs emit normal events while Postgres writes are reduced.
- After run completion, Postgres thread contains full committed messages/state.
- `recover()` replays unacked deltas after crash.

## Common Errors

- Skipping `ensure_table()` when you expect startup-time validation of database permissions or schema creation.
- Running without JetStream enabled on NATS server.
- Expecting query endpoints to include not-yet-flushed in-run deltas.

## Related Example

- No dedicated UI starter ships for this storage path; use `crates/tirea-agentos-server/tests/e2e_nats_postgres.rs` as the end-to-end integration fixture

## Key Files

- `crates/tirea-store-adapters/src/nats_buffered.rs`
- `crates/tirea-store-adapters/src/postgres_store.rs`
- `crates/tirea-agentos-server/tests/e2e_nats_postgres.rs`

## Related

- [Use Postgres Store](./use-postgres-store.md)
- [Expose NATS](./expose-nats.md)
- `crates/tirea-agentos-server/tests/e2e_nats_postgres.rs`
