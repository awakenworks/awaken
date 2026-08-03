//! Trait-generic **conformance suite** for the `WorkQueue` port — the shared behavioural
//! contract EVERY backend must satisfy, run here against both the in-memory reference and
//! the SQLite backend (ADR-0059 / ADR-0039-slice-2.6 pattern, mirroring
//! `awaken-store-conformance`).
//!
//! Why this exists and what it drives: the Phase-1 `proptest` properties fuzz the *one*
//! in-memory backend. This suite LIFTS the same invariants to the PORT: it asserts the
//! sqlite backend is observably identical to the reference on the safety-critical
//! semantics — the single-active-lease cap, reclaim-at-TTL,
//! environment isolation, stop-frees-next, and remove_env-purges. A divergence here is a
//! real parity bug (the kind that ships a run twice on one deployment mode but not
//! another); making it a checked contract forces the backends to agree by construction.
//! Postgres joins the same suite behind its DB harness (`pg_tests.sh`).

use awaken_session_contract::work_queue::{
    HeartbeatCondition, HeartbeatResult, LeaseHeartbeat, WorkQueue, WorkState,
};
use awaken_work_store::{InMemoryWorkQueue, LEASE_TTL_MS, SqliteWorkQueue};

fn block<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime")
        .block_on(f)
}

// ── The port contract, trait-generic over any WorkQueue backend ──────────────────

/// The single-active-lease cap: with items queued, repeated claims at one instant hand
/// out exactly ONE lease, and exactly one item is Active.
async fn single_active_cap<Q: WorkQueue>(q: &Q) {
    for i in 0..5 {
        q.enqueue_session("env", &format!("s{i}"))
            .await
            .expect("enqueue");
    }
    let handed = {
        let mut n = 0;
        for _ in 0..4 {
            if q.claim("env", "w", 0).await.expect("claim").is_some() {
                n += 1;
            }
        }
        n
    };
    assert_eq!(handed, 1, "more than one lease handed out at once");
    let active = q
        .list("env")
        .await
        .expect("list")
        .into_iter()
        .filter(|w| w.state == WorkState::Active)
        .count();
    assert_eq!(active, 1, "exactly one item must be Active");
}

/// Reclaim is exact at the TTL boundary: a live lease caps the env until `now == expiry`,
/// then the lapsed item is reclaimable.
async fn reclaim_at_ttl<Q: WorkQueue>(q: &Q) {
    q.enqueue_session("env", "s0").await.expect("enqueue");
    assert!(
        q.claim("env", "a", 0).await.expect("claim").is_some(),
        "first claim leases"
    );
    assert!(
        q.claim("env", "b", LEASE_TTL_MS - 1)
            .await
            .expect("claim")
            .is_none(),
        "held until the last ms before expiry"
    );
    assert!(
        q.claim("env", "b", LEASE_TTL_MS)
            .await
            .expect("claim")
            .is_some(),
        "reclaimable at exactly the TTL boundary"
    );
}

/// Requested reclaim age is measured from the last lease refresh, independently
/// of the TTL selected by that heartbeat.
async fn requested_reclaim_uses_refresh_clock<Q: WorkQueue>(q: &Q) {
    let id = q.enqueue_session("env", "s0").await.expect("enqueue");
    q.claim("env", "worker-a", 0)
        .await
        .expect("claim query")
        .expect("initial claim");
    assert!(matches!(
        q.heartbeat(
            "env",
            &id,
            "worker-a",
            1_000,
            LeaseHeartbeat {
                condition: HeartbeatCondition::First,
                desired_ttl_seconds: Some(120),
            },
        )
        .await
        .expect("heartbeat"),
        HeartbeatResult::Accepted(_)
    ));
    assert!(
        q.claim_with_reclaim("env", "worker-b", 1_001, Some(2))
            .await
            .expect("claim")
            .is_none(),
        "a one-millisecond-old refresh is younger than the requested age"
    );
    assert!(
        q.claim_with_reclaim("env", "worker-b", 1_002, Some(2))
            .await
            .expect("claim")
            .is_some(),
        "reclaim age is exact even when the live lease has a 120-second ttl"
    );
}

/// Environments lease independently: a claim in one never touches another's cap.
async fn env_isolation<Q: WorkQueue>(q: &Q) {
    q.enqueue_session("env_a", "a0").await.expect("enqueue a");
    q.enqueue_session("env_b", "b0").await.expect("enqueue b");
    let la = q
        .claim("env_a", "w", 0)
        .await
        .expect("claim query")
        .expect("env_a leases");
    let lb = q
        .claim("env_b", "w", 0)
        .await
        .expect("claim query")
        .expect("env_b leases independently");
    assert_eq!(
        q.get("env_a", &la.id)
            .await
            .expect("get")
            .map(|w| w.environment_id),
        Some("env_a".into())
    );
    assert_eq!(
        q.get("env_b", &lb.id)
            .await
            .expect("get")
            .map(|w| w.environment_id),
        Some("env_b".into())
    );
}

/// Stopping the active item frees the next, and no item is lost or duplicated.
async fn stop_frees_next<Q: WorkQueue>(q: &Q) {
    for i in 0..3 {
        q.enqueue_session("env", &format!("s{i}"))
            .await
            .expect("enqueue");
    }
    let first = q
        .claim("env", "w", 0)
        .await
        .expect("claim query")
        .expect("first");
    assert!(
        q.claim("env", "w", 0).await.expect("claim").is_none(),
        "capped while active"
    );
    q.stop("env", &first.id).await.expect("stop");
    assert!(
        q.claim("env", "w", 0).await.expect("claim").is_some(),
        "next claimable after stop"
    );
    assert_eq!(
        q.list("env").await.expect("list").len(),
        3,
        "item count conserved"
    );
}

/// remove_env purges everything: no items, nothing to claim.
async fn remove_env_purges<Q: WorkQueue>(q: &Q) {
    for i in 0..3 {
        q.enqueue_session("env", &format!("s{i}"))
            .await
            .expect("enqueue");
    }
    q.remove_env("env").await.unwrap();
    assert!(
        q.list("env").await.expect("list").is_empty(),
        "list empty after remove_env"
    );
    assert!(
        q.claim("env", "w", 0).await.expect("claim").is_none(),
        "nothing to claim after remove_env"
    );
}

/// The official heartbeat compare token is an atomic CAS: first succeeds once,
/// the returned token advances monotonically, and a stale token cannot extend
/// the lease after a newer heartbeat has committed.
async fn heartbeat_compare_and_extend<Q: WorkQueue>(q: &Q) {
    let id = q.enqueue_session("env", "s0").await.expect("enqueue");
    q.claim("env", "worker", 0)
        .await
        .expect("claim query")
        .expect("claim");

    assert!(matches!(
        q.heartbeat(
            "env",
            &id,
            "other-worker",
            1,
            LeaseHeartbeat {
                condition: HeartbeatCondition::First,
                desired_ttl_seconds: None,
            },
        )
        .await
        .expect("heartbeat"),
        HeartbeatResult::PreconditionFailed
    ));

    let first = match q
        .heartbeat(
            "env",
            &id,
            "worker",
            1,
            LeaseHeartbeat {
                condition: HeartbeatCondition::First,
                desired_ttl_seconds: Some(7),
            },
        )
        .await
        .expect("heartbeat")
    {
        HeartbeatResult::Accepted(receipt) => receipt,
        other => panic!("first heartbeat rejected: {other:?}"),
    };
    assert!(first.lease_extended);
    assert_eq!(first.ttl_seconds, 7);

    assert!(matches!(
        q.heartbeat(
            "env",
            &id,
            "worker",
            2,
            LeaseHeartbeat {
                condition: HeartbeatCondition::First,
                desired_ttl_seconds: None,
            },
        )
        .await
        .expect("heartbeat"),
        HeartbeatResult::PreconditionFailed
    ));

    let second = q
        .heartbeat(
            "env",
            &id,
            "worker",
            2,
            LeaseHeartbeat {
                condition: HeartbeatCondition::Matching(first.last_heartbeat.clone()),
                desired_ttl_seconds: None,
            },
        )
        .await
        .expect("heartbeat")
        .into_receipt()
        .expect("matching heartbeat");
    assert_ne!(second.last_heartbeat, first.last_heartbeat);

    assert!(matches!(
        q.heartbeat(
            "env",
            &id,
            "worker",
            3,
            LeaseHeartbeat {
                condition: HeartbeatCondition::Matching(first.last_heartbeat),
                desired_ttl_seconds: None,
            },
        )
        .await
        .expect("heartbeat"),
        HeartbeatResult::PreconditionFailed
    ));
}

/// Run the whole contract against a freshly-built backend `Q`.
async fn run_suite<Q: WorkQueue>(fresh: impl Fn() -> Q) {
    single_active_cap(&fresh()).await;
    reclaim_at_ttl(&fresh()).await;
    requested_reclaim_uses_refresh_clock(&fresh()).await;
    env_isolation(&fresh()).await;
    stop_frees_next(&fresh()).await;
    remove_env_purges(&fresh()).await;
    heartbeat_compare_and_extend(&fresh()).await;
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
