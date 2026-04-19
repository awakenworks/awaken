# Use NATS Stores

Use these when you need a durable, horizontally-scalable message queue backend
for mailbox dispatches or a buffered write path for thread checkpoints.

Two backends live in the `nats` feature of `awaken-stores`:

- **`NatsMailboxStore`** — `MailboxStore` implementation using JetStream for
  delivery signals and NATS KV as source of truth for dispatch state.
- **`NatsBufferedThreadStore<T>`** — `ThreadRunStore` decorator that buffers
  `checkpoint()` writes in JetStream + KV and asynchronously flushes to an
  inner store (e.g. `InMemoryStore`, `PostgresStore`). Coalesces per-thread
  writes to reduce DB load.

## Prerequisites

- `awaken-stores` with `nats` feature enabled
- A running NATS server with JetStream (`nats-server -js`)
- `tokio` runtime

## Enable the feature

```toml
[dependencies]
awaken-stores = { version = "0.2", features = ["nats"] }
```

## NatsMailboxStore

```rust,ignore
use awaken_stores::{NatsMailboxConfig, NatsMailboxStore};

let config = NatsMailboxConfig::new("nats://localhost:4222");
let store = NatsMailboxStore::connect(config).await?;
// Use wherever a `MailboxStore` is expected.
```

Defaults create:

- Stream `DISPATCH` (subjects `dispatch.*`, WorkQueue retention)
- KV bucket `dispatch-state` (source of truth), `thread-epoch`, `thread-index`
- Durable consumer `dispatch-worker`

An in-memory index populated by `kv.watch_all()` serves all read paths with
zero network I/O.

## NatsBufferedThreadStore

Wrap any existing `ThreadRunStore` to buffer writes:

```rust,ignore
use std::sync::Arc;
use awaken_stores::{InMemoryStore, NatsBufferedThreadConfig, NatsBufferedThreadStore};

let inner = Arc::new(InMemoryStore::new());
let config = NatsBufferedThreadConfig::new("nats://localhost:4222");
let buffered = NatsBufferedThreadStore::connect(inner, config).await?;
```

The store implements `ThreadRunStore`, so it plugs into any location accepting
that trait.

Defaults create:

- Stream `THREADLOG` (subjects `thread.>`, file storage, 24h retention)
- KV bucket `thread-hot` (latest_seq, flushed_seq, cached run records)
- Durable consumer `thread-flusher` with 30s ack_wait

The background flusher coalesces checkpoints per thread into the inner store
every `flush_interval` (default 500ms).

### Read consistency

Configure via `NatsBufferedThreadConfig::read_consistency`:

- `ReadYourWrites` (default) — reads overlay WAL tail on top of DB when
  `latest_seq > flushed_seq`
- `Strong` — reads trigger `force_flush()` before querying DB
- `Eventual` — reads go directly to DB, ignoring WAL

### Explicit flush

```rust,ignore
store.force_flush("thread-123").await?;
```

Blocks until the background flusher has drained every WAL entry for the given
thread into the inner store. Use for admin operations or critical reads.

## When to choose which

| Need | Use |
|------|-----|
| Multi-instance mailbox with distributed claim | `NatsMailboxStore` |
| Reduce DB write amplification for hot threads | `NatsBufferedThreadStore` over Postgres |
| Maintain pagination via DB indices while buffering writes | `NatsBufferedThreadStore` |
| Single-instance, no NATS available | `InMemoryMailboxStore` + `InMemoryStore` |

## Distributed deployment

### Shared NATS, shared DB

When running multiple awaken-server instances, every instance must connect to:

- The **same NATS cluster** with identical `stream_name`, `consumer_name`, and
  bucket names. Durable consumers ensure exactly-one-delivery semantics across
  instances.
- The **same inner `ThreadRunStore`** (e.g. shared PostgreSQL). Only one
  instance's flusher processes each WAL entry; the resulting DB write must be
  visible to all instances.

Pointing two instances at separate inner stores leads to divergent DB contents.

### Guarantees verified under multi-instance load

The distributed test suite (`tests/nats_*_distributed.rs`) verifies:

- **Mailbox claim exclusivity**: concurrent `claim_dispatch` calls from
  multiple instances on the same dispatch have exactly one winner (KV CAS).
- **Lease recovery**: when an instance holding a claim crashes, another
  instance reclaims the dispatch after lease expiry via
  `reclaim_expired_leases`.
- **Interrupt propagation**: interrupt from instance A is observed by instance
  B's in-memory index via `kv.watch_all()` within the flush window.
- **Write visibility**: checkpoint from instance A is readable from instance B
  via the WAL overlay (read-your-writes) before DB flush completes.
- **Concurrent writers**: parallel `checkpoint()` calls on the same thread
  from different instances produce monotonic unique `thread_seq` (KV CAS on
  `latest_seq`) and all distinct runs land in the shared DB.

### Consumer naming

All instances sharing a mailbox or buffered thread store must use identical
`consumer_name`. Different consumer names create independent consumers that
each receive a full copy of every message — this breaks coalescing and
duplicates DB writes.
