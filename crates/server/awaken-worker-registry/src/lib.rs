//! Durable worker-directory adapters over one shared transition kernel.
//!
//! Memory, SQLite, and PostgreSQL differ only in atomic storage mechanics. Every
//! incarnation/generation/state decision is delegated to `transition`, preventing
//! a backend-specific second implementation of worker replacement semantics.

#[cfg(any(test, feature = "test-support"))]
mod memory;
mod postgres;
mod schema;
mod sqlite;
mod transition;

pub use awaken_worker_contract::{
    ExecutionLocation, LeastLoadedPolicy, PlacementContext, PlacementError, PlacementPolicy,
    PlacementRequirements, RankedWorker, RegisteredWorker, RegistryError, RegistryMutation,
    WorkerAcpCapabilityObservation, WorkerAcpCapabilityRequirement, WorkerAssignment,
    WorkerCredentialObservation, WorkerCredentialRevision, WorkerCredentialState, WorkerDirectory,
    WorkerHeartbeat, WorkerIdentity, WorkerManifest, WorkerObservationSource, WorkerRegistration,
    WorkerSnapshot, WorkerState, place, place_assignment,
};
#[cfg(any(test, feature = "test-support"))]
pub use memory::MemoryWorkerDirectory;
pub use postgres::PostgresWorkerDirectory;
pub use schema::{BUNDLE_ID, registry_bundle};
pub use sqlite::SqliteWorkerDirectory;
