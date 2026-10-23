//! The Coordinator's storage selection: where committed Thread truth and the
//! durable dispatch queue live, and how the queue wakes its consumers.
//!
//! Process configuration authors this value once; the Coordinator opens its
//! runtime authority from it. Worker sandbox, ACP, capture and execution-pool
//! settings never enter it, so the Coordinator's persistence is selected
//! without the Session executor's configuration.

use std::num::NonZeroU32;
use std::path::PathBuf;

/// The commit-store backend for a thread's committed truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreKind {
    /// Per-thread SQLite databases under the storage dir (the default durable store).
    Sqlite,
    /// Per-thread filesystem append-log directories (`DeploymentConfig::store=Fs`).
    Fs,
    /// One shared Postgres coordinator keyed by thread (`DeploymentConfig::store=Postgres`).
    Postgres,
}

/// The dispatch-queue backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchBackend {
    /// One local SQLite queue file (`dispatch.db`) under the storage dir, or an
    /// in-memory queue when there is no dir. The default.
    Sqlite,
    /// One shared Postgres queue across the fleet (`DeploymentConfig::dispatch_backend=Postgres`).
    Postgres,
}

/// The cross-node wake for the served dispatch pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wake {
    /// In-process `LocalWakeSignal` + poll (no cross-node wake). The default.
    None,
    /// `pg_notify` on the Postgres store's own database.
    PgNotify,
    /// A NATS broker (requires the `nats` feature).
    Nats,
}

/// The storage the Coordinator opens its runtime authority over. Consumers:
/// the Coordinator's persistence (open, verify, migrate) and the scenario
/// composition that selects a reference authority from the same value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoordinatorStorage {
    /// The local durable root: SQLite / filesystem commits, the SQLite queue
    /// and the Worker registry live under it.
    pub storage_dir: Option<PathBuf>,
    pub store: StoreKind,
    pub dispatch_backend: DispatchBackend,
    pub wake: Wake,
    /// The `pg_notify` channel or NATS subject the wake uses.
    pub wake_channel: String,
    pub nats_url: Option<String>,
    pub database_url: Option<String>,
    /// The process-owned Postgres pool capacity.
    pub postgres_max_connections: NonZeroU32,
}
