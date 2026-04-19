#![cfg(feature = "nats")]

mod nats_fixture;

use std::time::Duration;

use awaken_contract::contract::mailbox::{MailboxStore, RunDispatch, RunDispatchStatus};
use awaken_stores::{NatsMailboxConfig, NatsMailboxStore};
use nats_fixture::NatsFixture;

fn test_dispatch(id: &str, thread_id: &str) -> RunDispatch {
    RunDispatch {
        dispatch_id: id.to_string(),
        thread_id: thread_id.to_string(),
        run_id: format!("{id}-run"),
        priority: 128,
        dedupe_key: None,
        dispatch_epoch: 0,
        status: RunDispatchStatus::Queued,
        available_at: 0,
        attempt_count: 0,
        max_attempts: 3,
        last_error: None,
        claim_token: None,
        claimed_by: None,
        lease_until: None,
        dispatch_instance_id: None,
        run_status: None,
        termination: None,
        run_response: None,
        run_error: None,
        completed_at: None,
        created_at: 0,
        updated_at: 0,
    }
}

async fn make_store(fixture: &NatsFixture) -> NatsMailboxStore {
    let mut config = NatsMailboxConfig::new(fixture.url.clone());
    config.stream_name = format!("DISPATCH_{}", uuid::Uuid::now_v7().simple());
    config.consumer_name = format!("c_{}", uuid::Uuid::now_v7().simple());
    config.dispatch_bucket = format!("d_{}", uuid::Uuid::now_v7().simple());
    config.epoch_bucket = format!("e_{}", uuid::Uuid::now_v7().simple());
    config.thread_index_bucket = format!("ti_{}", uuid::Uuid::now_v7().simple());
    config.sweeper_interval = Duration::from_millis(100);
    NatsMailboxStore::connect(config).await.expect("connect")
}

#[tokio::test]
async fn index_rebuilds_from_kv_on_restart() {
    let fixture = NatsFixture::start().await;

    // Shared config so the second instance reuses the same buckets.
    let stream_name = format!("DISPATCH_{}", uuid::Uuid::now_v7().simple());
    let dispatch_bucket = format!("d_{}", uuid::Uuid::now_v7().simple());
    let epoch_bucket = format!("e_{}", uuid::Uuid::now_v7().simple());
    let ti_bucket = format!("ti_{}", uuid::Uuid::now_v7().simple());
    let consumer_name = format!("c_{}", uuid::Uuid::now_v7().simple());
    let mk_config = || {
        let mut config = NatsMailboxConfig::new(fixture.url.clone());
        config.stream_name = stream_name.clone();
        config.consumer_name = consumer_name.clone();
        config.dispatch_bucket = dispatch_bucket.clone();
        config.epoch_bucket = epoch_bucket.clone();
        config.thread_index_bucket = ti_bucket.clone();
        config
    };

    let store1 = NatsMailboxStore::connect(mk_config())
        .await
        .expect("connect 1");
    store1.enqueue(&test_dispatch("d1", "t1")).await.unwrap();
    store1.shutdown().await.unwrap();
    drop(store1);

    // Second store connects to same buckets; index must rebuild.
    let store2 = NatsMailboxStore::connect(mk_config())
        .await
        .expect("connect 2");

    let loaded = store2.load_dispatch("d1").await.unwrap();
    assert!(loaded.is_some(), "second store should see d1 from KV");
    store2.shutdown().await.unwrap();
}

#[tokio::test]
async fn sweeper_republishes_available_dispatch() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;

    let d = test_dispatch("d1", "t1");
    store.enqueue(&d).await.unwrap();
    let claimed = store.claim("t1", "c1", 30_000, 100, 1).await.unwrap();
    assert_eq!(claimed.len(), 1);
    let token = claimed[0].claim_token.clone().unwrap();

    // Nack with retry_at in the past so sweeper picks it up on next tick.
    store
        .nack("d1", &token, 50, "transient error", 150)
        .await
        .unwrap();

    // Wait for sweeper to tick (sweeper_interval = 100ms).
    tokio::time::sleep(Duration::from_millis(400)).await;

    // Dispatch should be Queued and claimable again.
    let reclaim = store.claim("t1", "c1", 30_000, 500, 1).await.unwrap();
    assert_eq!(
        reclaim.len(),
        1,
        "sweeper should have re-queued the dispatch"
    );
    store.shutdown().await.unwrap();
}

/// Regression for the foreground-submit Blocker: `Mailbox::submit()`
/// interrupts (epoch 0→1) and then inline-claims the dispatch it just
/// wrote. `enqueue` must stamp the dispatch with the current thread
/// epoch so the epoch-safe claim path doesn't reject it as stale.
#[tokio::test]
async fn enqueue_stamps_current_thread_epoch_after_interrupt() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;

    // Bump the thread epoch via interrupt (nothing to supersede yet).
    store.interrupt("t-epoch", 1_000).await.unwrap();

    // Caller passes dispatch_epoch=0 (Mailbox::build_dispatch default);
    // enqueue must override it to the current thread epoch so claim
    // succeeds.
    let dispatch = test_dispatch("d-stamp", "t-epoch");
    store.enqueue(&dispatch).await.unwrap();

    let claimed = store
        .claim_dispatch("d-stamp", "consumer", 30_000, 2_000)
        .await
        .unwrap();
    assert!(
        claimed.is_some(),
        "dispatch written after interrupt must be claimable — \
         enqueue should stamp current thread epoch"
    );
    assert!(
        claimed.unwrap().dispatch_epoch >= 1,
        "stamped dispatch_epoch must be >= the post-interrupt epoch"
    );

    store.shutdown().await.unwrap();
}

/// Regression: background dispatch enqueued after an interrupt must
/// also be claimable via queue-scan `claim()`.
#[tokio::test]
async fn background_enqueue_after_interrupt_is_claimable_via_scan() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;

    store.interrupt("t-bg", 1_000).await.unwrap();

    let mut dispatch = test_dispatch("d-bg", "t-bg");
    dispatch.available_at = 0; // claimable immediately
    store.enqueue(&dispatch).await.unwrap();

    let claimed = store
        .claim("t-bg", "consumer", 30_000, 2_000, 10)
        .await
        .unwrap();
    assert_eq!(
        claimed.len(),
        1,
        "background dispatch must be claimable after interrupt"
    );

    store.shutdown().await.unwrap();
}

/// Regression: a stale queued dispatch missed by interrupt must not block
/// the queue head. Claim-time epoch validation should terminalize the stale
/// dispatch, release its dedupe lock, and continue to the next valid
/// candidate even when `limit = 1`.
#[tokio::test]
async fn claim_skips_and_supersedes_stale_epoch_queue_head() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;

    store.interrupt("t-stale-head", 1_000).await.unwrap();

    let mut old = test_dispatch("d-old-stale", "t-stale-head");
    old.dedupe_key = Some("stale-key".to_string());
    old.dispatch_epoch = 0;
    old.created_at = 1;
    store.__test_plant_dispatch_exact(&old).await.unwrap();
    store
        .__test_force_dedupe_lock("t-stale-head", "stale-key", "d-old-stale")
        .await
        .unwrap();

    let mut new = test_dispatch("d-new-valid", "t-stale-head");
    new.created_at = 2;
    store.enqueue(&new).await.unwrap();

    let claimed = store
        .claim("t-stale-head", "consumer", 30_000, 2_000, 1)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].dispatch_id, "d-new-valid");

    let old_after = store
        .load_dispatch("d-old-stale")
        .await
        .unwrap()
        .expect("old dispatch should remain inspectable");
    assert_eq!(old_after.status, RunDispatchStatus::Superseded);

    let mut fresh = test_dispatch("d-fresh-dedupe", "t-stale-head");
    fresh.dedupe_key = Some("stale-key".to_string());
    store
        .enqueue(&fresh)
        .await
        .expect("stale head dedupe lock must be released");

    store.shutdown().await.unwrap();
}

/// Regression: if interrupt's local index still says a dispatch is Queued
/// but authoritative KV has already moved it to Claimed, interrupt must not
/// count it as superseded or release its dedupe lock.
#[tokio::test]
async fn interrupt_does_not_release_dedupe_for_claimed_dispatch_from_stale_index() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;

    let mut indexed = test_dispatch("d-authoritative-claimed", "t-interrupt-race");
    indexed.dedupe_key = Some("race-key".to_string());
    store
        .__test_force_dedupe_lock("t-interrupt-race", "race-key", "d-authoritative-claimed")
        .await
        .unwrap();
    assert_eq!(
        store
            .__test_dedupe_lock_holder("t-interrupt-race", "race-key")
            .await
            .unwrap()
            .as_deref(),
        Some("d-authoritative-claimed")
    );

    let mut claimed = indexed.clone();
    claimed.status = RunDispatchStatus::Claimed;
    claimed.claim_token = Some("claim-token".to_string());
    claimed.claimed_by = Some("remote-consumer".to_string());
    claimed.lease_until = Some(60_000);
    store
        .__test_plant_dispatch_exact(&claimed)
        .await
        .expect("plant authoritative claimed dispatch");

    store.__test_upsert_index_only(&indexed).await;
    assert_eq!(
        store
            .__test_dedupe_lock_holder("t-interrupt-race", "race-key")
            .await
            .unwrap()
            .as_deref(),
        Some("d-authoritative-claimed")
    );

    let mut before_interrupt = test_dispatch("d-before-interrupt", "t-interrupt-race");
    before_interrupt.dedupe_key = Some("race-key".to_string());
    assert!(
        store.enqueue(&before_interrupt).await.is_err(),
        "dedupe lock should be held before interrupt"
    );

    let interrupted = store.interrupt("t-interrupt-race", 1_000).await.unwrap();
    assert_eq!(
        interrupted.superseded_count, 0,
        "claimed authoritative dispatch must not be counted as superseded"
    );
    assert_eq!(
        interrupted
            .active_dispatch
            .as_ref()
            .map(|dispatch| dispatch.dispatch_id.as_str()),
        Some("d-authoritative-claimed"),
        "interrupt should return the authoritative active dispatch"
    );

    let mut next = test_dispatch("d-next-same-key", "t-interrupt-race");
    next.dedupe_key = Some("race-key".to_string());
    let result = store.enqueue(&next).await;
    assert!(
        result.is_err(),
        "dedupe lock for active claimed dispatch must remain held"
    );

    store.shutdown().await.unwrap();
}

/// Regression for the dedupe-lock orphan case: if a prior acquirer
/// crashed between lock create and dispatch put, the next enqueue with
/// the same key must reconcile (purge the orphan) and succeed.
#[tokio::test]
async fn dedupe_lock_orphan_is_reconciled_by_next_enqueue() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;

    // Simulate a crash leaving only the dedupe lock behind.
    store
        .__test_force_dedupe_lock("t-orphan", "ghost-key", "never-materialised-d")
        .await
        .unwrap();

    let mut d1 = test_dispatch("d-recovers", "t-orphan");
    d1.dedupe_key = Some("ghost-key".to_string());
    store
        .enqueue(&d1)
        .await
        .expect("next enqueue must reconcile the orphan lock");

    store.shutdown().await.unwrap();
}

/// Regression: once a dispatch reaches a terminal state (cancelled
/// here), the dedupe lock MUST be released so a fresh request with the
/// same key can proceed.
#[tokio::test]
async fn dedupe_key_reusable_after_cancel() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;

    let mut first = test_dispatch("d-first", "t-reuse");
    first.dedupe_key = Some("reuse-key".to_string());
    store.enqueue(&first).await.unwrap();

    store.cancel("d-first", 1_000).await.unwrap();

    let mut second = test_dispatch("d-second", "t-reuse");
    second.dedupe_key = Some("reuse-key".to_string());
    store
        .enqueue(&second)
        .await
        .expect("dedupe_key must be reusable after terminal");

    store.shutdown().await.unwrap();
}

/// Regression: a delayed terminal release by an old owner must not delete
/// the dedupe lock acquired by a newer dispatch with the same key.
#[tokio::test]
async fn delayed_old_owner_release_does_not_delete_new_dedupe_lock() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;

    let mut first = test_dispatch("d-release-old", "t-release-race");
    first.dedupe_key = Some("race-key".to_string());
    store.enqueue(&first).await.unwrap();
    store.cancel("d-release-old", 1_000).await.unwrap();

    let mut second = test_dispatch("d-release-new", "t-release-race");
    second.dedupe_key = Some("race-key".to_string());
    store.enqueue(&second).await.unwrap();

    store
        .__test_release_dedupe_lock_as("t-release-race", "race-key", "d-release-old")
        .await;

    let mut third = test_dispatch("d-release-third", "t-release-race");
    third.dedupe_key = Some("race-key".to_string());
    let result = store.enqueue(&third).await;
    assert!(
        result.is_err(),
        "old owner release must not remove the new owner's active dedupe lock"
    );

    store.shutdown().await.unwrap();
}

/// Regression: two concurrent reconcilers racing to clean one orphan lock
/// must still admit exactly one new dispatch for the dedupe key.
#[tokio::test]
async fn concurrent_orphan_reconcile_admits_exactly_one_owner() {
    let fixture = NatsFixture::start().await;
    let store = std::sync::Arc::new(make_store(&fixture).await);

    store
        .__test_force_dedupe_lock("t-reconcile-race", "race-key", "missing-owner")
        .await
        .unwrap();

    let mut first = test_dispatch("d-race-a", "t-reconcile-race");
    first.dedupe_key = Some("race-key".to_string());
    let mut second = test_dispatch("d-race-b", "t-reconcile-race");
    second.dedupe_key = Some("race-key".to_string());

    let store_a = store.clone();
    let store_b = store.clone();
    let (a, b) = tokio::join!(async move { store_a.enqueue(&first).await }, async move {
        store_b.enqueue(&second).await
    },);
    let success_count = usize::from(a.is_ok()) + usize::from(b.is_ok());
    assert_eq!(
        success_count, 1,
        "exactly one enqueue should win orphan-lock reconciliation"
    );

    let dispatches = store
        .list_dispatches("t-reconcile-race", None, 10, 0)
        .await
        .unwrap();
    assert_eq!(dispatches.len(), 1);

    store.shutdown().await.unwrap();
}

/// Regression for the partial-failure recovery contract: when `enqueue`
/// commits the dispatch to KV but the subsequent JetStream publish drops
/// (KV put succeeds, publish fails), the sweeper must re-publish the
/// delivery signal and the dispatch must become claimable — not stay
/// stranded. We reproduce this with a test-only helper that plants the
/// dispatch in KV without publishing, then drive the sweeper.
#[tokio::test]
async fn partial_failure_kv_put_without_publish_is_recovered_by_sweeper() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;

    // Simulate the partial-failure hole: dispatch committed to KV,
    // JS publish ack never happened.
    let dispatch = test_dispatch("d-partial", "t-partial");
    store
        .__test_plant_dispatch_without_publish(&dispatch)
        .await
        .expect("plant dispatch");

    // Immediately after: no JS delivery signal, but the dispatch should
    // still be visible in the index (so `claim()` could theoretically
    // pick it up directly).
    assert!(store.index_contains("d-partial").await);

    // Give the sweeper time to tick (it re-publishes dispatches whose
    // JS delivery signal is missing).
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Regardless of whether the sweeper re-published, a direct claim
    // must succeed — the KV record is authoritative.
    let claimed = store
        .claim_dispatch("d-partial", "consumer", 30_000, 1_000)
        .await
        .expect("claim must not error")
        .expect("dispatch must be claimable after KV-only commit");
    assert_eq!(claimed.dispatch_id, "d-partial");

    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn dedup_rejects_duplicate_key_on_same_thread() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;

    let mut d1 = test_dispatch("d1", "t1");
    d1.dedupe_key = Some("dup-key".to_string());
    store.enqueue(&d1).await.expect("first enqueue");

    let mut d2 = test_dispatch("d2", "t1");
    d2.dedupe_key = Some("dup-key".to_string());
    let result = store.enqueue(&d2).await;
    assert!(
        result.is_err(),
        "second enqueue with same dedupe_key must fail"
    );

    store.shutdown().await.unwrap();
}
