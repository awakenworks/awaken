#![cfg(feature = "nats")]

mod nats_fixture;

use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, Instant},
};

use awaken_server_contract::contract::mailbox::{MailboxStore, RunDispatch};
use awaken_stores::{NatsMailboxConfig, NatsMailboxStore};
use nats_fixture::NatsFixture;

fn test_dispatch(id: &str, thread_id: &str) -> RunDispatch {
    RunDispatch::queued(
        id.to_string(),
        thread_id.to_string(),
        format!("{id}-run"),
        0,
    )
    .with_max_attempts(3)
}

fn stress_records(default: usize) -> usize {
    std::env::var("AWAKEN_NATS_STRESS_RECORDS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn shared_config(url: String) -> impl Fn() -> NatsMailboxConfig {
    let stream_name = format!("DISPATCH_{}", uuid::Uuid::now_v7().simple());
    let consumer_name = format!("c_{}", uuid::Uuid::now_v7().simple());
    let dispatch_bucket = format!("d_{}", uuid::Uuid::now_v7().simple());
    let epoch_bucket = format!("e_{}", uuid::Uuid::now_v7().simple());
    let thread_index_bucket = format!("ti_{}", uuid::Uuid::now_v7().simple());
    move || {
        let mut config = NatsMailboxConfig::new(url.clone());
        config.stream_name = stream_name.clone();
        config.consumer_name = consumer_name.clone();
        config.dispatch_bucket = dispatch_bucket.clone();
        config.epoch_bucket = epoch_bucket.clone();
        config.thread_index_bucket = thread_index_bucket.clone();
        config.sweeper_interval = Duration::from_secs(60);
        config
    }
}

#[tokio::test]
#[ignore = "stress test: set AWAKEN_NATS_STRESS_RECORDS=100000 for large runs"]
async fn claim_latency_uses_thread_index_under_large_global_kv() {
    let fixture = NatsFixture::start().await;
    let mk_config = shared_config(fixture.url.clone());
    let store = NatsMailboxStore::connect(mk_config())
        .await
        .expect("connect");
    let records = stress_records(10_000);

    for i in 0..records {
        let dispatch = test_dispatch(&format!("d-global-{i}"), &format!("t-global-{i}"))
            .with_created_at(i as u64);
        store.enqueue(&dispatch).await.unwrap();
    }

    let target = test_dispatch("d-target", "t-target").with_priority(1);
    store.enqueue(&target).await.unwrap();

    let start = Instant::now();
    let claimed = store
        .claim("t-target", "stress-consumer", 30_000, 10_000, 1)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].dispatch_id(), "d-target");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "claim latency should not scale linearly with unrelated global dispatch records"
    );

    store.shutdown().await.unwrap();
}

#[tokio::test]
#[ignore = "stress test for multi-node claim contention"]
async fn concurrent_nodes_claim_same_thread_without_duplicate_owners() {
    let fixture = NatsFixture::start().await;
    let mk_config = shared_config(fixture.url.clone());
    let seed = NatsMailboxStore::connect(mk_config()).await.expect("seed");
    let total = stress_records(1_000).min(1_000);
    for i in 0..total {
        let dispatch =
            test_dispatch(&format!("d-thread-{i}"), "t-hot-thread").with_created_at(i as u64);
        seed.enqueue(&dispatch).await.unwrap();
    }
    seed.shutdown().await.unwrap();

    let stores =
        futures::future::try_join_all((0..8).map(|_| NatsMailboxStore::connect(mk_config())))
            .await
            .expect("workers");
    let stores = Arc::new(stores);
    let mut handles = Vec::new();
    for worker in 0..stores.len() {
        let stores = Arc::clone(&stores);
        handles.push(tokio::spawn(async move {
            let mut out = Vec::new();
            loop {
                let claimed = stores[worker]
                    .claim(
                        "t-hot-thread",
                        &format!("consumer-{worker}"),
                        30_000,
                        100_000,
                        10,
                    )
                    .await
                    .unwrap();
                if claimed.is_empty() {
                    break;
                }
                for dispatch in claimed {
                    let token = dispatch.claim_token().unwrap().to_string();
                    stores[worker]
                        .ack(dispatch.dispatch_id(), &token, 100_001)
                        .await
                        .unwrap();
                    out.push(dispatch.dispatch_id().to_string());
                }
            }
            out
        }));
    }

    let mut ids = HashSet::new();
    for handle in handles {
        for id in handle.await.unwrap() {
            assert!(ids.insert(id), "dispatch claimed more than once");
        }
    }
    assert_eq!(ids.len(), total);

    for store in stores.iter() {
        store.shutdown().await.unwrap();
    }
}

#[tokio::test]
#[ignore = "chaos test for active claims plus signal redelivery pressure"]
async fn active_claim_with_many_queued_dispatches_remains_recoverable() {
    let fixture = NatsFixture::start().await;
    let mk_config = shared_config(fixture.url.clone());
    let store = NatsMailboxStore::connect(mk_config())
        .await
        .expect("connect");

    let active = test_dispatch("d-active", "t-active");
    store.enqueue(&active).await.unwrap();
    let claimed = store
        .claim("t-active", "owner", 30_000, 1_000, 1)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);

    for i in 0..stress_records(500) {
        let dispatch =
            test_dispatch(&format!("d-queued-{i}"), "t-active").with_created_at(10 + i as u64);
        store.enqueue(&dispatch).await.unwrap();
    }

    let signals = store
        .pull_dispatch_signals(64, Duration::from_secs(2))
        .await
        .unwrap();
    for signal in signals {
        signal
            .receipt
            .nack_with_delay(Duration::from_millis(250))
            .await
            .unwrap();
    }

    let token = claimed[0].claim_token().unwrap().to_string();
    store.ack("d-active", &token, 2_000).await.unwrap();
    let recovered = store
        .claim("t-active", "next", 30_000, 3_000, 10)
        .await
        .unwrap();
    assert!(
        !recovered.is_empty(),
        "queued dispatches should recover after active owner ack"
    );

    store.shutdown().await.unwrap();
}

#[tokio::test]
#[ignore = "stress test for high-concurrency dedupe conflicts"]
async fn high_concurrency_dedupe_key_admits_exactly_one_owner() {
    let fixture = NatsFixture::start().await;
    let mk_config = shared_config(fixture.url.clone());
    let store = Arc::new(
        NatsMailboxStore::connect(mk_config())
            .await
            .expect("connect"),
    );
    let total = stress_records(128);
    let mut handles = Vec::new();
    for i in 0..total {
        let store = Arc::clone(&store);
        handles.push(tokio::spawn(async move {
            let dispatch = test_dispatch(&format!("d-dedupe-{i}"), "t-dedupe-hot")
                .with_dedupe_key(Some("same-key".to_string()));
            store.enqueue(&dispatch).await
        }));
    }

    let mut success_count = 0usize;
    for handle in handles {
        if handle.await.unwrap().is_ok() {
            success_count += 1;
        }
    }
    assert_eq!(success_count, 1);

    store.shutdown().await.unwrap();
}
