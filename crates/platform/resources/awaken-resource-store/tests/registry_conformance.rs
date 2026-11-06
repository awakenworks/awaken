#![cfg(feature = "sqlite")]

//! `ResourceRegistryRepository` conformance for the SQLite backend.
//!
//! The laws live in `awaken_store_conformance::resource_registry`; this file
//! only supplies the backend. PostgreSQL runs the same suite behind its DB
//! harness in `postgres_registry.rs`, so the two media cannot diverge on the
//! aggregate CAS, duplicate-insert classification, or workspace partitioning.

use awaken_resource_store::SqliteResourceStore;
use awaken_store_conformance::resource_registry::run_suite;

#[tokio::test]
async fn sqlite_in_memory_backend_conforms() {
    run_suite(|| async {
        SqliteResourceStore::open(":memory:")
            .await
            .expect("open Registry")
    })
    .await;
}

#[tokio::test]
async fn sqlite_file_backend_conforms_and_survives_reopen() {
    // The suite runs on a file-backed store; reopening the same path afterwards
    // must still show every aggregate the suite committed (medium durability is
    // the backend's law, so it stays here rather than in the shared suite).
    let directory = tempfile::tempdir().expect("Registry directory");
    let path = directory.path().join("registry.sqlite");
    run_suite(|| {
        let path = path.clone();
        async move {
            SqliteResourceStore::open(&path)
                .await
                .expect("open Registry")
        }
    })
    .await;
    let reopened = SqliteResourceStore::open(&path)
        .await
        .expect("reopen Registry");
    let listed =
        awaken_resource_contract::ResourceRegistryRepository::list_memory_stores(&reopened, "ws-a")
            .await
            .expect("durable list");
    assert_eq!(
        listed.len(),
        2,
        "reopen shows the suite's committed aggregates"
    );
}
