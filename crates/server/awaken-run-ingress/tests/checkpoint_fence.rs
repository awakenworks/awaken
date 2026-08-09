mod harness;

use std::sync::Arc;

use awaken_agent_contract::stream::checkpoint::{StreamCheckpoint, StreamCheckpointStore};
use awaken_run_ingress::{
    DispatchQueue, FencedStreamCheckpointStore, MemoryDispatchStore, RunClaim, RunDispatch,
};
use awaken_store_inmem::MemoryStreamCheckpointStore;

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
    // Cause/effect graph: C1 claim current; C2 durable inner present; C3 replacement
    // advances epoch. Decision table: R1 C1+C2+!C3 -> put/get applies; R2 !C1+C2+C3
    // -> stale put/delete ignored; R3 new C1+C2+C3 -> replacement applies/deletes.
    // The remote `inner=None` row is covered through the transport E2E, where an
    // unverifiable local epoch delegates to the claim-bound upstream operations.
    let dispatch = Arc::new(MemoryDispatchStore::new());
    dispatch
        .enqueue(RunDispatch::new(activation("run-fenced-checkpoint")))
        .await
        .unwrap();
    let first = dispatch
        .claim("worker-a", 10, 0, &Default::default())
        .await
        .unwrap()
        .unwrap();
    let inner = Arc::new(MemoryStreamCheckpointStore::new());
    let first_store = FencedStreamCheckpointStore::new(
        Some(inner.clone()),
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

    let second = dispatch
        .claim("worker-b", 10, 11, &Default::default())
        .await
        .unwrap()
        .unwrap();
    let second_store = FencedStreamCheckpointStore::new(
        Some(inner.clone()),
        dispatch,
        RunClaim::from(&second.lease),
    );
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
