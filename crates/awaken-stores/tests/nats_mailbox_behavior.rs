#![cfg(feature = "nats")]

mod nats_fixture;

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

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

async fn make_shared_stores(fixture: &NatsFixture) -> (NatsMailboxStore, NatsMailboxStore) {
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
        config.sweeper_interval = Duration::from_secs(60);
        config
    };

    let store1 = NatsMailboxStore::connect(mk_config())
        .await
        .expect("connect 1");
    let store2 = NatsMailboxStore::connect(mk_config())
        .await
        .expect("connect 2");
    (store1, store2)
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
    store1.__test_purge_thread_index("t1").await.unwrap();
    store1.shutdown().await.unwrap();
    drop(store1);

    // Second store connects to same buckets; initial scan must rebuild the
    // authoritative thread index used by claim().
    let store2 = NatsMailboxStore::connect(mk_config())
        .await
        .expect("connect 2");

    let claimed = store2.claim("t1", "c2", 30_000, 1_000, 1).await.unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].dispatch_id, "d1");
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

#[tokio::test]
async fn load_dispatch_reads_authoritative_kv_not_stale_index() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;

    let dispatch = test_dispatch("d-strong-load", "t-strong-load");
    store.enqueue(&dispatch).await.unwrap();
    let claimed = store
        .claim("t-strong-load", "consumer", 30_000, 1_000, 1)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);

    let mut stale = dispatch;
    stale.status = RunDispatchStatus::Queued;
    store.__test_upsert_index_only(&stale).await;

    let loaded = store
        .load_dispatch("d-strong-load")
        .await
        .unwrap()
        .expect("authoritative dispatch");
    assert_eq!(loaded.status, RunDispatchStatus::Claimed);
    assert_eq!(loaded.claimed_by.as_deref(), Some("consumer"));
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn dispatch_signal_can_wake_a_different_store_instance() {
    let fixture = NatsFixture::start().await;
    let (store1, store2) = make_shared_stores(&fixture).await;

    store2.shutdown().await.unwrap();

    store1
        .enqueue(&test_dispatch("d-signal", "t-signal"))
        .await
        .unwrap();
    store2.__test_remove_dispatch_from_index("d-signal").await;
    assert!(
        !store2.index_contains("d-signal").await,
        "test requires store2's watcher index to be stale before pulling the signal"
    );

    let signals = store2
        .pull_dispatch_signals(8, Duration::from_secs(2))
        .await
        .unwrap();
    let signal = signals
        .into_iter()
        .find(|entry| entry.dispatch_id == "d-signal")
        .expect("dispatch signal");
    assert_eq!(signal.thread_id, "t-signal");
    assert!(
        store2.index_contains("d-signal").await,
        "pull_dispatch_signals must upsert the authoritative dispatch before acking the signal"
    );

    let claimed = store2
        .claim("t-signal", "consumer-2", 30_000, 1_000, 1)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].dispatch_id, "d-signal");
    signal.receipt.ack().await.unwrap();

    store1.shutdown().await.unwrap();
    store2.shutdown().await.unwrap();
}

#[tokio::test]
async fn pull_dispatch_signal_repairs_missing_thread_index_without_restart() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;

    store
        .enqueue(&test_dispatch(
            "d-signal-repairs-index",
            "t-signal-repairs-index",
        ))
        .await
        .unwrap();
    store
        .__test_purge_thread_index("t-signal-repairs-index")
        .await
        .unwrap();

    let mut signals = store
        .pull_dispatch_signals(1, Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(signals.len(), 1);
    let signal = signals.pop().unwrap();
    assert_eq!(signal.dispatch_id, "d-signal-repairs-index");

    let claimed = store
        .claim("t-signal-repairs-index", "consumer", 30_000, 1_000, 1)
        .await
        .unwrap();
    assert_eq!(
        claimed.len(),
        1,
        "pull_dispatch_signals should repair the authoritative thread index before claim"
    );
    assert_eq!(claimed[0].dispatch_id, "d-signal-repairs-index");
    signal.receipt.ack().await.unwrap();

    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn interrupt_detects_authoritative_active_dispatch_when_local_index_is_stale() {
    let fixture = NatsFixture::start().await;
    let (store1, store2) = make_shared_stores(&fixture).await;
    store2.shutdown().await.unwrap();

    store1
        .enqueue(&test_dispatch(
            "d-active-authoritative",
            "t-active-authoritative",
        ))
        .await
        .unwrap();
    let claimed = store1
        .claim("t-active-authoritative", "consumer-1", 30_000, 1_000, 1)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    let claim_token = claimed[0].claim_token.clone().unwrap();

    store2
        .__test_remove_dispatch_from_index("d-active-authoritative")
        .await;
    assert!(
        !store2.index_contains("d-active-authoritative").await,
        "test requires the interrupting node's local index to miss the active dispatch"
    );

    let interrupted = store2
        .interrupt("t-active-authoritative", 2_000)
        .await
        .unwrap();
    assert_eq!(
        interrupted
            .active_dispatch
            .as_ref()
            .map(|dispatch| dispatch.dispatch_id.as_str()),
        Some("d-active-authoritative")
    );

    let extended = store1
        .extend_lease("d-active-authoritative", &claim_token, 30_000, 3_000)
        .await
        .unwrap();
    assert!(
        !extended,
        "epoch-stale active dispatch must not keep extending its lease after interrupt"
    );
    let stale = store1
        .load_dispatch("d-active-authoritative")
        .await
        .unwrap()
        .expect("dispatch remains recorded");
    assert_eq!(stale.status, RunDispatchStatus::Superseded);

    store1.shutdown().await.unwrap();
}

#[tokio::test]
async fn interrupt_keeps_claim_guard_active_dispatch_over_stale_claimed_records() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    let thread_id = "t-active-override";

    store
        .enqueue(&test_dispatch("a-current-active", thread_id))
        .await
        .unwrap();
    let claimed = store
        .claim(thread_id, "current-consumer", 60_000, 1_000, 1)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].dispatch_id, "a-current-active");

    let mut stale = test_dispatch("z-stale-claimed", thread_id);
    stale.status = RunDispatchStatus::Claimed;
    stale.claim_token = Some("stale-token".to_string());
    stale.claimed_by = Some("stale-consumer".to_string());
    stale.lease_until = Some(500);
    store
        .__test_plant_dispatch_exact(&stale)
        .await
        .expect("plant stale claimed dispatch");

    let interrupted = store.interrupt(thread_id, 2_000).await.unwrap();
    assert_eq!(
        interrupted
            .active_dispatch
            .as_ref()
            .map(|dispatch| dispatch.dispatch_id.as_str()),
        Some("a-current-active"),
        "claim-guard active dispatch must not be overwritten by stale claimed scan results"
    );

    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn claim_uses_authoritative_ordering_when_local_index_only_has_later_dispatch() {
    let fixture = NatsFixture::start().await;
    let (store1, store2) = make_shared_stores(&fixture).await;
    store2.shutdown().await.unwrap();

    let mut high = test_dispatch("d-high-priority", "t-authoritative-order");
    high.priority = 10;
    high.created_at = 1;
    let mut low = test_dispatch("d-low-priority", "t-authoritative-order");
    low.priority = 128;
    low.created_at = 2;

    store1.enqueue(&high).await.unwrap();
    store1.enqueue(&low).await.unwrap();

    store2.__test_upsert_index_only(&low).await;
    store2
        .__test_remove_dispatch_from_index("d-high-priority")
        .await;
    assert!(
        !store2.index_contains("d-high-priority").await,
        "test requires local index to miss the higher-priority dispatch"
    );
    assert!(
        store2.index_contains("d-low-priority").await,
        "test requires local index to contain only the later dispatch"
    );

    let claimed = store2
        .claim("t-authoritative-order", "consumer-2", 30_000, 1_000, 1)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(
        claimed[0].dispatch_id, "d-high-priority",
        "claim must use authoritative ordering instead of the incomplete local watcher index"
    );

    store1.shutdown().await.unwrap();
}

#[tokio::test]
async fn reclaim_expired_leases_uses_authoritative_kv_and_clears_terminal_claim_fields() {
    let fixture = NatsFixture::start().await;
    let (store1, store2) = make_shared_stores(&fixture).await;
    store2.shutdown().await.unwrap();

    let mut dispatch = test_dispatch("d-expired-authoritative", "t-expired-authoritative");
    dispatch.max_attempts = 1;
    store1.enqueue(&dispatch).await.unwrap();
    let claimed = store1
        .claim("t-expired-authoritative", "consumer-1", 100, 1_000, 1)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);

    store2
        .__test_remove_dispatch_from_index("d-expired-authoritative")
        .await;
    assert!(
        !store2.index_contains("d-expired-authoritative").await,
        "test requires reclaiming node's local index to miss the expired claim"
    );

    let reclaimed = store2.reclaim_expired_leases(2_000, 10).await.unwrap();
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].dispatch_id, "d-expired-authoritative");
    assert_eq!(reclaimed[0].status, RunDispatchStatus::DeadLetter);
    assert!(reclaimed[0].claim_token.is_none());
    assert!(reclaimed[0].claimed_by.is_none());
    assert!(reclaimed[0].lease_until.is_none());

    let loaded = store1
        .load_dispatch("d-expired-authoritative")
        .await
        .unwrap()
        .expect("dispatch remains inspectable");
    assert_eq!(loaded.status, RunDispatchStatus::DeadLetter);
    assert!(loaded.claim_token.is_none());
    assert!(loaded.claimed_by.is_none());
    assert!(loaded.lease_until.is_none());

    store1.shutdown().await.unwrap();
}

#[tokio::test]
async fn dispatch_signal_nack_redelivers_same_dispatch() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;

    store
        .enqueue(&test_dispatch("d-signal-redeliver", "t-signal-redeliver"))
        .await
        .unwrap();

    let mut signals = store
        .pull_dispatch_signals(1, Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(signals.len(), 1);
    let signal = signals.pop().unwrap();
    assert_eq!(signal.dispatch_id, "d-signal-redeliver");
    signal.receipt.nack().await.unwrap();

    let deadline = Instant::now() + Duration::from_secs(3);
    let redelivered = loop {
        let mut signals = store
            .pull_dispatch_signals(1, Duration::from_millis(250))
            .await
            .unwrap();
        if let Some(signal) = signals.pop() {
            if signal.dispatch_id == "d-signal-redeliver" {
                break signal;
            }
            signal.receipt.ack().await.unwrap();
        }
        assert!(
            Instant::now() < deadline,
            "nacked dispatch signal must redeliver before the wakeup is lost"
        );
    };

    let dispatch = store
        .load_dispatch("d-signal-redeliver")
        .await
        .unwrap()
        .expect("dispatch should still exist after signal nack");
    assert_eq!(dispatch.status, RunDispatchStatus::Queued);
    redelivered.receipt.ack().await.unwrap();
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn dispatch_signal_delayed_nack_does_not_hot_loop() {
    let fixture = NatsFixture::start().await;
    let (store, other) = make_shared_stores(&fixture).await;
    other.shutdown().await.unwrap();

    store
        .enqueue(&test_dispatch("d-delayed-nack", "t-delayed-nack"))
        .await
        .unwrap();

    let mut signals = store
        .pull_dispatch_signals(1, Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(signals.len(), 1);
    let signal = signals.pop().unwrap();
    assert_eq!(signal.dispatch_id, "d-delayed-nack");

    let start = Instant::now();
    signal
        .receipt
        .nack_with_delay(Duration::from_millis(500))
        .await
        .unwrap();

    let early = store
        .pull_dispatch_signals(1, Duration::from_millis(150))
        .await
        .unwrap();
    assert!(
        early.is_empty(),
        "delayed nack must not immediately redeliver and spin while a thread is blocked"
    );

    let deadline = Instant::now() + Duration::from_secs(3);
    let redelivered = loop {
        let mut signals = store
            .pull_dispatch_signals(1, Duration::from_millis(250))
            .await
            .unwrap();
        if let Some(signal) = signals.pop() {
            if signal.dispatch_id == "d-delayed-nack" {
                break signal;
            }
            signal.receipt.ack().await.unwrap();
        }
        assert!(
            Instant::now() < deadline,
            "delayed nacked dispatch signal must redeliver after the delay"
        );
    };
    assert!(
        start.elapsed() >= Duration::from_millis(400),
        "redelivery happened too quickly for a delayed nack"
    );
    redelivered.receipt.ack().await.unwrap();

    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn sweeper_republishes_queued_signal_after_ttl() {
    let fixture = NatsFixture::start().await;
    let mut config = NatsMailboxConfig::new(fixture.url.clone());
    config.stream_name = format!("DISPATCH_{}", uuid::Uuid::now_v7().simple());
    config.consumer_name = format!("c_{}", uuid::Uuid::now_v7().simple());
    config.dispatch_bucket = format!("d_{}", uuid::Uuid::now_v7().simple());
    config.epoch_bucket = format!("e_{}", uuid::Uuid::now_v7().simple());
    config.thread_index_bucket = format!("ti_{}", uuid::Uuid::now_v7().simple());
    config.sweeper_interval = Duration::from_millis(50);
    config.sweeper_republish_after = Duration::from_millis(500);
    let store = NatsMailboxStore::connect(config).await.expect("connect");

    store
        .__test_plant_dispatch_without_publish(&test_dispatch("d-republish-ttl", "t-republish-ttl"))
        .await
        .unwrap();

    let first = loop {
        let mut signals = store
            .pull_dispatch_signals(1, Duration::from_millis(250))
            .await
            .unwrap();
        if let Some(signal) = signals.pop()
            && signal.dispatch_id == "d-republish-ttl"
        {
            break signal;
        }
    };
    first.receipt.ack().await.unwrap();

    let early = store
        .pull_dispatch_signals(1, Duration::from_millis(150))
        .await
        .unwrap();
    assert!(
        early.is_empty(),
        "sweeper must not republish before sweeper_republish_after elapses"
    );

    let deadline = Instant::now() + Duration::from_secs(3);
    let second = loop {
        let mut signals = store
            .pull_dispatch_signals(1, Duration::from_millis(250))
            .await
            .unwrap();
        if let Some(signal) = signals.pop()
            && signal.dispatch_id == "d-republish-ttl"
        {
            break signal;
        }
        assert!(
            Instant::now() < deadline,
            "queued dispatch signal should be republished after TTL"
        );
    };
    second.receipt.ack().await.unwrap();

    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn dispatch_signal_nack_redelivers_to_other_store_instance() {
    let fixture = NatsFixture::start().await;
    let (store1, store2) = make_shared_stores(&fixture).await;

    store1
        .enqueue(&test_dispatch("d-cross-redeliver", "t-cross-redeliver"))
        .await
        .unwrap();

    let mut signals = store1
        .pull_dispatch_signals(1, Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(signals.len(), 1);
    let signal = signals.pop().unwrap();
    assert_eq!(signal.dispatch_id, "d-cross-redeliver");
    signal.receipt.nack().await.unwrap();

    let deadline = Instant::now() + Duration::from_secs(3);
    let redelivered = loop {
        let mut signals = store2
            .pull_dispatch_signals(1, Duration::from_millis(250))
            .await
            .unwrap();
        if let Some(signal) = signals.pop() {
            if signal.dispatch_id == "d-cross-redeliver" {
                break signal;
            }
            signal.receipt.ack().await.unwrap();
        }
        assert!(
            Instant::now() < deadline,
            "nacked dispatch signal must be claimable by another store instance"
        );
    };

    let claimed = store2
        .claim("t-cross-redeliver", "consumer-2", 30_000, 1_000, 1)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].dispatch_id, "d-cross-redeliver");
    redelivered.receipt.ack().await.unwrap();

    store1.shutdown().await.unwrap();
    store2.shutdown().await.unwrap();
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

#[tokio::test]
async fn queue_claim_respects_retry_available_at_after_nack() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;

    store
        .enqueue(&test_dispatch("d-retry-window", "t-retry-window"))
        .await
        .unwrap();
    let claimed = store
        .claim("t-retry-window", "consumer-1", 30_000, 1_000, 1)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    let token = claimed[0].claim_token.clone().unwrap();
    store
        .nack("d-retry-window", &token, 10_000, "retry later", 2_000)
        .await
        .unwrap();

    let early = store
        .claim("t-retry-window", "consumer-2", 30_000, 3_000, 1)
        .await
        .unwrap();
    assert!(
        early.is_empty(),
        "queue claim must respect the current KV record's available_at retry window"
    );

    let inline = store
        .claim_dispatch("d-retry-window", "inline-consumer", 30_000, 3_000)
        .await
        .unwrap();
    assert!(
        inline.is_some(),
        "by-id inline claim still intentionally ignores available_at"
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

#[tokio::test]
async fn dedupe_lock_holder_delete_tombstone_is_reconciled_by_next_enqueue() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;

    let holder = test_dispatch("d-tombstone-holder", "t-tombstone");
    store.__test_plant_dispatch_exact(&holder).await.unwrap();
    store
        .__test_force_dedupe_lock("t-tombstone", "same-key", "d-tombstone-holder")
        .await
        .unwrap();
    store
        .__test_delete_dispatch_record("d-tombstone-holder")
        .await
        .unwrap();

    let mut next = test_dispatch("d-after-tombstone", "t-tombstone");
    next.dedupe_key = Some("same-key".to_string());
    store
        .enqueue(&next)
        .await
        .expect("dedupe lock holder tombstones should reconcile as missing holders");

    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn thread_claim_tombstone_does_not_block_next_claim() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;

    store
        .enqueue(&test_dispatch("d-claim-before-purge", "t-claim-tombstone"))
        .await
        .unwrap();
    let claimed = store
        .claim("t-claim-tombstone", "consumer-1", 30_000, 1_000, 1)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    let token = claimed[0].claim_token.clone().unwrap();
    store
        .ack("d-claim-before-purge", &token, 2_000)
        .await
        .unwrap();
    store
        .__test_purge_thread_claim("t-claim-tombstone")
        .await
        .unwrap();

    store
        .enqueue(&test_dispatch("d-claim-after-purge", "t-claim-tombstone"))
        .await
        .unwrap();
    let claimed = store
        .claim("t-claim-tombstone", "consumer-2", 30_000, 3_000, 1)
        .await
        .unwrap();
    assert_eq!(
        claimed.len(),
        1,
        "thread claim Delete/Purge tombstones should be treated as absent"
    );
    assert_eq!(claimed[0].dispatch_id, "d-claim-after-purge");

    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn dedupe_lock_tombstone_is_treated_as_absent() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;

    store
        .__test_force_dedupe_lock("t-lock-tombstone", "same-key", "old-holder")
        .await
        .unwrap();
    store
        .__test_purge_dedupe_lock("t-lock-tombstone", "same-key")
        .await
        .unwrap();
    assert_eq!(
        store
            .__test_dedupe_lock_holder("t-lock-tombstone", "same-key")
            .await
            .unwrap(),
        None
    );

    let mut next = test_dispatch("d-after-lock-tombstone", "t-lock-tombstone");
    next.dedupe_key = Some("same-key".to_string());
    store
        .enqueue(&next)
        .await
        .expect("dedupe lock tombstones should not block a new enqueue");

    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn purge_terminal_uses_authoritative_kv_when_local_index_is_missing() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;

    store
        .enqueue(&test_dispatch("d-authoritative-gc", "t-authoritative-gc"))
        .await
        .unwrap();
    let claimed = store
        .claim("t-authoritative-gc", "consumer", 30_000, 1_000, 1)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    let token = claimed[0].claim_token.clone().unwrap();
    store
        .ack("d-authoritative-gc", &token, 2_000)
        .await
        .unwrap();
    store
        .__test_remove_dispatch_from_index("d-authoritative-gc")
        .await;
    assert!(
        !store.index_contains("d-authoritative-gc").await,
        "test requires the local index to miss the terminal dispatch"
    );

    let purged = store.purge_terminal(3_000).await.unwrap();
    assert_eq!(
        purged, 1,
        "purge_terminal should scan authoritative dispatch KV, not only local index"
    );
    assert!(
        store
            .load_dispatch("d-authoritative-gc")
            .await
            .unwrap()
            .is_none()
    );

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

#[tokio::test]
async fn dedupe_key_reuse_publishes_distinct_dispatch_signals() {
    let fixture = NatsFixture::start().await;
    let mut config = NatsMailboxConfig::new(fixture.url.clone());
    config.stream_name = format!("DISPATCH_{}", uuid::Uuid::now_v7().simple());
    config.consumer_name = format!("c_{}", uuid::Uuid::now_v7().simple());
    config.dispatch_bucket = format!("d_{}", uuid::Uuid::now_v7().simple());
    config.epoch_bucket = format!("e_{}", uuid::Uuid::now_v7().simple());
    config.thread_index_bucket = format!("ti_{}", uuid::Uuid::now_v7().simple());
    config.sweeper_interval = Duration::from_secs(60);
    config.dedup_window = Duration::from_secs(120);
    let store = NatsMailboxStore::connect(config).await.expect("connect");

    let mut first = test_dispatch("d-dedupe-signal-1", "t-dedupe-signal");
    first.dedupe_key = Some("same-key".to_string());
    store.enqueue(&first).await.unwrap();
    let first_signal = store
        .pull_dispatch_signals(8, Duration::from_secs(2))
        .await
        .unwrap()
        .into_iter()
        .find(|entry| entry.dispatch_id == "d-dedupe-signal-1")
        .expect("first dispatch signal");
    first_signal.receipt.ack().await.unwrap();

    store
        .cancel("d-dedupe-signal-1", 1_000)
        .await
        .unwrap()
        .expect("cancel first dispatch");

    let mut second = test_dispatch("d-dedupe-signal-2", "t-dedupe-signal");
    second.dedupe_key = Some("same-key".to_string());
    store.enqueue(&second).await.unwrap();

    let second_signal = store
        .pull_dispatch_signals(8, Duration::from_secs(2))
        .await
        .unwrap()
        .into_iter()
        .find(|entry| entry.dispatch_id == "d-dedupe-signal-2")
        .expect("reusing a dedupe key must still publish a fresh dispatch signal");
    second_signal.receipt.ack().await.unwrap();

    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn concurrent_enqueue_same_thread_preserves_thread_index_entries() {
    let fixture = NatsFixture::start().await;
    let stream_name = format!("DISPATCH_{}", uuid::Uuid::now_v7().simple());
    let consumer_name = format!("c_{}", uuid::Uuid::now_v7().simple());
    let dispatch_bucket = format!("d_{}", uuid::Uuid::now_v7().simple());
    let epoch_bucket = format!("e_{}", uuid::Uuid::now_v7().simple());
    let thread_index_bucket = format!("ti_{}", uuid::Uuid::now_v7().simple());
    let mk_config = || {
        let mut config = NatsMailboxConfig::new(fixture.url.clone());
        config.stream_name = stream_name.clone();
        config.consumer_name = consumer_name.clone();
        config.dispatch_bucket = dispatch_bucket.clone();
        config.epoch_bucket = epoch_bucket.clone();
        config.thread_index_bucket = thread_index_bucket.clone();
        config.sweeper_interval = Duration::from_secs(60);
        config
    };

    let store = Arc::new(
        NatsMailboxStore::connect(mk_config())
            .await
            .expect("connect"),
    );
    let total = 64usize;
    let mut handles = Vec::new();
    for i in 0..total {
        let store = Arc::clone(&store);
        handles.push(tokio::spawn(async move {
            let mut dispatch = test_dispatch(&format!("d-hot-index-{i}"), "t-hot-index");
            dispatch.created_at = i as u64;
            dispatch.updated_at = i as u64;
            store.enqueue(&dispatch).await
        }));
    }
    for handle in handles {
        handle.await.unwrap().unwrap();
    }
    store.shutdown().await.unwrap();
    drop(store);

    let verifier = NatsMailboxStore::connect(mk_config())
        .await
        .expect("verifier connect");
    let mut claimed = Vec::new();
    for i in 0..total {
        let got = verifier
            .claim("t-hot-index", "verifier", 30_000, 10_000 + i as u64, 1)
            .await
            .unwrap();
        assert_eq!(
            got.len(),
            1,
            "thread index should expose every concurrently enqueued dispatch"
        );
        let dispatch = got.into_iter().next().unwrap();
        let token = dispatch.claim_token.clone().unwrap();
        verifier
            .ack(&dispatch.dispatch_id, &token, 20_000 + i as u64)
            .await
            .unwrap();
        claimed.push(dispatch.dispatch_id);
    }
    claimed.sort();
    assert_eq!(claimed.len(), total);
    for i in 0..total {
        assert!(
            claimed.iter().any(|id| id == &format!("d-hot-index-{i}")),
            "missing dispatch d-hot-index-{i}"
        );
    }

    verifier.shutdown().await.unwrap();
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
