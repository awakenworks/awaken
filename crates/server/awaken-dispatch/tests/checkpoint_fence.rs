// @formal evidence checkpoint.terminal_completion_clears_resume
use awaken_dispatch_contract::{DispatchClaims, DispatchEnqueue};
#[path = "checkpoint_fence/authority_matrix.rs"]
mod authority_matrix;
mod harness;

use std::sync::Arc;

use awaken_agent_contract::observation::{Observed, UnknownCause};
use awaken_agent_contract::stream::checkpoint::{StreamCheckpoint, StreamCheckpointStore};
use awaken_dispatch::{FencedStreamCheckpointStore, MemoryDispatchStore, RunClaim, RunDispatch};
use awaken_store_inmem::MemoryStreamCheckpointStore;

use awaken_dispatch_testkit::ConformanceClock as _;
use harness::activation;

/// The partial an observation found. An `Unknown` fails the assertion instead
/// of reading as absence.
fn present(observed: Observed<StreamCheckpoint>) -> StreamCheckpoint {
    match observed {
        Observed::Present(checkpoint) => checkpoint,
        Observed::Absent => panic!("expected a checkpoint, the store reported absence"),
        Observed::Unknown(cause) => panic!("the store could not answer: {cause}"),
    }
}

fn checkpoint(text: &str) -> StreamCheckpoint {
    StreamCheckpoint {
        run_id: "run-fenced-checkpoint".to_string(),
        thread_id: "thread-run-fenced-checkpoint".to_string(),
        model: "model".to_string(),
        partial_text: text.to_string(),
        partial_tools: Vec::new(),
        retry_count: 0,
    }
}

#[tokio::test]
async fn replacement_fences_stale_checkpoint_put_and_delete() {
    // Cause/effect graph: C1 claim current; C2 durable inner present; C3 replacement
    // advances epoch. Decision table: R1 C1+C2+!C3 -> put/get applies; R2 !C1+C2+C3
    // -> stale put/delete ignored; R3 new C1+C2+C3 -> replacement applies/deletes.
    // The remote `inner=None` row is covered through the transport E2E, where an
    // unverifiable local epoch delegates to the claim-bound upstream operations.
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let dispatch = Arc::new(MemoryDispatchStore::new(clock.clock()));
    dispatch
        .enqueue(RunDispatch::new(activation("run-fenced-checkpoint")))
        .await
        .unwrap();
    clock.set(0);
    let first = dispatch
        .claim("worker-a", 10, &Default::default())
        .await
        .unwrap()
        .unwrap();
    let inner = Arc::new(MemoryStreamCheckpointStore::new());
    let first_store = FencedStreamCheckpointStore::new(
        Some(inner.clone()),
        dispatch.clone(),
        RunClaim::from(&first.lease),
    );
    first_store.put(checkpoint("first")).await.unwrap();
    assert_eq!(
        present(inner.get("run-fenced-checkpoint").await).partial_text,
        "first"
    );

    clock.set(11);
    let second = dispatch
        .claim("worker-b", 10, &Default::default())
        .await
        .unwrap()
        .unwrap();
    let second_store = FencedStreamCheckpointStore::new(
        Some(inner.clone()),
        dispatch,
        RunClaim::from(&second.lease),
    );
    // R4 stale reads also fail: no replacement checkpoint leaks to an old claim.
    assert!(matches!(
        first_store.get("run-fenced-checkpoint").await,
        Observed::Unknown(UnknownCause::Fenced(_))
    ));
    assert!(first_store.put(checkpoint("stale")).await.is_err());
    assert!(first_store.delete("run-fenced-checkpoint").await.is_err());
    assert_eq!(
        present(inner.get("run-fenced-checkpoint").await).partial_text,
        "first",
        "stale checkpoint mutations are ignored"
    );
    // R5 a current claim for R cannot read/write/delete any other Run, even
    // with a valid inner store. Wrong-Run rejection precedes every inner effect.
    let mut foreign = checkpoint("foreign");
    foreign.run_id = "other-run".into();
    assert!(second_store.put(foreign).await.is_err());
    assert!(matches!(
        second_store.get("other-run").await,
        Observed::Unknown(UnknownCause::Fenced(_))
    ));
    assert!(second_store.delete("other-run").await.is_err());
    assert_eq!(inner.get("other-run").await, Observed::Absent);
    second_store.put(checkpoint("replacement")).await.unwrap();
    assert_eq!(
        present(second_store.get("run-fenced-checkpoint").await).partial_text,
        "replacement"
    );
    second_store.delete("run-fenced-checkpoint").await.unwrap();
    assert_eq!(inner.get("run-fenced-checkpoint").await, Observed::Absent);
}
