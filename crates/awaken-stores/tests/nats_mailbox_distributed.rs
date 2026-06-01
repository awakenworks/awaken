#![cfg(feature = "nats")]

#[path = "nats_fixture.rs"]
mod nats_fixture;

use std::sync::Arc;
use std::time::Duration;

use awaken_server_contract::contract::mailbox::{MailboxStore, RunDispatch, RunDispatchStatus};
use awaken_stores::{NatsMailboxConfig, NatsMailboxStore};
use nats_fixture::NatsFixture;

fn shared_config(fixture: &NatsFixture) -> (String, NatsMailboxConfig) {
    let suffix = uuid::Uuid::now_v7().simple().to_string();
    let stream = format!("DISPATCH_{suffix}");
    let consumer = format!("c_{suffix}");
    let dispatch_b = format!("d_{suffix}");
    let epoch_b = format!("e_{suffix}");
    let ti_b = format!("ti_{suffix}");
    let mut cfg = NatsMailboxConfig::new(fixture.url.clone());
    cfg.stream_name = stream.clone();
    cfg.consumer_name = consumer;
    cfg.dispatch_bucket = dispatch_b;
    cfg.epoch_bucket = epoch_b;
    cfg.thread_index_bucket = ti_b;
    (suffix, cfg)
}

fn test_dispatch(id: &str, thread_id: &str) -> RunDispatch {
    RunDispatch::queued(
        id.to_string(),
        thread_id.to_string(),
        format!("{id}-run"),
        0,
    )
    .with_max_attempts(3)
}

async fn wait_for_index(store: &NatsMailboxStore, dispatch_id: &str, timeout_ms: u64) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_millis(timeout_ms) {
        if store.load_dispatch(dispatch_id).await.unwrap().is_some() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// Two instances competing to claim the same dispatch — only one wins.
#[tokio::test]
async fn concurrent_claim_same_dispatch_only_one_wins() {
    let fixture = NatsFixture::start().await;
    let (_, cfg) = shared_config(&fixture);

    let store_a = Arc::new(NatsMailboxStore::connect(cfg.clone()).await.expect("a"));
    let store_b = Arc::new(NatsMailboxStore::connect(cfg).await.expect("b"));

    // Enqueue via A, wait for B's index to see it.
    store_a.enqueue(&test_dispatch("d1", "t1")).await.unwrap();
    assert!(wait_for_index(&store_b, "d1", 2_000).await);

    // Both try to claim concurrently.
    let a = Arc::clone(&store_a);
    let b = Arc::clone(&store_b);
    let (r_a, r_b) = tokio::join!(
        async move {
            a.claim_dispatch("d1", "consumer-a", 30_000, 1000)
                .await
                .unwrap()
        },
        async move {
            b.claim_dispatch("d1", "consumer-b", 30_000, 1000)
                .await
                .unwrap()
        },
    );

    let won_a = r_a.is_some();
    let won_b = r_b.is_some();
    assert!(
        won_a ^ won_b,
        "exactly one should win (a={won_a}, b={won_b})"
    );

    // Verify winner's claim_token is unique.
    let winner_token = match (r_a, r_b) {
        (Some(d), None) | (None, Some(d)) => d.claim_token().expect("claim_token set").to_string(),
        _ => unreachable!("exactly one winner verified above"),
    };
    assert!(!winner_token.is_empty());

    store_a.shutdown().await.unwrap();
    store_b.shutdown().await.unwrap();
}

/// Two instances competing to claim different dispatches on the same thread:
/// the per-thread guard must still allow only one active claim globally.
#[tokio::test]
async fn concurrent_claim_different_dispatches_same_thread_only_one_wins() {
    let fixture = NatsFixture::start().await;
    let (_, cfg) = shared_config(&fixture);

    let store_a = Arc::new(NatsMailboxStore::connect(cfg.clone()).await.expect("a"));
    let store_b = Arc::new(NatsMailboxStore::connect(cfg).await.expect("b"));

    store_a.enqueue(&test_dispatch("d1", "t1")).await.unwrap();
    store_a.enqueue(&test_dispatch("d2", "t1")).await.unwrap();
    assert!(wait_for_index(&store_b, "d1", 2_000).await);
    assert!(wait_for_index(&store_b, "d2", 2_000).await);

    let a = Arc::clone(&store_a);
    let b = Arc::clone(&store_b);
    let (r_a, r_b) = tokio::join!(
        async move {
            a.claim_dispatch("d1", "consumer-a", 30_000, 1000)
                .await
                .unwrap()
        },
        async move {
            b.claim_dispatch("d2", "consumer-b", 30_000, 1000)
                .await
                .unwrap()
        },
    );

    let won_a = r_a.is_some();
    let won_b = r_b.is_some();
    assert!(
        won_a ^ won_b,
        "same-thread claims must be globally exclusive (a={won_a}, b={won_b})"
    );

    store_a.shutdown().await.unwrap();
    store_b.shutdown().await.unwrap();
}

/// Instance A crashes holding a claim; Instance B reclaims after lease expiry.
#[tokio::test]
async fn expired_lease_reclaimable_by_other_instance() {
    let fixture = NatsFixture::start().await;
    let (_, cfg) = shared_config(&fixture);

    let store_a = NatsMailboxStore::connect(cfg.clone()).await.expect("a");
    store_a.enqueue(&test_dispatch("d1", "t1")).await.unwrap();

    // A claims with 100ms lease.
    let claimed = store_a
        .claim_dispatch("d1", "consumer-a", 100, 1000)
        .await
        .unwrap();
    assert!(claimed.is_some());

    // A "crashes" — drop without ack.
    drop(store_a);

    // B connects, waits for lease to expire, reclaims.
    let store_b = NatsMailboxStore::connect(cfg).await.expect("b");
    assert!(wait_for_index(&store_b, "d1", 2_000).await);

    // After lease expiry (100ms + guard), reclaim should succeed.
    let now_after_expiry = 1000 + 1000; // 1 sec later > 100ms lease
    let reclaimed = store_b
        .reclaim_expired_leases(now_after_expiry, 10)
        .await
        .unwrap();
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].dispatch_id(), "d1");

    // Now B can claim the re-queued dispatch.
    let b_claim = store_b
        .claim_dispatch("d1", "consumer-b", 30_000, now_after_expiry + 1)
        .await
        .unwrap();
    assert!(b_claim.is_some());
    assert_eq!(b_claim.unwrap().claimed_by(), Some("consumer-b"));

    store_b.shutdown().await.unwrap();
}

/// A stale owner from one instance must not be able to ack after another
/// instance reclaims and owns the dispatch.
#[tokio::test]
async fn late_ack_after_cross_instance_reclaim_is_rejected() {
    let fixture = NatsFixture::start().await;
    let (_, cfg) = shared_config(&fixture);

    let store_a = NatsMailboxStore::connect(cfg.clone()).await.expect("a");
    let store_b = NatsMailboxStore::connect(cfg).await.expect("b");
    store_a
        .enqueue(&test_dispatch("d-late-ack", "t-late-ack"))
        .await
        .unwrap();
    assert!(wait_for_index(&store_b, "d-late-ack", 2_000).await);

    let claimed_a = store_a
        .claim_dispatch("d-late-ack", "consumer-a", 100, 1_000)
        .await
        .unwrap()
        .expect("a owns dispatch");
    let token_a = claimed_a.claim_token().expect("a claim token").to_string();

    let reclaimed = store_b.reclaim_expired_leases(1_101, 10).await.unwrap();
    assert_eq!(reclaimed.len(), 1);
    let claimed_b = store_b
        .claim_dispatch("d-late-ack", "consumer-b", 30_000, 1_102)
        .await
        .unwrap()
        .expect("b owns dispatch");
    let token_b = claimed_b.claim_token().expect("b claim token").to_string();

    assert!(
        store_a.ack("d-late-ack", &token_a, 1_103).await.is_err(),
        "old owner ack must not clear a newer cross-instance claim"
    );
    let still_claimed = store_b
        .load_dispatch("d-late-ack")
        .await
        .unwrap()
        .expect("dispatch");
    assert_eq!(still_claimed.status(), RunDispatchStatus::Claimed);
    assert_eq!(still_claimed.claimed_by(), Some("consumer-b"));

    store_b.ack("d-late-ack", &token_b, 1_104).await.unwrap();
    store_a.shutdown().await.unwrap();
    store_b.shutdown().await.unwrap();
}

/// Lease reclaim must tolerate clock skew: a backward or exact-boundary clock
/// cannot steal a live lease, but a later clock can.
#[tokio::test]
async fn cross_instance_lease_reclaim_uses_strict_expiry_boundary() {
    let fixture = NatsFixture::start().await;
    let (_, cfg) = shared_config(&fixture);

    let store_a = NatsMailboxStore::connect(cfg.clone()).await.expect("a");
    let store_b = NatsMailboxStore::connect(cfg).await.expect("b");
    store_a
        .enqueue(&test_dispatch("d-lease-boundary", "t-lease-boundary"))
        .await
        .unwrap();
    assert!(wait_for_index(&store_b, "d-lease-boundary", 2_000).await);

    store_a
        .claim_dispatch("d-lease-boundary", "consumer-a", 100, 1_000)
        .await
        .unwrap()
        .expect("a owns dispatch");

    assert!(
        store_b
            .reclaim_expired_leases(999, 10)
            .await
            .unwrap()
            .is_empty(),
        "backward-skewed clock must not reclaim"
    );
    assert!(
        store_b
            .reclaim_expired_leases(1_100, 10)
            .await
            .unwrap()
            .is_empty(),
        "exact lease_until boundary must not reclaim"
    );
    let reclaimed = store_b.reclaim_expired_leases(1_101, 10).await.unwrap();
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].status(), RunDispatchStatus::Queued);

    store_a.shutdown().await.unwrap();
    store_b.shutdown().await.unwrap();
}

/// Retry scheduling must not be bypassed by another instance claiming by
/// thread before `retry_at`.
#[tokio::test]
async fn cross_instance_nack_retry_window_respects_retry_at() {
    let fixture = NatsFixture::start().await;
    let (_, cfg) = shared_config(&fixture);

    let store_a = NatsMailboxStore::connect(cfg.clone()).await.expect("a");
    let store_b = NatsMailboxStore::connect(cfg).await.expect("b");
    store_a
        .enqueue(&test_dispatch("d-retry-boundary", "t-retry-boundary"))
        .await
        .unwrap();
    assert!(wait_for_index(&store_b, "d-retry-boundary", 2_000).await);

    let claimed = store_a
        .claim("t-retry-boundary", "consumer-a", 30_000, 1_000, 1)
        .await
        .unwrap();
    let token = claimed[0].claim_token().expect("claim token").to_string();
    store_a
        .nack("d-retry-boundary", &token, 2_000, "retry later", 1_001)
        .await
        .unwrap();

    assert!(
        store_b
            .claim("t-retry-boundary", "consumer-b", 30_000, 1_999, 1)
            .await
            .unwrap()
            .is_empty(),
        "dispatch must not be claimable before retry_at"
    );
    let claimed_at_retry = store_b
        .claim("t-retry-boundary", "consumer-b", 30_000, 2_000, 1)
        .await
        .unwrap();
    assert_eq!(claimed_at_retry.len(), 1);
    assert_eq!(claimed_at_retry[0].dispatch_id(), "d-retry-boundary");

    store_a.shutdown().await.unwrap();
    store_b.shutdown().await.unwrap();
}

/// Interrupt from one instance is seen by another's index via KV watch.
#[tokio::test]
async fn interrupt_supersedes_across_instances() {
    let fixture = NatsFixture::start().await;
    let (_, cfg) = shared_config(&fixture);

    let store_a = NatsMailboxStore::connect(cfg.clone()).await.expect("a");
    let store_b = NatsMailboxStore::connect(cfg).await.expect("b");

    // A enqueues 3 dispatches on t1.
    for i in 0..3 {
        store_a
            .enqueue(&test_dispatch(&format!("d{i}"), "t1"))
            .await
            .unwrap();
    }

    // Wait for B to see them in its index.
    assert!(wait_for_index(&store_b, "d2", 2_000).await);

    // B interrupts t1.
    let result = store_b.interrupt("t1", 2000).await.unwrap();
    assert_eq!(result.superseded_count, 3);

    // A should eventually see all 3 as Superseded via watcher.
    let start = std::time::Instant::now();
    let mut all_superseded = false;
    while start.elapsed() < Duration::from_secs(3) {
        let queued = store_a
            .list_dispatches("t1", Some(&[RunDispatchStatus::Queued]), 10, 0)
            .await
            .unwrap();
        if queued.is_empty() {
            all_superseded = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(all_superseded, "A's index should see superseded dispatches");

    store_a.shutdown().await.unwrap();
    store_b.shutdown().await.unwrap();
}

/// Regression for issue #3: two instances concurrently enqueueing the same
/// `(thread_id, dedupe_key)` must produce exactly ONE accepted dispatch —
/// the local-index dedupe check was racy; the KV-backed dedupe lock makes
/// it authoritative.
#[tokio::test]
async fn concurrent_enqueue_same_dedupe_key_only_one_wins() {
    let fixture = NatsFixture::start().await;
    let (_, cfg) = shared_config(&fixture);

    let store_a = Arc::new(NatsMailboxStore::connect(cfg.clone()).await.expect("a"));
    let store_b = Arc::new(NatsMailboxStore::connect(cfg).await.expect("b"));

    let d1 = test_dispatch("dedupe-a", "t-dedupe-race").with_dedupe_key(Some("same-key".into()));
    let d2 = test_dispatch("dedupe-b", "t-dedupe-race").with_dedupe_key(Some("same-key".into()));

    let a = Arc::clone(&store_a);
    let b = Arc::clone(&store_b);
    let (r_a, r_b) = tokio::join!(async move { a.enqueue(&d1).await }, async move {
        b.enqueue(&d2).await
    },);
    let won_a = r_a.is_ok();
    let won_b = r_b.is_ok();
    assert!(
        won_a ^ won_b,
        "exactly one enqueue must succeed for a given dedupe_key (a={won_a}, b={won_b})"
    );

    store_a.shutdown().await.unwrap();
    store_b.shutdown().await.unwrap();
}

/// queued_thread_ids converges across instances.
#[tokio::test]
async fn queued_thread_ids_converges_across_instances() {
    let fixture = NatsFixture::start().await;
    let (_, cfg) = shared_config(&fixture);

    let store_a = NatsMailboxStore::connect(cfg.clone()).await.expect("a");
    let store_b = NatsMailboxStore::connect(cfg).await.expect("b");

    store_a.enqueue(&test_dispatch("d1", "t1")).await.unwrap();
    store_b.enqueue(&test_dispatch("d2", "t2")).await.unwrap();

    // Wait for both indices to converge.
    let start = std::time::Instant::now();
    let mut converged = false;
    while start.elapsed() < Duration::from_secs(3) {
        let ids_a = store_a.queued_thread_ids().await.unwrap();
        let ids_b = store_b.queued_thread_ids().await.unwrap();
        if ids_a.len() == 2 && ids_b.len() == 2 {
            converged = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(converged, "both instances should see both threads");

    store_a.shutdown().await.unwrap();
    store_b.shutdown().await.unwrap();
}
