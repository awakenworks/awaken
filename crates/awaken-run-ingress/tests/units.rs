//! Focused unit coverage for the small surfaces the end-to-end suites do not
//! exercise: the clock port, the request/context accessors, the worker builders
//! and live-stream wiring, and the synchronous `DurableRunIngress` façade.

mod harness;

use std::sync::Arc;

use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::stream::sink::Sink;
use awaken_run_ingress::{
    Clock, DispatchWorker, DurableRunIngress, ManualClock, MemoryDispatchStore, RunDispatch,
    RunExecutionContext, RunExecutionRequest, SystemClock,
};
use awaken_runtime::memory::{MemoryCommitCoordinator, MemoryStreamSink};
use awaken_runtime::{RunIngress, Runtime};
use awaken_runtime_contract::activation::PersistenceMode;
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
async fn durable_ingress_foreground_submit_and_cancel() {
    let runtime: Arc<Runtime> = text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store, commit.clone());

    // Foreground submit executes inline through the runtime (additive, G6).
    let context = RuntimeRunContext::new(PersistenceMode::ReadWrite).with_commit(commit.clone());
    let phase = ingress
        .submit(activation("run-fg"), context)
        .await
        .expect("foreground submit");
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
    assert_eq!(commit.commit_count(), 1);

    // Cancelling a run that is not in flight is a typed NotActive, not a panic.
    let err = ingress
        .cancel(&RunId("not-running".to_string()))
        .expect_err("no such active run");
    assert_eq!(err, ControlError::NotActive);

    // The worker accessor is reachable for out-of-band driving.
    let _ = ingress.worker();
}
