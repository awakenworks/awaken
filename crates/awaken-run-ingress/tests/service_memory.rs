//! The autonomous dispatch daemon over the in-memory store (M2).
//!
//! These prove the daemon drains submitted work without a caller driving it,
//! resumes a parked run on delivery, recovers a crashed lease on its own clock,
//! and shuts down cleanly.

mod harness;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::store::run_store::RunStore;
use awaken_run_ingress::{
    DispatchServiceConfig, DurableRunIngress, ManualClock, MemoryDispatchStore, PendingInput,
    RunDispatch, RunExecutionRequest, SystemClock,
};
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime_contract::resume::ResumeResult;

use harness::{THREAD, TICKET, activation, text_runtime, tool_runtime};

/// Poll a condition up to ~3s, yielding to the daemon between checks.
async fn wait_for(cond: impl Fn() -> bool) -> bool {
    for _ in 0..600 {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    cond()
}

fn pending(message_id: &str, run: &str, result: ResumeResult) -> PendingInput {
    PendingInput {
        message_id: message_id.to_string(),
        run_id: RunId(run.to_string()),
        thread_id: ThreadId(THREAD.to_string()),
        correlation_id: TICKET.to_string(),
        result,
    }
}

#[tokio::test]
async fn service_drains_submitted_runs_and_shuts_down() {
    let runtime = text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store.clone(), commit.clone());
    let service = ingress.spawn_service(Arc::new(SystemClock), DispatchServiceConfig::default());

    // Submit and let the daemon pick it up — the test never drives the worker.
    service.submit(activation("run-1")).await.expect("submit");
    assert!(
        wait_for(|| commit.commit_count() >= 1).await,
        "the daemon drained the submitted run"
    );
    assert_eq!(
        store.dispatch_count(),
        0,
        "the finished dispatch was removed"
    );

    service.shutdown().await;
}

#[tokio::test]
async fn service_resumes_a_parked_run_on_delivery() {
    let (runtime, ran) = tool_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store, commit.clone());
    let service = ingress.spawn_service(Arc::new(SystemClock), DispatchServiceConfig::default());

    // The daemon runs the submission until it parks on the gate.
    service.submit(activation("run-1")).await.expect("submit");
    assert!(
        wait_for(|| commit.commit_count() >= 1).await,
        "the run parked"
    );
    assert_eq!(ran.load(Ordering::SeqCst), 0);

    // Delivering input wakes the daemon, which resumes the run to completion.
    service
        .deliver(pending(
            "msg-1",
            "run-1",
            ResumeResult::Decision {
                allow: true,
                note: None,
            },
        ))
        .await
        .expect("deliver");
    assert!(
        wait_for(|| commit.commit_count() >= 2).await,
        "the daemon resumed the run"
    );
    assert_eq!(ran.load(Ordering::SeqCst), 1, "the pending tool ran once");
    let record = RunStore::get(commit.as_ref(), &RunId("run-1".to_string())).expect("record");
    assert_eq!(record.phase, Phase::Ended(EndCause::NaturalEnd));

    service.shutdown().await;
}

#[tokio::test]
async fn service_recovers_a_crashed_lease_on_its_clock() {
    let runtime = text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store.clone(), commit.clone());

    // Simulate a crashed worker: a claimed run with a held lease, nothing run.
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();
    assert!(
        store
            .claim("dead-worker", 1_000, 0)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(commit.commit_count(), 0);

    // A hand-driven clock: the daemon polls slowly, so only the clock advances it.
    let clock = Arc::new(ManualClock::new(0));
    let service = ingress.spawn_service(
        clock.clone(),
        DispatchServiceConfig {
            poll_interval: Duration::from_secs(10),
        },
    );

    // Advance past the lease and nudge: the daemon reclaims and runs the dispatch.
    clock.set(2_000);
    service.notify();
    assert!(
        wait_for(|| commit.commit_count() >= 1).await,
        "recovery ran the crashed dispatch"
    );
    assert_eq!(store.dispatch_count(), 0);

    service.shutdown().await;
}

#[tokio::test]
async fn shutdown_is_clean_with_no_work() {
    let runtime = text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store, commit);
    let service = ingress.spawn_service(Arc::new(SystemClock), DispatchServiceConfig::default());
    // No work submitted: shutdown still returns promptly.
    service.shutdown().await;
}
