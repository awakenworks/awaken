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
awaken-stores = { version = "0.4.0", features = ["nats"] }
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

The store keeps an in-memory list/query index from `kv.watch_all()`. Claim and
interrupt paths use the authoritative per-thread `thread-index` KV records and
load each dispatch record from KV, so they do not depend on local watcher
completeness.

Operational knobs on `NatsMailboxConfig`:

- `credentials`: optional NATS credentials-file contents for authenticated
  clusters.
- `sweeper_interval`: how often queued dispatches are checked for missing wakeup
  signals.
- `sweeper_republish_after`: how long a queued dispatch signal can remain
  suppressed before the sweeper republishes it.
- `dedup_window`: JetStream duplicate window for dispatch signal publishing.
- `watcher_initial_scan_timeout`: startup timeout for rebuilding the local and
  per-thread indexes from KV.
- `authoritative_scan_timeout`: timeout for authoritative maintenance scans.
- `nats_request_timeout`: request/reply timeout for live command delivery before
  falling back to durable dispatch.

Signal-loop tuning lives in server environment variables so existing
`MailboxConfig` struct literals remain source-compatible with 0.2.x:

| Variable | Default | Purpose |
|----------|---------|---------|
| `AWAKEN_DISPATCH_SIGNAL_BATCH_SIZE` | `32` | Maximum JetStream dispatch signals fetched per pull. |
| `AWAKEN_DISPATCH_SIGNAL_FETCH_EXPIRES_MS` | `500` | Pull fetch expiration. |
| `AWAKEN_DISPATCH_SIGNAL_NACK_BASE_DELAY_MS` | `500` | Initial delayed NAK for queued dispatches blocked by an active thread claim. |
| `AWAKEN_DISPATCH_SIGNAL_NACK_MAX_DELAY_MS` | `30000` | Maximum delayed NAK after redelivery backoff. |
| `AWAKEN_DISPATCH_SIGNAL_MAX_CONCURRENT_HANDLERS` | `32` | Maximum signal handler tasks active per pulled batch. |

The signal loop uses delayed NAK with capped exponential backoff when a queued
dispatch is available but cannot run because the thread already has an active
claim. This avoids immediate JetStream redelivery loops while keeping at-least-
once wakeup behavior.

### Operational metrics

NATS mailbox metrics are emitted through the global `metrics` recorder:

- `awaken_mailbox_dispatch_signal_pulled_total`
- `awaken_mailbox_dispatch_signal_ack_total`
- `awaken_mailbox_dispatch_signal_nack_total{delayed}`
- `awaken_mailbox_dispatch_signal_redelivery_total`
- `awaken_mailbox_dispatch_signal_republish_total`
- `awaken_mailbox_claim_attempt_total{result}`
- `awaken_mailbox_claim_scan_keys_total`
- `awaken_mailbox_claim_scan_duration_ms`
- `awaken_mailbox_authoritative_scan_keys_total`
- `awaken_mailbox_authoritative_scan_duration_ms`
- `awaken_mailbox_queued_without_signal_age_ms`
- `awaken_mailbox_claimed_dispatch_lease_age_ms`
- `awaken_mailbox_expired_claim_reclaimed_total`
- `awaken_mailbox_dedupe_lock_reconciled_total`
- `awaken_mailbox_dedupe_lock_conflict_total`
- `awaken_mailbox_live_delivery_total{result}`
- `awaken_mailbox_index_rebuild_keys_total`
- `awaken_mailbox_index_rebuild_duration_ms`

Recommended alerts:

- Queued dispatch age remains above the service's recovery target.
- Dispatch signal delayed NAK or redelivery rate spikes.
- Expired claimed dispatches are reclaimed repeatedly.
- Claim scan duration p95/p99 grows with unrelated global dispatch volume.
- Dedupe lock conflicts or reconciliations spike.
- Watcher initial index rebuild duration exceeds startup tolerance.

### Failure-mode operations

Queued dispatch stuck:

- Check `awaken_mailbox_queued_without_signal_age_ms`,
  `awaken_mailbox_dispatch_signal_republish_total`, and the JetStream durable
  consumer pending/redelivery counts.
- Verify the dispatch record is `Queued` in the dispatch KV bucket and present
  in `thread-index` for its thread. A restarting store rebuilds missing
  per-thread index entries from dispatch KV during watcher initial scan.

Claimed dispatch lease expired:

- Run or wait for `reclaim_expired_leases`. NATS reclaim scans authoritative
  thread-claim guard records and then point-reads dispatch KV, not the local
  watcher index, so a node with an incomplete local cache can still recover
  expired claims without scanning historical terminal dispatch records.
- Terminal reclaim paths clear `claim_token`, `claimed_by`, and `lease_until`
  before writing `DeadLetter` or `Superseded` records.

Dedupe lock orphan:

- A new enqueue with the same `(thread_id, dedupe_key)` reconciles the lock
  against authoritative dispatch KV and thread epoch. Missing, terminal, or
  stale queued holders are purged with revision checks before retrying.

Consumer lag or redelivery pressure:

- Inspect the durable consumer for pending messages and redeliveries.
- Increase `AWAKEN_DISPATCH_SIGNAL_BATCH_SIZE` only when mailbox workers and
  NATS can handle the extra concurrency. Increase the delayed NAK cap when
  long-running claims create repeated blocked redeliveries.

Watcher initial scan problems:

- Increase `watcher_initial_scan_timeout` when dispatch KV is large.
- Treat repeated startup timeouts as a signal to purge old terminal dispatches
  after `gc_ttl` and review `awaken_mailbox_index_rebuild_duration_ms`.

Safe restart:

- Stop accepting new requests, let active claims either finish or expire, then
  restart nodes. Durable dispatch signals and dispatch KV survive process exit;
  unacked signals redeliver and expired claims are reclaimed.

Live delivery fallback:

- `awaken_mailbox_live_delivery_total{result="no_subscriber"}` is normal when
  the active runner is absent or does not ack before `nats_request_timeout`.
  Persistent growth with queued dispatch age indicates live commands are
  falling back but durable dispatch recovery is lagging.

### Stress and chaos tests

NATS stress coverage is compiled in
`crates/awaken-stores/tests/nats_mailbox_stress.rs` and ignored by default.
Run it explicitly against Docker-backed testcontainers:

```bash
cargo test -p awaken-stores --features nats --test nats_mailbox_stress -- --ignored
```

Set `AWAKEN_NATS_STRESS_RECORDS` to scale the record count for 10k or 100k
dispatch-record runs.

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
every `flush_interval` (default 500ms). WAL messages are ACKed only after the
inner checkpoint and `flushed_seq` watermark write both succeed, so transient
watermark failures redeliver instead of leaving `force_flush()` stuck behind an
old watermark.

`NatsBufferedThreadConfig::credentials` accepts optional NATS credentials-file
contents for authenticated clusters.

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

### Poison-message quarantine

A WAL entry that consistently fails to apply (for example, a deserialization
error after an incompatible upgrade) is quarantined instead of being retried
forever. The flusher computes a stable hash over the entry, NAKs with bounded
backoff, and after the configured threshold parks the entry to a side channel
so the WAL stream keeps moving. Operators see this as a metric tick rather
than as silently stuck checkpoints. Inspect quarantined entries through the
JetStream admin tooling and replay them after the underlying defect is fixed.

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
  bucket names. JetStream WorkQueue delivery is an at-least-once wakeup signal;
  duplicate execution is prevented by dispatch KV CAS and the thread claim
  guard.
- The **same inner `ThreadRunStore`** (e.g. shared PostgreSQL). Only one
  instance's flusher processes each WAL entry; the resulting DB write must be
  visible to all instances.

Pointing two instances at separate inner stores leads to divergent DB contents.

### Guarantees verified under multi-instance load

The NATS integration suites (`tests/nats_mailbox_behavior.rs`,
`tests/nats_mailbox_conformance.rs`, `tests/nats_mailbox_stress.rs`, and
`tests/nats_buffered_thread_*.rs`) verify:

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
