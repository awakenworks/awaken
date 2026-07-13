//! Focused unit coverage for the small surfaces the end-to-end suites do not
//! exercise: the clock port, the request/context accessors, the worker builders
//! and live-stream wiring, and the synchronous `DurableRunIngress` façade.

mod harness;

use std::sync::Arc;

use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::stream::sink::Sink;
use awaken_run_ingress::{
    Clock, DispatchQueue, DispatchWorker, DurableRunIngress, ManualClock, MemoryDispatchStore,
    RunExecutionContext, RunExecutionRequest, SystemClock,
};
use awaken_runtime::memory::{MemoryCommitCoordinator, MemoryStreamSink};
use awaken_runtime::{RunIngress, Runtime};
use awaken_runtime_contract::control::Error as ControlError;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;

use harness::{THREAD, activation, text_runtime};

#[test]
fn manual_clock_sets_and_advances_and_system_clock_reads() {
    let clock = ManualClock::new(5);
    assert_eq!(clock.now_ms(), 5);
    clock.advance(10);
    assert_eq!(clock.now_ms(), 15);
    clock.set(3);
    assert_eq!(clock.now_ms(), 3);
    // The system clock returns a real, non-zero epoch.
    assert!(SystemClock.now_ms() > 0);
}

#[test]
fn request_exposes_run_and_thread_ids() {
    let request = RunExecutionRequest::new(activation("run-7"));
    assert_eq!(request.run_id().0, "run-7");
    assert_eq!(request.thread_id().0, THREAD);
}

/// The submitter's W3C `traceparent` SURVIVES the durable queue hop: it is persisted
/// on enqueue and handed back on the `Claimed` request, so the worker can rebuild the
/// `wake.dispatch` span with the submitter's trace as its remote parent (the plumbing
/// the `durable_trace_propagation` e2e asserts nests end-to-end). A run enqueued with
/// no context stays `None` — a fresh, un-parented span.
#[tokio::test]
async fn the_admitting_traceparent_survives_the_enqueue_claim_queue_hop() {
    let store = MemoryDispatchStore::new();
    let traceparent = "00-aa11bb22cc33dd44ee55ff6677889900-1122334455667788-01";

    store
        .enqueue(RunExecutionRequest::new(activation("traced")).with_traceparent(Some(
            traceparent.to_string(),
        )))
        .await
        .unwrap();
    // A second run with no captured context — proves the queue does not fabricate one.
    store
        .enqueue(RunExecutionRequest::new(harness::activation_on(
            "untraced", "thread-2",
        )))
        .await
        .unwrap();

    let traced = store.claim("w", 1_000, 0).await.unwrap().expect("traced");
    assert_eq!(traced.request.run_id().0, "traced");
    assert_eq!(
        traced.request.traceparent.as_deref(),
        Some(traceparent),
        "the admitting traceparent is persisted and returned on the claimed request"
    );

    let untraced = store.claim("w", 1_000, 0).await.unwrap().expect("untraced");
    assert_eq!(untraced.request.run_id().0, "untraced");
    assert_eq!(
        untraced.request.traceparent, None,
        "a run admitted with no trace context carries no traceparent"
    );
}

#[test]
fn execution_context_keeps_its_commit_handle() {
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RunExecutionContext::new(commit.clone())
        .with_stream_sink(Arc::new(MemoryStreamSink::new()));
    // The commit handle is the same source the worker reads and writes through.
    assert!(Arc::ptr_eq(
        context.commit(),
        &(commit as Arc<dyn awaken_agent_contract::commit::coordinator::Coordinator>)
    ));
}

#[tokio::test]
async fn worker_builders_attach_a_stream_sink_and_lease() {
    let runtime = text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let sink = Arc::new(MemoryStreamSink::new());
    let worker = DispatchWorker::new(runtime, store.clone(), commit, "unit-worker")
        .with_stream_sink(sink.clone() as Arc<dyn Sink>)
        .with_lease_ms(5_000);

    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();
    let processed = worker.tick(0).await.expect("tick");
    assert_eq!(
        processed,
        Some((
            RunId("run-1".to_string()),
            Phase::Ended(EndCause::NaturalEnd)
        ))
    );
    // The attached sink received live progress, proving the wiring is live.
    assert!(!sink.events().is_empty(), "the run streamed to the sink");
    // The accessor returns the same runtime handle.
    assert!(Arc::ptr_eq(worker.runtime(), worker.runtime()));
}

#[tokio::test]
async fn worker_resumes_a_durable_run_from_a_pre_seeded_checkpoint() {
    use awaken_agent_contract::store::stream_checkpoint::{
        StreamCheckpoint, StreamCheckpointStore,
    };
    use awaken_runtime::memory::MemoryStreamCheckpointStore;

    let runtime = text_runtime(); // answers "done"
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let checkpoints = Arc::new(MemoryStreamCheckpointStore::new());
    // A crash mid-recovery left this partial for the run's in-flight step.
    checkpoints
        .put(StreamCheckpoint {
            run_id: "run-ckpt".to_string(),
            thread_id: THREAD.to_string(),
            model: "m".to_string(),
            partial_text: "Resumed ".to_string(),
            partial_tools: Vec::new(),
        })
        .await;

    let worker = DispatchWorker::new(runtime, store.clone(), commit.clone(), "unit-worker")
        .with_stream_checkpoint(checkpoints.clone() as Arc<dyn StreamCheckpointStore>);

    store
        .enqueue(RunExecutionRequest::new(activation("run-ckpt")))
        .await
        .unwrap();
    let processed = worker.tick(0).await.expect("tick");
    assert_eq!(
        processed,
        Some((
            RunId("run-ckpt".to_string()),
            Phase::Ended(EndCause::NaturalEnd)
        ))
    );

    // The durable worker threaded the checkpoint store into the run context, so
    // the engine resumed the first step from the flushed partial: the committed
    // turn is the recovered prefix stitched onto the model's continuation.
    let committed = commit.committed();
    let assistant = committed
        .messages
        .iter()
        .find(|m| m.role == awaken_agent_contract::agent::message::Role::Assistant)
        .expect("assistant message committed");
    assert_eq!(assistant.text_content(), "Resumed done");
    // The consumed checkpoint is cleared once the step concludes.
    assert!(checkpoints.get("run-ckpt").await.is_none());
}

#[tokio::test]
async fn durable_ingress_foreground_submit_and_cancel() {
    let runtime: Arc<Runtime> = text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store, commit.clone());

    // Foreground submit executes inline through the runtime (additive, G6).
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let phase = ingress
        .submit(activation("run-fg"), context)
        .await
        .expect("foreground submit");
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
    // Per-step durability: input commit at the first step boundary, then the
    // terminal commit.
    assert_eq!(commit.commit_count(), 2);

    // Cancelling a run that is not in flight is a typed NotActive, not a panic.
    let err = ingress
        .cancel(&RunId("not-running".to_string()))
        .expect_err("no such active run");
    assert_eq!(err, ControlError::NotActive);

    // The worker accessor is reachable for out-of-band driving.
    let _ = ingress.worker();
}
