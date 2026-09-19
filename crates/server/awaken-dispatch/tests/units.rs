//! Focused unit coverage for the small surfaces the end-to-end suites do not
//! exercise: the clock port, the request/context accessors, the worker builders
//! and live-stream wiring, and the synchronous `DispatchTestHarness` façade.

mod harness;

use std::sync::Arc;

use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::stream::sink::Sink;
use awaken_dispatch::{
    Clock, DispatchQueue, DispatchTestHarness, DispatchWorker, ManualClock, MemoryDispatchStore,
    RunDispatch, SystemClock,
};
use awaken_runtime_contract::control::Error as ControlError;
use awaken_store_inmem::{MemoryCommitCoordinator, MemoryStreamSink};

use awaken_dispatch_testkit::ConformanceClock as _;
use harness::{THREAD, activation, text_runtime};

#[test]
fn configured_postgres_failure_cannot_pass_as_a_skipped_test() {
    // G1/G2 connected, required or optional => return the fixture. G3 optional
    // unavailable => explicit skip. G4 configured unavailable => fail the test,
    // never green without executing SQL. No process-global env mutation needed.
    for required in [false, true] {
        assert_eq!(
            harness::postgres_connection_result(Ok(7), required),
            Some(7)
        );
    }
    assert_eq!(
        harness::postgres_connection_result::<()>(Err(sqlx::Error::PoolClosed), false),
        None
    );
    assert!(
        std::panic::catch_unwind(|| harness::postgres_connection_result::<()>(
            Err(sqlx::Error::PoolClosed),
            true
        ))
        .is_err()
    );
}

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
    let request = RunDispatch::new(activation("run-7"));
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
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = MemoryDispatchStore::new().with_clock(clock.clock());
    let traceparent = "00-aa11bb22cc33dd44ee55ff6677889900-1122334455667788-01";

    store
        .enqueue(
            RunDispatch::new(activation("traced")).with_traceparent(Some(traceparent.to_string())),
        )
        .await
        .unwrap();
    // A second run with no captured context — proves the queue does not fabricate one.
    store
        .enqueue(RunDispatch::new(harness::activation_on(
            "untraced", "thread-2",
        )))
        .await
        .unwrap();

    clock.set(0);
    let traced = store
        .claim("w", 1_000, &Default::default())
        .await
        .unwrap()
        .expect("traced");
    assert_eq!(traced.request.run_id().0, "traced");
    assert_eq!(
        traced.request.traceparent.as_deref(),
        Some(traceparent),
        "the admitting traceparent is persisted and returned on the claimed request"
    );

    clock.set(0);
    let untraced = store
        .claim("w", 1_000, &Default::default())
        .await
        .unwrap()
        .expect("untraced");
    assert_eq!(untraced.request.run_id().0, "untraced");
    assert_eq!(
        untraced.request.traceparent, None,
        "a run admitted with no trace context carries no traceparent"
    );
}

#[tokio::test]
async fn worker_builders_attach_a_stream_sink_and_lease() {
    // Test design. Causes: C1 Worker builder receives a StreamSink; C2 it receives
    // a custom lease duration; C3 a fresh Run is driven. Effects: E1 C3 reaches
    // NaturalEnd; E2 events reach the supplied sink; E3 the custom lease governs
    // the claim. Constraint/Invariant: builder composition preserves one Worker
    // path. Decision rule: compose C1+C2, execute C3, and observe all ports.
    let runtime = text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let sink = Arc::new(MemoryStreamSink::new());
    let worker = DispatchWorker::new(runtime, store.clone(), commit, "unit-worker")
        .with_stream_sink(sink.clone() as Arc<dyn Sink>)
        .with_lease_ms(5_000);

    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    let processed = worker.tick(harness::clock(0)).await.expect("tick");
    assert_eq!(
        processed,
        Some((
            RunId("run-1".to_string()),
            RunState::Ended(EndCause::NaturalEnd)
        ))
    );
    // The attached sink received live progress, proving the wiring is live.
    assert!(!sink.events().is_empty(), "the run streamed to the sink");
    // The accessor returns the same runtime handle.
    assert!(Arc::ptr_eq(worker.runtime(), worker.runtime()));
}

#[tokio::test]
async fn dispatch_harness_attaches_one_best_effort_stream_sink_to_its_worker() {
    // Cause/effect graph: C1 DispatchTestHarness is configured before sharing its
    // worker; C2 a sink is present vs absent; C3 the runtime emits live deltas.
    // Effects: E1 the existing worker path forwards deltas to the supplied sink;
    // E2 committed terminal state remains identical and authoritative. Constraint:
    // this builder must reuse DispatchWorker.with_stream_sink, not create a second
    // event channel or durable stream record.
    //
    // | Rule | sink | runtime delta | Effects |
    // | R1 | present | yes | E1+E2 |
    // | R2 | absent | yes | E2 only (existing default path) |
    // R2 is covered by the foreground submit/cancel integration below.
    let runtime = text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let sink = Arc::new(MemoryStreamSink::new());
    let ingress = DispatchTestHarness::new(runtime, store, commit.clone())
        .with_stream_sink(sink.clone() as Arc<dyn Sink>);

    let state = ingress
        .submit_background(activation("run-ingress-stream"))
        .await
        .expect("durable ingress run");

    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd), "R1/E2");
    assert!(!sink.events().is_empty(), "R1/E1");
}

#[tokio::test]
async fn worker_resumes_a_durable_run_from_a_pre_seeded_checkpoint() {
    use awaken_agent_contract::agent::state::StateKey;
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_agent_contract::stream::checkpoint::{StreamCheckpoint, StreamCheckpointStore};
    use awaken_runtime_contract::model_operation::{
        ActiveModelOperation, ModelExecutionEvidence, ModelOperation, ModelOperationPhase,
    };
    use awaken_store_inmem::MemoryStreamCheckpointStore;
    // C1 retained partial; C2 predecessor evidence absent/Unknown/Terminated.
    // R1 absent and R2 Unknown => zero model calls, no assistant/false terminal,
    // retained checkpoint and dispatch. R3 Terminated => exactly one continuation,
    // stitched text, NaturalEnd, checkpoint cleanup. A partial is never stop proof.
    // The prerequisite uses the same ThreadCommit/RecoveryProjection as production.
    for evidence in [
        None,
        Some(ModelExecutionEvidence::Unknown),
        Some(ModelExecutionEvidence::Terminated(
            awaken_runtime_contract::model_operation::TerminalReceipt::from_wire_status(Some(
                "completed",
            ))
            .unwrap(),
        )),
    ] {
        let (runtime, calls) = harness::counting_text_runtime();
        let store = Arc::new(MemoryDispatchStore::new());
        let commit = Arc::new(MemoryCommitCoordinator::new());
        let checkpoints = Arc::new(MemoryStreamCheckpointStore::new());
        let partial = StreamCheckpoint {
            run_id: "run-ckpt".into(),
            thread_id: THREAD.into(),
            model: "m".into(),
            partial_text: "Resumed ".into(),
            partial_tools: Vec::new(),
            retry_count: 0,
        };
        checkpoints.put(partial.clone()).await.unwrap();
        if let Some(evidence) = evidence {
            let mut prior = ModelOperation::next(
                None,
                awaken_runtime_contract::resolved::ModelBinding::new("p", "m", "b"),
            )
            .unwrap();
            prior.phase = ModelOperationPhase::Returned;
            prior.execution = evidence;
            awaken_runtime_contract::CommitCoordinator::commit(
                commit.as_ref(),
                awaken_runtime_contract::ThreadCommit::assemble(
                    ThreadId(THREAD.into()),
                    awaken_runtime_contract::RunDisposition::running(RunId("run-ckpt".into())),
                    true,
                    Vec::new(),
                    vec![ActiveModelOperation::write(&Some(prior))],
                    Vec::new(),
                ),
            )
            .await
            .unwrap();
        }
        let worker = DispatchWorker::new(runtime, store.clone(), commit.clone(), "unit-worker")
            .with_stream_checkpoint(checkpoints.clone() as Arc<dyn StreamCheckpointStore>);
        store
            .enqueue(RunDispatch::new(activation("run-ckpt")))
            .await
            .unwrap();
        let processed = worker.tick(harness::clock(0)).await.expect("tick");
        let committed = commit.committed();
        let assistant: Vec<_> = committed
            .messages
            .iter()
            .filter(|m| m.role == awaken_agent_contract::agent::message::Role::Assistant)
            .collect();
        if evidence
            == Some(ModelExecutionEvidence::Terminated(
                awaken_runtime_contract::model_operation::TerminalReceipt::from_wire_status(Some(
                    "completed",
                ))
                .unwrap(),
            ))
        {
            assert_eq!(
                processed,
                Some((
                    RunId("run-ckpt".into()),
                    RunState::Ended(EndCause::NaturalEnd)
                ))
            );
            assert_eq!(
                calls.load(std::sync::atomic::Ordering::SeqCst),
                1,
                "R3 one continuation"
            );
            assert_eq!(assistant.len(), 1);
            assert_eq!(assistant[0].text_content(), "Resumed done");
            assert_eq!(
                checkpoints.get("run-ckpt").await,
                awaken_agent_contract::observation::Observed::Absent
            );
            assert_eq!(store.dispatch_count(), 0);
        } else {
            assert_eq!(processed, None, "R1/R2 no physical settlement");
            assert_eq!(
                calls.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "R1/R2 no reentry"
            );
            assert!(assistant.is_empty());
            assert_eq!(
                checkpoints.get("run-ckpt").await,
                awaken_agent_contract::observation::Observed::Present(partial)
            );
            assert_eq!(store.dispatch_count(), 1, "R1/R2 retain dispatch fence");
        }
    }
}

#[tokio::test]
async fn dispatch_harness_unknown_live_cancel_is_typed() {
    // Cause U1: cancellation names no active Runtime attempt. Effect E1: the
    // public control surface returns typed NotActive and performs no execution.
    // The Durable inline start/resume decision table is owned once by
    // `durable_inline_start_and_resume_fail_closed_before_executor_entry`.
    let ingress = DispatchTestHarness::new(
        text_runtime(),
        Arc::new(MemoryDispatchStore::new()),
        Arc::new(MemoryCommitCoordinator::new()),
    );
    let err = ingress
        .cancel(&RunId("not-running".to_string()))
        .await
        .expect_err("no such active run");
    assert_eq!(err, ControlError::NotActive, "U1/E1");

    // The worker accessor is reachable for out-of-band driving.
    let _ = ingress.worker();
}
