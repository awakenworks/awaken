mod harness;

use std::sync::Arc;

use awaken_agent_contract::stream::checkpoint::{StreamCheckpoint, StreamCheckpointStore};
use awaken_run_ingress::{
    DispatchQueue, FencedStreamCheckpointStore, MemoryDispatchStore, RunClaim, RunDispatch,
};
use awaken_runtime::memory::MemoryStreamCheckpointStore;

use harness::activation;

fn checkpoint(text: &str) -> StreamCheckpoint {
    StreamCheckpoint {
        run_id: "run-fenced-checkpoint".to_string(),
        thread_id: "thread-run-fenced-checkpoint".to_string(),
        model: "model".to_string(),
        partial_text: text.to_string(),
        partial_tools: Vec::new(),
    }
}

#[tokio::test]
async fn replacement_fences_stale_checkpoint_put_and_delete() {
    let dispatch = Arc::new(MemoryDispatchStore::new());
    dispatch
        .enqueue(RunDispatch::new(activation("run-fenced-checkpoint")))
        .await
        .unwrap();
    let first = dispatch.claim("worker-a", 10, 0).await.unwrap().unwrap();
    let inner = Arc::new(MemoryStreamCheckpointStore::new());
    let first_store = FencedStreamCheckpointStore::new(
        inner.clone(),
        dispatch.clone(),
        RunClaim::from(&first.lease),
    );
    first_store.put(checkpoint("first")).await;
    assert_eq!(
        inner
            .get("run-fenced-checkpoint")
            .await
            .unwrap()
            .partial_text,
        "first"
    );

    let second = dispatch.claim("worker-b", 10, 11).await.unwrap().unwrap();
    let second_store =
        FencedStreamCheckpointStore::new(inner.clone(), dispatch, RunClaim::from(&second.lease));
    first_store.put(checkpoint("stale")).await;
    first_store.delete("run-fenced-checkpoint").await;
    assert_eq!(
        inner
            .get("run-fenced-checkpoint")
            .await
            .unwrap()
            .partial_text,
        "first",
        "stale checkpoint mutations are ignored"
    );
    second_store.put(checkpoint("replacement")).await;
    assert_eq!(
        second_store
            .get("run-fenced-checkpoint")
            .await
            .unwrap()
            .partial_text,
        "replacement"
    );
    second_store.delete("run-fenced-checkpoint").await;
    assert!(inner.get("run-fenced-checkpoint").await.is_none());
}
