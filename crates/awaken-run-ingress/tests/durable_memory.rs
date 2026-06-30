//! Durable ingress over the in-memory dispatch store and commit coordinator.
//!
//! These prove the durable slice end to end without a database: a durable submit
//! persists then runs a fresh run; the durable-only operation fails closed on
//! direct ingress (G5); enqueue and pending append are idempotent; a parked run
//! resumes through delivered input (#4); and an expired lease is recovered.

mod harness;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use awaken_agent_contract::agent::message::Role;
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_run_ingress::{
    DispatchOutcome, DurableRunIngress, MemoryDispatchStore, PendingInbox, PendingInput,
    RunDispatch, RunExecutionRequest, RunIngressCapabilities,
};
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime::{DirectRunIngress, RunIngress};
use awaken_runtime_contract::activation::PersistenceMode;
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;

use harness::{FP, SNAP, THREAD, TICKET, activation, text_runtime, tool_runtime};

fn allow_command() -> ResumeCommand {
    ResumeCommand {
        correlation_id: TICKET.to_string(),
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId(THREAD.to_string()),
        snapshot_id: SNAP.to_string(),
        catalog_fingerprint: FP.to_string(),
        result: ResumeResult::Decision {
            allow: true,
            note: None,
        },
        now_ms: 0,
    }
}

/// Pending input answering the gate's ticket (the common case).
fn pending(message_id: &str, run: &str, result: ResumeResult) -> PendingInput {
    pending_for(message_id, run, TICKET, result)
}

/// Pending input answering a specific ticket correlation.
fn pending_for(
    message_id: &str,
    run: &str,
    correlation: &str,
    result: ResumeResult,
) -> PendingInput {
    PendingInput {
        message_id: message_id.to_string(),
        run_id: RunId(run.to_string()),
        thread_id: ThreadId(THREAD.to_string()),
        correlation_id: correlation.to_string(),
        available_at_ms: None,
        result,
    }
}

#[tokio::test]
async fn durable_submit_persists_then_runs_to_completion() {
    let runtime = text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store.clone(), commit.clone());

    let phase = ingress
        .submit_background(activation("run-1"))
        .await
        .expect("durable submit");
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));

    // Committed truth holds the run; the dispatch was settled and removed.
    assert_eq!(commit.commit_count(), 1);
    assert_eq!(commit.committed().messages[0].text_content(), "done");
    assert_eq!(store.dispatch_count(), 0, "a finished dispatch is removed");
}

#[tokio::test]
async fn direct_ingress_fails_durable_submit_closed_while_durable_does_not() {
    // G5: the durable-only operation (submit_background) fails closed on direct
    // ingress and succeeds on durable ingress.
    let runtime = text_runtime();
    let direct = DirectRunIngress::new(runtime.clone());
    let err = direct
        .submit_background(activation("run-1"))
        .await
        .expect_err("direct has no durable submit");
    assert!(matches!(
        err,
        awaken_runtime_contract::execution::Error::Execution(_)
    ));

    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let durable = DurableRunIngress::new(runtime, store, commit);
    assert_eq!(durable.capabilities(), RunIngressCapabilities::DURABLE);
    assert!(durable.submit_background(activation("run-1")).await.is_ok());
}

#[tokio::test]
async fn durable_submit_is_idempotent_per_run() {
    let runtime = text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store, commit.clone());

    ingress
        .submit_background(activation("run-1"))
        .await
        .expect("first submit");
    // Re-submitting the same run id is a no-op: the dispatch was already settled,
    // and re-enqueue does not create a second run or a second commit.
    let phase = ingress
        .submit_background(activation("run-1"))
        .await
        .expect("second submit");
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
    assert_eq!(commit.commit_count(), 1, "the run committed exactly once");
}

#[tokio::test]
async fn parked_run_resumes_through_delivered_input() {
    let (runtime, ran) = tool_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store.clone(), commit.clone());

    // A durable submit parks on the gate.
    let phase = ingress
        .submit_background(activation("run-1"))
        .await
        .expect("submit parks");
    assert_eq!(phase, Phase::Waiting);
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "tool must not run while parked"
    );
    assert_eq!(store.dispatch_count(), 1, "the parked dispatch is retained");

    // Delivering an allow decision wakes and resumes the run to completion.
    let resumed = ingress
        .deliver_resume(
            pending(
                "msg-1",
                "run-1",
                ResumeResult::Decision {
                    allow: true,
                    note: None,
                },
            ),
            0,
        )
        .await
        .expect("resume");
    assert_eq!(resumed, Phase::Ended(EndCause::NaturalEnd));
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "allow runs the pending tool once"
    );
    assert_eq!(
        store.dispatch_count(),
        0,
        "the finished dispatch is removed"
    );
    assert!(
        commit
            .committed()
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.text_content().contains("echoed"))
    );
}

#[tokio::test]
async fn duplicate_pending_delivery_is_idempotent() {
    let store = MemoryDispatchStore::new();
    let input = pending("msg-1", "run-1", ResumeResult::Input("hi".to_string()));
    assert!(
        store.append(input.clone()).await.unwrap(),
        "first append stores"
    );
    assert!(
        !store.append(input).await.unwrap(),
        "a duplicate message id is a no-op"
    );
    assert_eq!(store.pending_count(&RunId("run-1".to_string())), 1);
}

#[tokio::test]
async fn expired_lease_is_reclaimable_for_recovery() {
    let store = MemoryDispatchStore::new();
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();

    // First claim takes a 1000ms lease at t=0.
    assert!(
        store.claim("worker-a", 1_000, 0).await.unwrap().is_some(),
        "a fresh run is claimable"
    );
    // While the lease holds, the run is not re-claimable.
    assert!(
        store.claim("worker-b", 1_000, 500).await.unwrap().is_none(),
        "a held lease blocks a second claim"
    );
    // After the lease expires, recovery reclaims it.
    let recovered = store.claim("worker-b", 1_000, 1_001).await.unwrap();
    assert_eq!(
        recovered.map(|c| c.lease.owner),
        Some("worker-b".to_string()),
        "an expired lease is reclaimed by the next worker"
    );
}

#[tokio::test]
async fn worker_recovery_runs_a_crashed_dispatch_to_completion() {
    let runtime = text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store.clone(), commit.clone());

    // Simulate a worker that claimed a run, then crashed before executing it:
    // enqueue and claim directly, leaving a held lease and no committed run.
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
    assert_eq!(
        commit.commit_count(),
        0,
        "the crashed attempt committed nothing"
    );

    // Recovery after the lease expires reclaims and completes the run.
    let processed = ingress.recover(2_000).await.expect("recover");
    assert_eq!(
        processed,
        vec![(
            RunId("run-1".to_string()),
            Phase::Ended(EndCause::NaturalEnd)
        )]
    );
    assert_eq!(
        commit.commit_count(),
        1,
        "recovery ran the run exactly once"
    );
    assert_eq!(store.dispatch_count(), 0);
}

#[tokio::test]
async fn settle_done_clears_pending_and_dispatch() {
    // A store-level invariant: settling Done removes the dispatch and any pending.
    let store = MemoryDispatchStore::new();
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();
    store
        .append(pending(
            "msg-1",
            "run-1",
            ResumeResult::Input("x".to_string()),
        ))
        .await
        .unwrap();
    store
        .settle(&RunId("run-1".to_string()), DispatchOutcome::Done, &[])
        .await
        .unwrap();
    assert_eq!(store.dispatch_count(), 0);
    assert_eq!(store.pending_count(&RunId("run-1".to_string())), 0);
}

#[tokio::test]
async fn committed_resume_is_not_reapplied_after_a_crash(/* M1 */) {
    // Model the crash window: a resume commits, but the worker dies before it
    // settles. The dispatch is left 'running' with the pending input still
    // present and the ticket already cleared. Recovery must NOT re-run the tool.
    let (runtime, ran) = tool_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime.clone(), store.clone(), commit.clone());

    // Park, then deliver input WITHOUT driving (just append).
    assert_eq!(
        ingress
            .submit_background(activation("run-1"))
            .await
            .unwrap(),
        Phase::Waiting
    );
    store
        .append(pending(
            "msg-1",
            "run-1",
            ResumeResult::Decision {
                allow: true,
                note: None,
            },
        ))
        .await
        .unwrap();

    // Worker got partway: it claimed (took a lease) and committed the resume,
    // then crashed before settle. Drive those two steps by hand.
    let _claimed = store.claim("dead-worker", 1_000, 0).await.unwrap();
    let context = RuntimeRunContext::new(PersistenceMode::ReadWrite).with_commit(commit.clone());
    let phase = runtime
        .resume(allow_command(), commit.as_ref(), context)
        .await
        .expect("resume commits");
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "the tool ran once before the crash"
    );

    // Recovery after the lease expires: the committed run is terminal, so the
    // worker settles it without re-running the tool, and clears the pending.
    let processed = ingress.recover(2_000).await.expect("recover");
    assert_eq!(
        processed,
        vec![(
            RunId("run-1".to_string()),
            Phase::Ended(EndCause::NaturalEnd)
        )]
    );
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "recovery did not re-apply the resume"
    );
    assert_eq!(store.dispatch_count(), 0);
    assert_eq!(store.pending_count(&RunId("run-1".to_string())), 0);
}

#[tokio::test]
async fn input_for_a_superseded_ticket_is_not_delivered(/* M1 */) {
    // Input whose correlation does not match the run's committed ticket is stale;
    // it is dropped without delivery, and the run stays parked until the right
    // input arrives.
    let (runtime, ran) = tool_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store.clone(), commit);

    assert_eq!(
        ingress
            .submit_background(activation("run-1"))
            .await
            .unwrap(),
        Phase::Waiting
    );

    // Stale input (wrong correlation) does not resume the run.
    let phase = ingress
        .deliver_resume(
            pending_for(
                "stale",
                "run-1",
                "some-old-ticket",
                ResumeResult::Decision {
                    allow: true,
                    note: None,
                },
            ),
            0,
        )
        .await
        .expect("stale delivery");
    assert_eq!(phase, Phase::Waiting, "a stale input leaves the run parked");
    assert_eq!(ran.load(Ordering::SeqCst), 0, "the tool did not run");
    assert_eq!(
        store.pending_count(&RunId("run-1".to_string())),
        0,
        "the stale input was dropped"
    );

    // The correctly-correlated input resumes the run.
    let phase = ingress
        .deliver_resume(
            pending(
                "good",
                "run-1",
                ResumeResult::Decision {
                    allow: true,
                    note: None,
                },
            ),
            0,
        )
        .await
        .expect("good delivery");
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
    assert_eq!(ran.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn pending_edit_and_retract_are_revision_guarded() {
    // M3a: the in-memory store is the spec for revision-guarded pending ops.
    harness::assert_pending_revision_cas(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn cross_thread_outbox_store_spec() {
    harness::assert_cross_thread_outbox(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn staged_delivery_relays_and_resumes_a_parked_run() {
    // M3b end to end: a parked run is resumed by a cross-thread delivery that is
    // staged in the outbox and relayed to its pending input.
    let (runtime, ran) = tool_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store.clone(), commit.clone());

    assert_eq!(
        ingress
            .submit_background(activation("run-1"))
            .await
            .unwrap(),
        Phase::Waiting
    );

    // Stage the delivery (as if from another thread); it is not pending yet.
    let staged = ingress
        .stage_cross_thread(pending(
            "x1",
            "run-1",
            ResumeResult::Decision {
                allow: true,
                note: None,
            },
        ))
        .await
        .unwrap();
    assert!(staged);
    assert_eq!(store.pending_count(&RunId("run-1".to_string())), 0);

    // Relay moves it to pending and drives the run to completion.
    let processed = ingress.relay_outbox(0).await.expect("relay");
    assert_eq!(
        processed,
        vec![(
            RunId("run-1".to_string()),
            Phase::Ended(EndCause::NaturalEnd)
        )]
    );
    assert_eq!(ran.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn scheduled_delivery_due_store_spec() {
    harness::assert_scheduled_due(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn dead_letter_budget_store_spec() {
    harness::assert_dead_letter(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn cancel_store_spec() {
    harness::assert_cancel(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn cancel_durable_commits_cancelled_for_a_parked_run() {
    let (runtime, ran) = tool_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store.clone(), commit.clone());

    assert_eq!(
        ingress
            .submit_background(activation("run-1"))
            .await
            .unwrap(),
        Phase::Waiting
    );
    assert!(commit.waiting_for(&RunId("run-1".to_string())).is_some());

    // Durable cancel commits a terminal Cancelled and clears the ticket.
    assert!(
        ingress
            .cancel_durable(&RunId("run-1".to_string()))
            .await
            .unwrap()
    );
    let record = awaken_agent_contract::store::run_store::RunStore::get(
        commit.as_ref(),
        &RunId("run-1".to_string()),
    )
    .expect("run record");
    assert_eq!(record.phase, Phase::Ended(EndCause::Cancelled));
    assert!(commit.waiting_for(&RunId("run-1".to_string())).is_none());
    assert_eq!(store.dispatch_count(), 0);
    assert_eq!(ran.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancel_durable_for_a_queued_run_that_never_ran() {
    let runtime = text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store.clone(), commit.clone());

    // Enqueue without driving, then cancel: a terminal Cancelled is committed
    // even though the run never executed.
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();
    assert!(
        ingress
            .cancel_durable(&RunId("run-1".to_string()))
            .await
            .unwrap()
    );
    let record = awaken_agent_contract::store::run_store::RunStore::get(
        commit.as_ref(),
        &RunId("run-1".to_string()),
    )
    .expect("run record");
    assert_eq!(record.phase, Phase::Ended(EndCause::Cancelled));
    assert_eq!(store.dispatch_count(), 0);

    // Cancelling again is a no-op.
    assert!(
        !ingress
            .cancel_durable(&RunId("run-1".to_string()))
            .await
            .unwrap()
    );
}
