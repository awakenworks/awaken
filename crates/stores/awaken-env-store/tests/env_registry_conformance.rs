//! Trait-generic **conformance suite** for the `EnvRegistry` port — the shared behavioural
//! contract every backend must satisfy, run against both the in-memory reference and the
//! SQLite backend (ADR-0059, the `awaken-store-conformance` pattern).
//!
//! It lifts the Phase-1 `proptest` invariants (which fuzz only the in-memory backend) to
//! the PORT: the durable sqlite registry must be observably identical to the reference on
//! the archive-vs-delete distinction and fail-closed lookups. A divergence — e.g. a
//! durable `delete` that soft-hides instead of removing, or an `archive` that drops a
//! record `get` should still return — is a real cross-deployment inconsistency, caught
//! here as a contract violation. Postgres joins behind its DB harness.

use awaken_env_store::{InMemoryEnvRegistry, SqliteEnvRegistry};
use awaken_session_contract::env_registry::EnvRegistry;
use serde_json::json;

fn block<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime")
        .block_on(f)
}

async fn make<R: EnvRegistry>(r: &R, name: &str) -> String {
    r.create(name.into(), String::new(), Default::default(), json!({}))
        .await
        .id
}

// ── The port contract, trait-generic over any EnvRegistry backend ────────────────

/// Distinct ids across creates, all retrievable (no id reuse regardless of name).
async fn unique_ids<R: EnvRegistry>(r: &R) {
    let mut ids = Vec::new();
    for _ in 0..5 {
        ids.push(make(r, "same-name").await);
    }
    let unique: std::collections::BTreeSet<_> = ids.iter().collect();
    assert_eq!(unique.len(), 5, "ids collided");
    for id in &ids {
        assert!(r.exists(id).await, "a created id is missing");
    }
}

/// Archive is SOFT (record stays retrievable, leaves list_active); delete is HARD.
async fn archive_soft_delete_hard<R: EnvRegistry>(r: &R) {
    let a = make(r, "a").await;
    let d = make(r, "d").await;
    assert!(r.archive(&a).await.is_some());
    assert!(
        r.get(&a).await.is_some(),
        "archive must keep the record retrievable"
    );
    let active: Vec<String> = r.list_active().await.into_iter().map(|e| e.id).collect();
    assert!(
        !active.contains(&a),
        "archived record must leave list_active"
    );
    assert!(active.contains(&d), "a live record stays in list_active");
    assert!(r.delete(&d).await, "delete of an existing id reports true");
    assert!(r.get(&d).await.is_none(), "delete must remove the record");
}

/// Fail-closed: archive/update/delete on a never-created id → None/false.
async fn missing_id_fails_closed<R: EnvRegistry>(r: &R) {
    assert!(r.archive("env_missing").await.is_none());
    assert!(r.update("env_missing", Default::default()).await.is_none());
    assert!(!r.delete("env_missing").await);
    assert!(!r.exists("env_missing").await);
}

/// Delete is idempotent: the second delete reports false, nothing resurrects.
async fn delete_idempotent<R: EnvRegistry>(r: &R) {
    let id = make(r, "e").await;
    assert!(r.delete(&id).await, "first delete true");
    assert!(!r.delete(&id).await, "second delete false");
    assert!(r.get(&id).await.is_none());
}

async fn run_suite<R: EnvRegistry>(fresh: impl Fn() -> R) {
    unique_ids(&fresh()).await;
    archive_soft_delete_hard(&fresh()).await;
    missing_id_fails_closed(&fresh()).await;
    delete_idempotent(&fresh()).await;
}

// ── Backend rows: each must pass the identical suite ─────────────────────────────

#[test]
fn in_memory_backend_conforms() {
    block(run_suite(InMemoryEnvRegistry::new));
}

#[test]
fn sqlite_backend_conforms() {
    block(run_suite(|| {
        SqliteEnvRegistry::open_in_memory().expect("sqlite in-memory registry")
    }));
}
