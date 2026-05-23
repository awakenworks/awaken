#![cfg(feature = "postgres")]

use awaken_contract::contract::stream_checkpoint::{StreamCheckpoint, StreamCheckpointStore};
use awaken_stores::PostgresStore;
use sqlx::PgPool;

fn sample(run_id: &str, partial_text: &str, updated_at_ms: u64) -> StreamCheckpoint {
    StreamCheckpoint {
        run_id: run_id.to_string(),
        thread_id: "thread-1".to_string(),
        upstream_model: "test-model".to_string(),
        partial_text: partial_text.to_string(),
        completed_tool_calls: Vec::new(),
        in_flight_tool: None,
        updated_at_ms,
    }
}

#[tokio::test]
#[ignore]
async fn postgres_stream_checkpoint_put_get_delete_roundtrip() {
    let pool = PgPool::connect("postgres://localhost/awaken_test")
        .await
        .unwrap();
    let prefix = format!("test_stream_checkpoint_{}", uuid::Uuid::now_v7().simple());
    let store = PostgresStore::with_prefix(pool, prefix);
    store.ensure_schema().await.unwrap();

    store.put(sample("run-a", "hello", 1_000)).await.unwrap();
    let first = store.get("run-a").await.unwrap().unwrap();
    assert_eq!(first.thread_id, "thread-1");
    assert_eq!(first.partial_text, "hello");
    assert_eq!(first.updated_at_ms, 1_000);

    store.put(sample("run-a", "updated", 2_000)).await.unwrap();
    let updated = store.get("run-a").await.unwrap().unwrap();
    assert_eq!(updated.partial_text, "updated");
    assert_eq!(updated.updated_at_ms, 2_000);

    store.delete("run-a").await.unwrap();
    assert!(store.get("run-a").await.unwrap().is_none());
}
