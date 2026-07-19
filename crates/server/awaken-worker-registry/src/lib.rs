//! Durable worker-directory adapters over one shared transition kernel.
//!
//! Memory, SQLite, and PostgreSQL differ only in atomic storage mechanics. Every
//! incarnation/generation/state decision is delegated to `transition`, preventing
//! a backend-specific second implementation of worker replacement semantics.

mod memory;
mod postgres;
mod schema;
mod sqlite;
mod transition;

pub use awaken_worker_contract::{
    RegisteredWorker, RegistryError, RegistryMutation, WorkerDirectory, WorkerHeartbeat,
    WorkerIdentity, WorkerManifest, WorkerRegistration, WorkerSnapshot, WorkerState,
};
pub use memory::MemoryWorkerDirectory;
pub use postgres::PostgresWorkerDirectory;
pub use schema::{BUNDLE_ID, registry_bundle};
pub use sqlite::SqliteWorkerDirectory;
