#![cfg(feature = "nats")]

mod nats_fixture;

use awaken_server_contract::contract::mailbox::RunDispatch;
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

fn unique_config(fixture: &NatsFixture) -> NatsMailboxConfig {
    let mut config = NatsMailboxConfig::new(fixture.url.clone());
    config.stream_name = format!("DISPATCH_{}", uuid::Uuid::now_v7().simple());
    config.consumer_name = format!("c_{}", uuid::Uuid::now_v7().simple());
    config.dispatch_bucket = format!("d_{}", uuid::Uuid::now_v7().simple());
    config.epoch_bucket = format!("e_{}", uuid::Uuid::now_v7().simple());
    config.thread_index_bucket = format!("ti_{}", uuid::Uuid::now_v7().simple());
    config
}

#[tokio::test]
async fn connect_creates_stream_and_buckets() {
    let fixture = NatsFixture::start().await;
    let store = NatsMailboxStore::connect(unique_config(&fixture))
        .await
        .expect("connect");
    store.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn watcher_populates_index_on_kv_put() {
    let fixture = NatsFixture::start().await;
    let store = NatsMailboxStore::connect(unique_config(&fixture))
        .await
        .expect("connect");

    let dispatch = test_dispatch("d1", "t1");
    let bytes = awaken_stores::nats_mailbox::__test_encode_dispatch(&dispatch);
    store
        .kv_dispatch()
        .put("dispatch.d1", bytes.into())
        .await
        .expect("kv put");

    // Wait up to 1s for watcher to observe.
    let mut found = false;
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        if store.index_contains("d1").await {
            found = true;
            break;
        }
    }
    assert!(found, "watcher did not populate index");
    store.shutdown().await.unwrap();
}
