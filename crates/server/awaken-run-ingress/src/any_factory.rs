// @formal surface obligation=ingress.single_owner
// @formal surface obligation=ingress.epoch_reclaim
// @formal surface obligation=ingress.settle_fence
// @formal reason: Coordinator SQL backend selection; the shared erased
// @formal reason: dispatch implementation retains one exact role bundle.
//! Coordinator-owned SQLite/Postgres/NATS factories for the erased dispatch store.

use std::sync::Arc;

use awaken_agent_contract::stream::checkpoint::StreamCheckpointStore;
use awaken_dispatch_erasure::AnyDispatchStore;

#[cfg(feature = "durable")]
use crate::postgres::PostgresDispatchStore;
#[cfg(feature = "durable")]
use crate::sqlite::SqliteDispatchStore;

/// Open the SQLite backend at `path` (per-thread file queue; survives restart).
/// `clock` is the time the backend judges its own rows against — lease
/// expiry, retry due, reservation lapse — and is required for the reason
/// given on [`SqliteDispatchStore::open`].
#[cfg(feature = "durable")]
pub async fn open_sqlite(
    path: &str,
    clock: Arc<dyn awaken_time::WallClock>,
) -> Result<AnyDispatchStore, String> {
    SqliteDispatchStore::open(path, clock)
        .await
        .map(|store| AnyDispatchStore::from_durable_store(store, None))
        .map_err(|e| e.to_string())
}

/// Open an in-memory SQLite backend for tests and scenario fixtures over a
/// caller-owned clock, so a test drives the exact time the selected backend
/// judges its rows against.
#[cfg(feature = "durable")]
#[cfg(any(test, feature = "test-support"))]
pub async fn open_sqlite_in_memory(
    clock: Arc<dyn awaken_time::WallClock>,
) -> Result<AnyDispatchStore, String> {
    SqliteDispatchStore::open_in_memory(clock)
        .await
        .map(|store| AnyDispatchStore::from_durable_store(store, None))
        .map_err(|e| e.to_string())
}

/// Connect the shared Postgres backend at `url`: one queue for a multi-node
/// fleet, `FOR UPDATE SKIP LOCKED` gives distinct-claim across processes.
#[cfg(feature = "durable")]
pub async fn connect_postgres(url: &str, max_connections: u32) -> Result<AnyDispatchStore, String> {
    PostgresDispatchStore::connect(url, max_connections)
        .await
        .map(from_postgres_store)
        .map_err(|e| e.to_string())
}

/// Connect to a Postgres queue whose migration ledger was applied by the
/// deployment migration phase. No DDL is executed.
#[cfg(feature = "durable")]
pub async fn connect_postgres_existing(
    url: &str,
    max_connections: u32,
) -> Result<AnyDispatchStore, String> {
    PostgresDispatchStore::connect_existing(url, max_connections)
        .await
        .map(from_postgres_store)
        .map_err(|e| e.to_string())
}

/// Build the migrated Postgres dispatch authority from the process-owned
/// pool. Composition roots use this to share one reconnect/backpressure
/// budget with commit and Worker-registry adapters for the same database.
#[cfg(feature = "durable")]
pub async fn with_postgres_pool(pool: sqlx::PgPool) -> Result<AnyDispatchStore, String> {
    PostgresDispatchStore::with_pool(pool)
        .await
        .map(from_postgres_store)
        .map_err(|error| error.to_string())
}

/// Verify an externally migrated dispatch schema over a process-owned pool.
#[cfg(feature = "durable")]
pub async fn with_existing_postgres_pool(pool: sqlx::PgPool) -> Result<AnyDispatchStore, String> {
    PostgresDispatchStore::with_existing_pool(pool)
        .await
        .map(from_postgres_store)
        .map_err(|error| error.to_string())
}

/// Pair a process-owned dispatch pool with its PG-NOTIFY hint. The listener
/// clones the pool handle; it does not create a second connection pool.
#[cfg(feature = "durable")]
pub async fn with_postgres_pool_and_wake(
    pool: sqlx::PgPool,
    channel: &str,
) -> Result<(AnyDispatchStore, Arc<dyn crate::wake::WakeSignal>), String> {
    with_postgres_pool_and_wake_access(pool, channel, false).await
}

/// Pair an externally migrated process-owned dispatch pool with its
/// PG-NOTIFY hint without executing DDL.
#[cfg(feature = "durable")]
pub async fn with_existing_postgres_pool_and_wake(
    pool: sqlx::PgPool,
    channel: &str,
) -> Result<(AnyDispatchStore, Arc<dyn crate::wake::WakeSignal>), String> {
    with_postgres_pool_and_wake_access(pool, channel, true).await
}

#[cfg(feature = "durable")]
async fn with_postgres_pool_and_wake_access(
    pool: sqlx::PgPool,
    channel: &str,
    verify_only: bool,
) -> Result<(AnyDispatchStore, Arc<dyn crate::wake::WakeSignal>), String> {
    let store = if verify_only {
        PostgresDispatchStore::with_existing_pool(pool).await
    } else {
        PostgresDispatchStore::with_pool(pool).await
    }
    .map_err(|error| error.to_string())?;
    let wake: Arc<dyn crate::wake::WakeSignal> =
        Arc::new(crate::wake::PgNotifyWake::new(store.wake_pool(), channel));
    Ok((from_postgres_store(store), wake))
}

/// Connect the Postgres backend and, sharing its pool, a `PgNotifyWake` over
/// `channel`: the served pool's wake fires `pg_notify` on the same database it
/// enqueues into, so a peer node's `LISTEN` is nudged with no extra
/// infrastructure (ADR-0019/0024). Returns the store plus the ready wake signal;
/// keeps sqlx out of the host crate. Only Postgres carries a cross-node wake —
/// SQLite is single-process, so it stays on the in-process `LocalWakeSignal`.
#[cfg(feature = "durable")]
pub async fn connect_postgres_with_wake(
    url: &str,
    channel: &str,
    max_connections: u32,
) -> Result<(AnyDispatchStore, Arc<dyn crate::wake::WakeSignal>), String> {
    let store = PostgresDispatchStore::connect(url, max_connections)
        .await
        .map_err(|e| e.to_string())?;
    let wake: Arc<dyn crate::wake::WakeSignal> =
        Arc::new(crate::wake::PgNotifyWake::new(store.wake_pool(), channel));
    Ok((from_postgres_store(store), wake))
}

/// Verify and connect an already-migrated Postgres queue with a shared
/// `LISTEN`/`NOTIFY` wake adapter.
#[cfg(feature = "durable")]
pub async fn connect_postgres_existing_with_wake(
    url: &str,
    channel: &str,
    max_connections: u32,
) -> Result<(AnyDispatchStore, Arc<dyn crate::wake::WakeSignal>), String> {
    let store = PostgresDispatchStore::connect_existing(url, max_connections)
        .await
        .map_err(|e| e.to_string())?;
    let wake: Arc<dyn crate::wake::WakeSignal> =
        Arc::new(crate::wake::PgNotifyWake::new(store.wake_pool(), channel));
    Ok((from_postgres_store(store), wake))
}

/// Connect the Postgres durable backend but pair it with a
/// [`NatsWakeSignal`](crate::wake::NatsWakeSignal)
/// (feature `nats`) instead of `pg_notify` for the cross-node wake hint. The
/// durable STORE stays Postgres (the queue authority); only the best-effort wake
/// fan-out moves to a NATS `subject`, for a fleet that already runs a NATS broker
/// (ADR-0019/0028). Returns the store plus the ready wake signal. Like every
/// [`WakeSignal`](crate::wake::WakeSignal) the hint is non-authoritative — a lost NATS message only defers
/// a drain to the poll fallback, so correctness never depends on it.
#[cfg(feature = "nats")]
pub async fn connect_postgres_with_nats_wake(
    db_url: &str,
    nats_url: &str,
    subject: &str,
    max_connections: u32,
) -> Result<(AnyDispatchStore, Arc<dyn crate::wake::WakeSignal>), String> {
    let store = PostgresDispatchStore::connect(db_url, max_connections)
        .await
        .map_err(|e| e.to_string())?;
    let wake: Arc<dyn crate::wake::WakeSignal> = Arc::new(
        crate::wake::NatsWakeSignal::connect(nats_url, subject.to_string())
            .await
            .map_err(|e| e.to_string())?,
    );
    Ok((from_postgres_store(store), wake))
}

/// Verify and connect an already-migrated Postgres queue with a NATS wake
/// adapter. The NATS hint remains non-authoritative.
#[cfg(feature = "nats")]
pub async fn connect_postgres_existing_with_nats_wake(
    db_url: &str,
    nats_url: &str,
    subject: &str,
    max_connections: u32,
) -> Result<(AnyDispatchStore, Arc<dyn crate::wake::WakeSignal>), String> {
    let store = PostgresDispatchStore::connect_existing(db_url, max_connections)
        .await
        .map_err(|e| e.to_string())?;
    let wake: Arc<dyn crate::wake::WakeSignal> = Arc::new(
        crate::wake::NatsWakeSignal::connect(nats_url, subject.to_string())
            .await
            .map_err(|e| e.to_string())?,
    );
    Ok((from_postgres_store(store), wake))
}

#[cfg(feature = "durable")]
fn from_postgres_store(store: PostgresDispatchStore) -> AnyDispatchStore {
    let stream_checkpoint: Arc<dyn StreamCheckpointStore> = Arc::new(store.checkpoint_store());
    AnyDispatchStore::from_durable_store(store, Some(stream_checkpoint))
}
