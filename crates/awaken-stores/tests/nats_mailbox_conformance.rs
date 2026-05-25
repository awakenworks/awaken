#![cfg(feature = "nats")]

#[path = "mailbox_conformance.rs"]
mod mailbox_conformance;
mod nats_fixture;

use awaken_stores::{NatsMailboxConfig, NatsMailboxStore};
use nats_fixture::NatsFixture;

async fn make_store(fixture: &NatsFixture) -> NatsMailboxStore {
    let mut config = NatsMailboxConfig::new(fixture.url.clone());
    config.stream_name = format!("DISPATCH_{}", uuid::Uuid::now_v7().simple());
    config.consumer_name = format!("c_{}", uuid::Uuid::now_v7().simple());
    config.dispatch_bucket = format!("d_{}", uuid::Uuid::now_v7().simple());
    config.epoch_bucket = format!("e_{}", uuid::Uuid::now_v7().simple());
    config.thread_index_bucket = format!("ti_{}", uuid::Uuid::now_v7().simple());
    NatsMailboxStore::connect(config).await.expect("connect")
}

#[tokio::test]
async fn nats_enqueue_and_list() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    mailbox_conformance::enqueue_and_list(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_dedupe_key_rejects_duplicate() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    mailbox_conformance::dedupe_key_rejects_duplicate(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_queued_thread_ids_returns_active_threads() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    mailbox_conformance::queued_thread_ids_returns_active_threads(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_count_dispatches_by_status_tracks_lifecycle() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    mailbox_conformance::count_dispatches_by_status_tracks_lifecycle(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_list_terminal_dispatches_returns_all_terminal() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    mailbox_conformance::list_terminal_dispatches_returns_all_terminal(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_claim_returns_queued_dispatch() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    mailbox_conformance::claim_returns_queued_dispatch(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_claim_respects_available_at() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    mailbox_conformance::claim_respects_available_at(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_claim_respects_priority_before_created_at() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    mailbox_conformance::claim_respects_priority_before_created_at(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_claim_limit_preserves_thread_exclusivity() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    mailbox_conformance::claim_limit_preserves_thread_exclusivity(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_list_dispatches_orders_by_priority_then_created_at() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    mailbox_conformance::list_dispatches_orders_by_priority_then_created_at(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_extend_lease_success() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    mailbox_conformance::extend_lease_success(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_extend_lease_wrong_token() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    mailbox_conformance::extend_lease_wrong_token(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_claim_dispatch_by_id() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    mailbox_conformance::claim_dispatch_by_id(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_claim_dispatch_ignores_available_at() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    mailbox_conformance::claim_dispatch_ignores_available_at(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_record_dispatch_start_marks_running() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    mailbox_conformance::record_dispatch_start_marks_running(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_record_run_result_sets_terminal_projection() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    mailbox_conformance::record_run_result_sets_terminal_projection(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_record_projection_rejects_missing_or_unclaimed_dispatch() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    mailbox_conformance::record_projection_rejects_missing_or_unclaimed_dispatch(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_fifo_ordering_within_same_priority() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    mailbox_conformance::fifo_ordering_within_same_priority(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_ack_transitions_to_acked() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    mailbox_conformance::ack_transitions_to_acked(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_ack_rejects_wrong_claim_token() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    mailbox_conformance::ack_rejects_wrong_claim_token(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_nack_returns_to_queued() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    mailbox_conformance::nack_returns_to_queued(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_nack_dead_letters_after_max_attempts() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    mailbox_conformance::nack_dead_letters_after_max_attempts(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_cancel_queued_dispatch() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    mailbox_conformance::cancel_queued_dispatch(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_cancel_claimed_dispatch_returns_none() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    mailbox_conformance::cancel_claimed_dispatch_returns_none(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_interrupt_supersedes_queued() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    mailbox_conformance::interrupt_supersedes_queued(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_interrupt_returns_active_claimed() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    mailbox_conformance::interrupt_returns_active_claimed(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_current_dispatch_epoch_tracks_interrupt() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    mailbox_conformance::current_dispatch_epoch_tracks_interrupt(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_supersede_claimed_terminalizes_active_dispatch() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    mailbox_conformance::supersede_claimed_terminalizes_active_dispatch(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_extend_lease_rejects_stale_claim_after_interrupt() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    mailbox_conformance::extend_lease_rejects_stale_claim_after_interrupt(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_record_dispatch_start_rejects_stale_claim_after_interrupt() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    mailbox_conformance::record_dispatch_start_rejects_stale_claim_after_interrupt(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_reclaim_expired_stale_claim_supersedes() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    mailbox_conformance::reclaim_expired_stale_claim_supersedes(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_reclaim_expired_leases_requeues() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    mailbox_conformance::reclaim_expired_leases_requeues(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_reclaim_expired_leases_dead_letters_at_max_attempts() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    mailbox_conformance::reclaim_expired_leases_dead_letters_at_max_attempts(&store).await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_reclaim_expired_leases_dead_letter_is_idempotent_under_concurrency() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    mailbox_conformance::reclaim_expired_leases_dead_letter_is_idempotent_under_concurrency(&store)
        .await;
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn nats_purge_terminal_removes_old() {
    let fixture = NatsFixture::start().await;
    let store = make_store(&fixture).await;
    mailbox_conformance::purge_terminal_removes_old(&store).await;
    store.shutdown().await.unwrap();
}
