#![allow(deprecated)] // ADR-0038 D7: integration tests exercise the legacy checkpoint API directly
#![cfg(feature = "nats")]

#[path = "nats_buffered_thread_fixture.rs"]
mod fixture;
#[path = "thread_store_conformance.rs"]
mod thread_store_conformance;

use std::sync::Arc;

use awaken_stores::{InMemoryStore, NatsBufferedThreadStore};
use fixture::{NatsFixture, unique_config};

async fn make_store(fixture: &NatsFixture) -> NatsBufferedThreadStore<InMemoryStore> {
    let inner = Arc::new(InMemoryStore::new());
    NatsBufferedThreadStore::connect(inner, unique_config(fixture))
        .await
        .expect("connect")
}

#[tokio::test]
async fn nats_checkpoint_persists_messages_and_run() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    thread_store_conformance::checkpoint_persists_messages_and_run(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_load_messages_returns_none_for_unknown_thread() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    thread_store_conformance::load_messages_returns_none_for_unknown_thread(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_latest_run_returns_most_recent() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    thread_store_conformance::latest_run_returns_most_recent(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_checkpoint_overwrites_messages() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    thread_store_conformance::checkpoint_overwrites_messages(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_load_thread_reflects_checkpoint() {
    // load_thread goes through to inner; we need force_flush first.
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    let thread_id = "t-meta";
    let run = thread_store_conformance::make_run(
        "r1",
        thread_id,
        awaken_contract::contract::lifecycle::RunStatus::Done,
    );
    use awaken_contract::contract::storage::ThreadRunStore;
    store.checkpoint(thread_id, &[], &run).await.unwrap();
    store.force_flush(thread_id).await.unwrap();
    use awaken_contract::contract::storage::ThreadStore;
    let thread = store.load_thread(thread_id).await.unwrap();
    assert!(thread.is_some());
    assert_eq!(thread.unwrap().id, thread_id);
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_append_message_records_assigns_seq() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    thread_store_conformance::append_message_records_assigns_seq(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_checkpoint_rejects_missing_parent_thread() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    thread_store_conformance::checkpoint_rejects_missing_parent_thread(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_checkpoint_rejects_cycle_parent_assignment() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    thread_store_conformance::checkpoint_rejects_cycle_parent_assignment(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_delete_thread_with_detach_preserves_children() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    thread_store_conformance::delete_thread_with_detach_preserves_children(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_delete_thread_with_reject_preserves_tree() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    thread_store_conformance::delete_thread_with_reject_preserves_tree(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_delete_thread_with_cascade_removes_descendants() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    thread_store_conformance::delete_thread_with_cascade_removes_descendants(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_load_run_returns_none_for_unknown() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    thread_store_conformance::load_run_returns_none_for_unknown(&store).await;
    store.shutdown().await.unwrap();
}
