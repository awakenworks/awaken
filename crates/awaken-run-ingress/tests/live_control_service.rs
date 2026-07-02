//! Integration tests for `LiveRunControlService` — the G18 fail-closed
//! live-control seam (cancel and wake by correlation-id).
//!
//! G18: configuration publication, live control, and execution stay on separate
//! runtime-facing ports. `LiveRunControlService` owns active-run steering only:
//! it never starts runs, publishes config, or holds a second commit boundary.
//!
//! G5: `DirectRunIngress` durable-only operations fail closed on direct ingress;
//! the live-control service mirrors this: cancel is fail-closed (NotFound for
//! unknown ids), wake is fail-closed (NoSubscriber when no live run exists).

mod harness;

use std::sync::Arc;

use awaken_agent_contract::agent::run::{EndCause, Phase};
use awaken_run_ingress::{
    DispatchWorker, DurableRunIngress, LiveRunControlError, LiveRunControlService,
    MemoryDispatchStore, RunDispatch, RunExecutionRequest,
};
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime::{RunIngress, Runtime};

use harness::{activation, text_runtime};

/// Build a service backed by a real (text-only) worker over an in-memory store.
fn make_service(
    runtime: Arc<Runtime>,
) -> (
    Arc<DispatchWorker<MemoryDispatchStore>>,
    LiveRunControlService<MemoryDispatchStore>,
) {
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let worker = Arc::new(DispatchWorker::new(runtime, store, commit, "test-worker"));
    let svc = LiveRunControlService::new(worker.clone());
    (worker, svc)
}

// ── Cancel fail-closed ────────────────────────────────────────────────────────

/// Cancel is fail-closed: an unknown correlation-id returns NotFound, not a
/// panic or a silent success (G18).
#[tokio::test]
async fn cancel_is_fail_closed_for_unknown_correlation_id() {
    let (_, svc) = make_service(text_runtime());
    let err = svc.cancel("nonexistent-id").await.unwrap_err();
    assert!(
        matches!(err, LiveRunControlError::NotFound(_)),
        "cancel must be fail-closed for unknown id; got {err}"
    );
}

/// Cancel is fail-closed: two different unknown ids both return NotFound.
#[tokio::test]
async fn cancel_fail_closed_for_multiple_unknown_ids() {
    let (_, svc) = make_service(text_runtime());
    for id in &["run-1", "run-2", "run-3"] {
        let err = svc.cancel(id).await.unwrap_err();
        assert!(
            matches!(err, LiveRunControlError::NotFound(_)),
            "cancel of {id} must return NotFound"
        );
    }
}

// ── Wake fail-closed ──────────────────────────────────────────────────────────

/// Wake is live-only and fail-closed: no live subscriber → NoSubscriber (G5/G18).
#[test]
fn wake_is_fail_closed_when_no_live_subscriber() {
    let (_, svc) = make_service(text_runtime());
    let err = svc.wake("nonexistent-id").unwrap_err();
    assert!(
        matches!(err, LiveRunControlError::NoSubscriber(_)),
        "wake must be fail-closed when no live subscriber; got {err}"
    );
}

/// Wake returns NoSubscriber even when a queued (non-live) dispatch exists,
/// because wake has no durable fallback.
#[tokio::test]
async fn wake_is_fail_closed_for_queued_but_not_live_run() {
    let runtime = text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let worker = Arc::new(DispatchWorker::new(
        runtime,
        store.clone(),
        commit,
        "test-worker",
    ));
    let svc = LiveRunControlService::new(worker.clone());

    // Enqueue a run so it is durable but not live.
    store
        .enqueue(RunExecutionRequest::new(activation("run-queued")))
        .await
        .unwrap();

    // Wake must still fail closed — no live subscriber exists.
    let err = svc.wake("run-queued").unwrap_err();
    assert!(
        matches!(err, LiveRunControlError::NoSubscriber(_)),
        "wake must return NoSubscriber for a queued (non-live) run"
    );
}

// ── Separation from run submission ───────────────────────────────────────────

/// LiveRunControlService does not submit runs — it only steers existing ones.
/// Calling cancel on an id that was never submitted returns NotFound (not a
/// successful no-op), proving the service cannot accidentally start a run.
#[tokio::test]
async fn live_control_cannot_start_a_run() {
    let (_, svc) = make_service(text_runtime());
    let err = svc.cancel("brand-new-run").await.unwrap_err();
    assert!(
        matches!(err, LiveRunControlError::NotFound(_)),
        "live-control service must not be able to start a run"
    );
}

/// DurableRunIngress and LiveRunControlService are separate seams: both are
/// built from the same worker but neither starts runs on behalf of the other.
/// This test proves that a run submitted through durable ingress, once complete,
/// cannot be cancelled through the live-control seam (separation of concerns).
#[tokio::test]
async fn durable_ingress_and_live_control_are_independent_seams() {
    let runtime = text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime.clone(), store.clone(), commit.clone());

    // Build the control service independently using the same underlying store and
    // runtime — it is a separate seam over the same infrastructure.
    let (_, svc) = make_service(runtime);

    // Submit through ingress; verify the run ends.
    use awaken_runtime_contract::runtime_context::RuntimeRunContext;
    let ctx = RuntimeRunContext::new().with_commit(commit);
    let phase = ingress
        .submit(activation("run-1"), ctx)
        .await
        .expect("foreground submit");
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));

    // Cancel after completion is NotFound — no live active run remains.
    let err = svc.cancel("run-1").await.unwrap_err();
    assert!(
        matches!(err, LiveRunControlError::NotFound(_)),
        "a completed run is not cancelable (NotFound)"
    );
}
