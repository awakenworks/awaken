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
conformance_test!(load_run_returns_none_for_unknown);
