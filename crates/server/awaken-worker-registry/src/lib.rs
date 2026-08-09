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

fn durable_i64(field: &'static str, value: u64) -> Result<i64, RegistryError> {
    i64::try_from(value).map_err(|_| {
        RegistryError::Persistence(format!(
            "worker registry {field} {value} exceeds durable BIGINT range"
        ))
    })
}

#[cfg(test)]
mod durable_integer_tests {
    use super::*;

    #[test]
    fn durable_bigint_conversion_fails_closed() {
        assert_eq!(
            durable_i64("generation", i64::MAX as u64).unwrap(),
            i64::MAX
        );
        assert!(matches!(
            durable_i64("generation", i64::MAX as u64 + 1),
            Err(RegistryError::Persistence(message))
                if message.contains("generation") && message.contains("exceeds")
        ));
    }
}
