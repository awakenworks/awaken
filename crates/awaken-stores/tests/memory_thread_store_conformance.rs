#[path = "thread_store_conformance.rs"]
mod thread_store_conformance;

use awaken_stores::InMemoryStore;

macro_rules! conformance_test {
    ($name:ident) => {
        #[tokio::test]
        async fn $name() {
            let store = InMemoryStore::new();
            thread_store_conformance::$name(&store).await;
        }
    };
}

conformance_test!(checkpoint_persists_messages_and_run);
conformance_test!(load_messages_returns_none_for_unknown_thread);
conformance_test!(latest_run_returns_most_recent);
conformance_test!(checkpoint_overwrites_messages);
conformance_test!(load_thread_reflects_checkpoint);
conformance_test!(append_message_records_assigns_seq);
conformance_test!(checkpoint_append_assigns_version);
conformance_test!(checkpoint_append_unconditional_appends);
conformance_test!(checkpoint_append_rejects_stale_version);
conformance_test!(checkpoint_append_rejects_existing_message_id);
conformance_test!(list_threads_query_filters_lineage);
conformance_test!(list_threads_query_filters_root_threads);
conformance_test!(list_runs_filters_by_id_prefix);
conformance_test!(list_threads_query_filters_by_id_prefix);
conformance_test!(list_threads_query_id_prefix_paginates_and_binds_cursor);
conformance_test!(checkpoint_rejects_missing_parent_thread);
conformance_test!(checkpoint_rejects_cycle_parent_assignment);
conformance_test!(delete_thread_with_detach_preserves_children);
conformance_test!(delete_thread_with_reject_preserves_tree);
conformance_test!(delete_thread_with_cascade_removes_descendants);
conformance_test!(list_message_records_query_filters_and_orders);
conformance_test!(load_run_returns_none_for_unknown);
