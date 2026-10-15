//! Trait-generic **conformance suite** for the Environment definition roles — the shared behavioural
//! contract every backend must satisfy, run against both the in-memory reference and the
//! SQLite backend (ADR-0059, the `awaken-store-conformance` pattern).
//!
//! It lifts the Phase-1 `proptest` invariants (which fuzz only the in-memory backend) to
//! the PORT: the durable sqlite registry must be observably identical to the reference on
//! the archive-vs-delete distinction and fail-closed lookups. A divergence — e.g. a
//! durable `delete` that soft-hides instead of removing, or an `archive` that drops a
//! record `get` should still return — is a real cross-deployment inconsistency, caught
//! here as a contract violation. Postgres joins behind its DB harness.
//!
//! The suite lives in `awaken_store_conformance::env_registry`; this file only
//! supplies the backends.

use awaken_env_store::{InMemoryEnvRegistry, SqliteEnvRegistry};
use awaken_store_conformance::env_registry::run_suite;

fn block<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        // The sqlx pool bounds connection admission with a Tokio timer.
        .enable_time()
        .build()
        .expect("current-thread runtime")
        .block_on(f)
}

#[test]
fn in_memory_backend_conforms() {
    block(run_suite(awaken_time::system_clock(), |clock| {
        std::future::ready(InMemoryEnvRegistry::new(clock))
    }));
}

#[test]
fn sqlite_backend_conforms() {
    block(run_suite(awaken_time::system_clock(), |clock| async move {
        SqliteEnvRegistry::open_in_memory(clock)
            .await
            .expect("sqlite in-memory registry")
    }));
}
