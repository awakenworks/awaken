//! Conformance test binary wiring the shared `mailbox_conformance` harness to
//! the in-memory `InMemoryMailboxStore` backend.

mod mailbox_conformance;

use awaken_stores::InMemoryMailboxStore;

macro_rules! conformance_test {
    ($name:ident) => {
        #[tokio::test]
        async fn $name() {
            let store = InMemoryMailboxStore::new();
            mailbox_conformance::$name(&store).await;
        }
    };
}

conformance_test!(enqueue_and_list);
conformance_test!(claim_returns_queued_dispatch);
conformance_test!(claim_respects_available_at);
conformance_test!(claim_respects_priority_before_created_at);
conformance_test!(claim_limit_preserves_thread_exclusivity);
conformance_test!(list_dispatches_orders_by_priority_then_created_at);
conformance_test!(ack_transitions_to_acked);
conformance_test!(ack_rejects_wrong_claim_token);
conformance_test!(nack_returns_to_queued);
conformance_test!(nack_dead_letters_after_max_attempts);
conformance_test!(cancel_queued_dispatch);
conformance_test!(cancel_claimed_dispatch_returns_none);
conformance_test!(extend_lease_success);
conformance_test!(extend_lease_wrong_token);
conformance_test!(interrupt_supersedes_queued);
conformance_test!(interrupt_returns_active_claimed);
conformance_test!(dedupe_key_rejects_duplicate);
conformance_test!(reclaim_expired_leases_requeues);
conformance_test!(reclaim_expired_leases_dead_letters_at_max_attempts);
conformance_test!(purge_terminal_removes_old);
conformance_test!(queued_thread_ids_returns_active_threads);
conformance_test!(claim_dispatch_by_id);
conformance_test!(claim_dispatch_ignores_available_at);
conformance_test!(record_dispatch_start_marks_running);
conformance_test!(record_run_result_sets_terminal_projection);
conformance_test!(record_projection_rejects_missing_or_unclaimed_dispatch);
conformance_test!(fifo_ordering_within_same_priority);
