//! Trait-generic **conformance suite** for the `ManagedSessionRepository` port — the
//! shared behavioural contract every backend must satisfy, run against both the in-memory
//! reference and the SQLite backend (ADR-0059, the `awaken-store-conformance` pattern).
//!
//! Every save also persists an owner scope in the same repository operation. The generic
//! suite checks that universal invariant for both backends; durable restart persistence
//! remains in the backend-specific suite.

use awaken_session_contract::{ManagedSessionRepository, PersistedSession};
use awaken_session_store::{InMemorySessionRepository, SqliteManagedSessionRepository};
use serde_json::json;

fn block<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime")
        .block_on(f)
}

fn session(id: &str, title: &str) -> PersistedSession {
    PersistedSession {
        session_id: id.to_string(),
        agent_id: "assistant".into(),
        model: "kimi".into(),
        title: Some(title.to_string()),
        metadata: std::collections::BTreeMap::from([("k".into(), "v".into())]),
        environment_id: "env".into(),
        mcp_servers: vec![json!({ "name": "fs", "type": "stdio", "url": "x" })],
    }
}

// ── The universal port contract, trait-generic over any backend ──────────────────

/// Round-trip: a saved aggregate reads back byte-for-byte (every field persists).
async fn save_get_round_trips<R: ManagedSessionRepository>(r: &R) {
    let want = session("sesn_1", "hello");
    r.save(want.clone()).await;
    assert_eq!(
        r.get("sesn_1").await,
        Some(want),
        "the full aggregate must round-trip"
    );
}

/// Absent id → None (no fabrication, fail-closed read).
async fn absent_id_reads_none<R: ManagedSessionRepository>(r: &R) {
    assert!(r.get("never-saved").await.is_none());
}

/// Idempotent upsert: saving the same id twice keeps the latest, not two rows.
async fn save_is_idempotent_upsert<R: ManagedSessionRepository>(r: &R) {
    r.save(session("sesn_1", "first")).await;
    r.save(session("sesn_1", "second")).await;
    assert_eq!(
        r.get("sesn_1").await.and_then(|s| s.title),
        Some("second".into())
    );
}

/// A visible row and its owner are one write: no backend may expose the row with
/// a missing or stale scope after `save_owned` returns.
async fn save_owned_is_one_atomic_repository_fact<R: ManagedSessionRepository>(r: &R) {
    r.save_owned("ws_a", session("sesn_owned", "owned")).await;
    assert!(r.get("sesn_owned").await.is_some());
    assert_eq!(r.owner("sesn_owned").await.as_deref(), Some("ws_a"));

    r.save_owned("ws_b", session("sesn_owned", "moved")).await;
    assert_eq!(r.owner("sesn_owned").await.as_deref(), Some("ws_b"));
    assert_eq!(
        r.get("sesn_owned").await.and_then(|s| s.title),
        Some("moved".into())
    );
}

async fn run_suite<R: ManagedSessionRepository>(fresh: impl Fn() -> R) {
    save_get_round_trips(&fresh()).await;
    absent_id_reads_none(&fresh()).await;
    save_is_idempotent_upsert(&fresh()).await;
    save_owned_is_one_atomic_repository_fact(&fresh()).await;
}

// ── Backend rows: each must pass the identical universal suite ───────────────────

#[test]
fn in_memory_backend_conforms() {
    block(run_suite(InMemorySessionRepository::default));
}

#[test]
fn sqlite_backend_conforms() {
    block(run_suite(|| {
        SqliteManagedSessionRepository::open_in_memory().expect("sqlite in-memory repo")
    }));
}
