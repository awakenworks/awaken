//! One mutable-register contract for interrupted Run checkpoints. Fenced
//! dispatch admission remains owned by dispatch-testkit and its decorators.
use awaken_agent_contract::observation::Observed;
use awaken_agent_contract::stream::checkpoint::{
    PartialToolCall, StreamCheckpoint, StreamCheckpointStore,
};

pub fn sample(run_id: &str) -> StreamCheckpoint {
    StreamCheckpoint {
        run_id: run_id.into(),
        thread_id: "t1".into(),
        model: "m".into(),
        partial_text: "the partial so far".into(),
        partial_tools: vec![PartialToolCall {
            call_id: "c1".into(),
            tool_id: "search".into(),
            raw_arguments: r#"{"q":"ru"#.into(),
        }],
        retry_count: 0,
    }
}

pub async fn writes_are_exact(store: &dyn StreamCheckpointStore, run_id: &str) -> StreamCheckpoint {
    // Causes: C1 absent row; C2 identical write; C3 changed full frame;
    // C4 different Run; C5 concurrent same-Run writes. Rules/effects:
    // R1 C1 => Absent, then put => exact Present; R2 C2 => same frame;
    // R3 C3 => exact replacement, including tools/retry count;
    // R4 C4 delete => original untouched; R5 C5 => one whole frame,
    // never a torn mix. The returned acknowledged frame is the restart oracle.
    assert_eq!(store.get(run_id).await, Observed::Absent, "R1");
    let first = sample(run_id);
    store.put(first.clone()).await.unwrap();
    store.put(first.clone()).await.unwrap();
    assert_eq!(
        store.get(run_id).await,
        Observed::Present(first.clone()),
        "R1/R2"
    );
    let mut next = first.clone();
    next.partial_text = "replacement".into();
    next.partial_tools.clear();
    next.retry_count = 3;
    store.put(next.clone()).await.unwrap();
    assert_eq!(
        store.get(run_id).await,
        Observed::Present(next.clone()),
        "R3"
    );
    let other = format!("{run_id}:other");
    store.put(sample(&other)).await.unwrap();
    store.delete(&other).await.unwrap();
    assert_eq!(
        store.get(run_id).await,
        Observed::Present(next.clone()),
        "R4"
    );
    let (a, b) = tokio::join!(store.put(first.clone()), store.put(next.clone()));
    a.unwrap();
    b.unwrap();
    let observed = store.get(run_id).await;
    assert!(
        observed == Observed::Present(first) || observed == Observed::Present(next.clone()),
        "R5 whole frame"
    );
    store.put(next.clone()).await.unwrap();
    next
}

pub async fn delete_is_idempotent(store: &dyn StreamCheckpointStore, run_id: &str) {
    // R6 present -> delete -> Absent; R7 absent -> delete -> Absent.
    // Unknown must fail either assertion, never count as successful absence.
    store.delete(run_id).await.unwrap();
    assert_eq!(store.get(run_id).await, Observed::Absent, "R6");
    store.delete(run_id).await.unwrap();
    assert_eq!(store.get(run_id).await, Observed::Absent, "R7");
}
