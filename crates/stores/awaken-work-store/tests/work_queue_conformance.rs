//! Trait-generic **conformance suite** for the `WorkQueue` port — the shared behavioural
//! contract EVERY backend must satisfy, run here against both the in-memory reference and
//! the SQLite backend (ADR-0059 / ADR-0039-slice-2.6 pattern, mirroring
//! `awaken-store-conformance`).
//!
//! Why this exists and what it drives: the Phase-1 `proptest` properties fuzz the *one*
//! in-memory backend. This suite LIFTS the same invariants to the PORT: it asserts the
//! sqlite backend is observably identical to the reference on the safety-critical
//! semantics — the single-active-lease cap (exactly-once dispatch), reclaim-at-TTL,
//! environment isolation, stop-frees-next, and remove_env-purges. A divergence here is a
//! real parity bug (the kind that ships a run twice on one deployment mode but not
//! another); making it a checked contract forces the backends to agree by construction.
//! Postgres joins the same suite behind its DB harness (`pg_tests.sh`).

use awaken_session_contract::work_queue::{WorkQueue, WorkState};
use awaken_work_store::{InMemoryWorkQueue, LEASE_TTL_MS, SqliteWorkQueue};

fn block<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime")
        .block_on(f)
}

// ── The port contract, trait-generic over any WorkQueue backend ──────────────────

/// The single-active-lease cap: with items queued, repeated claims at one instant hand
/// out exactly ONE lease, and exactly one item is Active. (Exactly-once dispatch.)
async fn single_active_cap<Q: WorkQueue>(q: &Q) {
    for i in 0..5 {
        q.enqueue_session("env", &format!("s{i}")).await;
    }
    let handed = {
        let mut n = 0;
        for _ in 0..4 {
            if q.claim("env", "w", 0).await.is_some() {
                n += 1;
            }
        }
        n
    };
    assert_eq!(handed, 1, "more than one lease handed out at once");
    let active = q
        .list("env")
        .await
        .into_iter()
        .filter(|w| w.state == WorkState::Active)
        .count();
    assert_eq!(active, 1, "exactly one item must be Active");
}

/// Reclaim is exact at the TTL boundary: a live lease caps the env until `now == expiry`,
/// then the lapsed item is reclaimable.
async fn reclaim_at_ttl<Q: WorkQueue>(q: &Q) {
    q.enqueue_session("env", "s0").await;
    assert!(q.claim("env", "a", 0).await.is_some(), "first claim leases");
    assert!(
        q.claim("env", "b", LEASE_TTL_MS - 1).await.is_none(),
        "held until the last ms before expiry"
    );
    assert!(
        q.claim("env", "b", LEASE_TTL_MS).await.is_some(),
        "reclaimable at exactly the TTL boundary"
    );
}

/// Environments lease independently: a claim in one never touches another's cap.
async fn env_isolation<Q: WorkQueue>(q: &Q) {
    q.enqueue_session("env_a", "a0").await;
    q.enqueue_session("env_b", "b0").await;
    let la = q.claim("env_a", "w", 0).await.expect("env_a leases");
    let lb = q
        .claim("env_b", "w", 0)
        .await
        .expect("env_b leases independently");
    assert_eq!(
        q.get("env_a", &la.id).await.map(|w| w.environment_id),
        Some("env_a".into())
    );
    assert_eq!(
        q.get("env_b", &lb.id).await.map(|w| w.environment_id),
        Some("env_b".into())
    );
}

/// Stopping the active item frees the next, and no item is lost or duplicated.
async fn stop_frees_next<Q: WorkQueue>(q: &Q) {
    for i in 0..3 {
        q.enqueue_session("env", &format!("s{i}")).await;
    }
    let first = q.claim("env", "w", 0).await.expect("first");
    assert!(
        q.claim("env", "w", 0).await.is_none(),
        "capped while active"
    );
    q.stop("env", &first.id).await;
    assert!(
        q.claim("env", "w", 0).await.is_some(),
        "next claimable after stop"
    );
    assert_eq!(q.list("env").await.len(), 3, "item count conserved");
}

/// remove_env purges everything: no items, nothing to claim.
async fn remove_env_purges<Q: WorkQueue>(q: &Q) {
    for i in 0..3 {
        q.enqueue_session("env", &format!("s{i}")).await;
    }
    q.remove_env("env").await;
    assert!(
        q.list("env").await.is_empty(),
        "list empty after remove_env"
    );
    assert!(
        q.claim("env", "w", 0).await.is_none(),
        "nothing to claim after remove_env"
    );
}

/// Run the whole contract against a freshly-built backend `Q`.
async fn run_suite<Q: WorkQueue>(fresh: impl Fn() -> Q) {
    single_active_cap(&fresh()).await;
    reclaim_at_ttl(&fresh()).await;
    env_isolation(&fresh()).await;
    stop_frees_next(&fresh()).await;
    remove_env_purges(&fresh()).await;
}

// ── Backend rows: each must pass the identical suite ─────────────────────────────

#[test]
fn in_memory_backend_conforms() {
    block(run_suite(InMemoryWorkQueue::new));
}

#[test]
fn sqlite_backend_conforms() {
    block(run_suite(|| {
        SqliteWorkQueue::open_in_memory().expect("sqlite in-memory queue")
    }));
}
