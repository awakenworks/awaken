//! Trait-generic **conformance suite** for the `ManagedSessionRepository` port — the
//! shared behavioural contract every backend must satisfy, run against both the in-memory
//! reference and the SQLite backend (ADR-0059, the `awaken-store-conformance` pattern).
//!
//! Every save also persists an owner scope in the same repository operation. The generic
//! suite checks that universal invariant for both backends; durable restart persistence
//! remains in the backend-specific suite.

use awaken_session_contract::{ManagedSessionRepository, PersistedSession, SessionLifecycleFact};
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
        resources: awaken_session_contract::SessionResourceState::from_legacy(
            serde_json::from_value(json!({
                "inputs": [{
                    "binding_id": "input-file",
                    "source": { "kind": "file", "file_id": "file-1" },
                    "mount_path": "/mnt/input",
                    "access": "read_only"
                }]
            }))
            .unwrap(),
        ),
        status: "idle".into(),
        archived_at: None,
    }
}

fn fact(id: &str, session_id: &str, event_type: &str) -> SessionLifecycleFact {
    SessionLifecycleFact {
        id: id.into(),
        session_id: session_id.into(),
        workspace_id: Some("ws_a".into()),
        event_type: event_type.into(),
        timestamp: 1_700_000_000,
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

/// The lifecycle fact is committed in the same repository transaction as the
/// aggregate and owner. Notification may crash afterwards without losing the fact.
async fn lifecycle_outbox_tracks_every_committed_transition<R: ManagedSessionRepository>(r: &R) {
    let created = fact("evt:create", "sesn_lifecycle", "session.created");
    r.save_owned_with_lifecycle(
        "ws_a",
        session("sesn_lifecycle", "lifecycle"),
        created.clone(),
    )
    .await;
    assert!(r.get("sesn_lifecycle").await.is_some());
    assert_eq!(r.owner("sesn_lifecycle").await.as_deref(), Some("ws_a"));
    assert_eq!(r.pending_lifecycle().await, vec![created.clone()]);

    // Stable event identity makes an enqueue retry a no-op.
    r.append_lifecycle(created.clone()).await;
    assert_eq!(r.pending_lifecycle().await, vec![created.clone()]);
    r.complete_lifecycle(&created.id).await;
    assert!(r.pending_lifecycle().await.is_empty());

    let archived = fact("evt:archive", "sesn_lifecycle", "session.archived");
    r.archive_with_lifecycle("sesn_lifecycle", "2026-07-19T00:00:00Z", archived.clone())
        .await;
    let durable = r.get("sesn_lifecycle").await.expect("archived session");
    assert_eq!(durable.status, "terminated");
    assert_eq!(durable.archived_at.as_deref(), Some("2026-07-19T00:00:00Z"));
    assert_eq!(r.pending_lifecycle().await, vec![archived.clone()]);
    r.complete_lifecycle(&archived.id).await;

    let deleted = fact("evt:delete", "sesn_lifecycle", "session.deleted");
    r.delete_with_lifecycle("sesn_lifecycle", deleted.clone())
        .await;
    let durable = r.get("sesn_lifecycle").await.expect("delete tombstone");
    assert_eq!(durable.status, "deleted");
    assert_eq!(r.pending_lifecycle().await, vec![deleted]);
}

/// Prepared/Releasing activations remain discoverable after a process crash;
/// terminal Active/Released/Failed records do not create reconciliation work.
async fn pending_resource_activation_index_is_durable<R: ManagedSessionRepository>(r: &R) {
    let mut pending = session("sesn_pending", "pending");
    let desired = pending.resources.active.clone();
    pending.resources = Default::default();
    pending
        .resources
        .prepare(&pending.session_id, desired)
        .unwrap();
    r.save_owned("ws_a", pending.clone()).await;

    assert_eq!(r.pending_resource_sessions().await, vec![pending.clone()]);

    pending.resources.start_attempt().unwrap();
    pending.resources.commit().unwrap();
    r.save_owned("ws_a", pending).await;
    assert!(r.pending_resource_sessions().await.is_empty());
}

async fn run_suite<R: ManagedSessionRepository>(fresh: impl Fn() -> R) {
    save_get_round_trips(&fresh()).await;
    absent_id_reads_none(&fresh()).await;
    save_is_idempotent_upsert(&fresh()).await;
    save_owned_is_one_atomic_repository_fact(&fresh()).await;
    lifecycle_outbox_tracks_every_committed_transition(&fresh()).await;
    pending_resource_activation_index_is_durable(&fresh()).await;
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
