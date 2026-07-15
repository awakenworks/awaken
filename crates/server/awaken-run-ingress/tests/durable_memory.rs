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
use awaken_ext_builtin_tools::MessageSender;
use awaken_run_ingress::{
    DispatchOutcome, DispatchQueue, DispatchWorker, DurableRunIngress, Inbox, MemoryDispatchStore,
    OutboxMessageSender, PendingInput, RunExecutionRequest, RunIngressCapabilities,
};
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime::{DirectRunIngress, RunIngress};
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;

use harness::{
    FP, SNAP, THREAD, TICKET, activation, input_echo_runtime, schedule_runtime, text_runtime,
    tool_runtime,
};

fn allow_command() -> ResumeCommand {
    ResumeCommand {
        correlation_id: TICKET.to_string(),
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId(THREAD.to_string()),
        snapshot_id: awaken_runtime_contract::ExecutableAgentSnapshotId(SNAP.to_string()),
        catalog_fingerprint: awaken_runtime_contract::CatalogFingerprint(FP.to_string()),
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
    harness::pending(message_id, run, correlation, result)
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

    // Committed truth holds the run: the user turn then the assistant reply.
    // Per-step durability: the input commits at the first step boundary, the
    // terminal turn through finish.
    assert_eq!(commit.commit_count(), 2);
    let messages = commit.committed().messages;
    assert_eq!(messages[0].text_content(), "go");
    assert_eq!(messages.last().unwrap().text_content(), "done");
    assert_eq!(store.dispatch_count(), 0, "a finished dispatch is removed");
}

#[tokio::test]
async fn a_worker_routes_inference_through_the_resolved_model_executor() {
    // Cause: the worker carries a model resolver that maps the run's binding model_ref
    // to a labeled executor. Effect: the drive routes inference through THAT executor,
    // not the runtime's bound default — the committed reply is the resolved model's
    // ("RESOLVED"), never the runtime default ("done"). This is the per-run provider
    // seam a database-less worker uses to run the run's own configured model.
    use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};

    struct Labeled;
    #[async_trait::async_trait]
    impl LlmExecutor for Labeled {
        async fn infer(
            &self,
            _r: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            Ok(ChatResponse {
                output: AssistantOutput::text("RESOLVED"),
                usage: None,
                stop_reason: None,
            })
        }
    }

    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let resolver: awaken_run_ingress::ModelResolverFn =
        Arc::new(|_model_ref| Some(Arc::new(Labeled) as Arc<dyn LlmExecutor>));
    let ingress = DurableRunIngress::with_owner_and_resolver(
        text_runtime(), // its bound model would reply "done"
        store.clone(),
        commit.clone(),
        "owner",
        None,
        Some(resolver),
    );

    let phase = ingress
        .submit_background(activation("run-resolved"))
        .await
        .expect("durable submit");
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));

    let messages = commit.committed().messages;
    assert_eq!(
        messages.last().unwrap().text_content(),
        "RESOLVED",
        "the worker ran the resolved model, not the runtime's bound default"
    );
}

#[tokio::test]
async fn durable_run_drains_live_inbox_steer_at_the_boundary() {
    // ADR-0054 P2: a steer message offered into the durable ingress's per-session
    // inbox is drained by the worker-driven run at its safe loop boundary — steer
    // reaches a durable (worker-driven) run, not only the direct native path.
    use awaken_agent_contract::agent::message::{Id as MessageId, Message};
    use awaken_runtime_contract::live_inbox::MessageOrigin;

    let runtime = text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store, commit.clone());

    // Queue a steer before the run is driven; the worker drains the *same* inbox.
    let _ = ingress.live_inbox().offer_as(
        MessageOrigin::External,
        Message::text(MessageId("client-id".into()), Role::User, "steer me"),
    );

    let phase = ingress
        .submit_background(activation("run-1"))
        .await
        .expect("durable submit");
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));

    let messages = commit.committed().messages;
    let steer = messages
        .iter()
        .find(|m| m.id.0 == "run-1-inbox-0")
        .expect("steer drained + re-identified into the durable transcript");
    assert_eq!(steer.text_content(), "steer me");
    // The caller-supplied id never reaches the committed transcript.
    assert!(messages.iter().all(|m| m.id.0 != "client-id"));
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
    // Two commits (input + terminal) from the first submit; the re-submit
    // adds none.
    assert_eq!(commit.commit_count(), 2, "the run committed exactly once");
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
        2,
        "recovery ran the run exactly once (input + terminal commits)"
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
    // Never claimed, so the row's fence epoch is the default 0; settle under it.
    store
        .settle(&RunId("run-1".to_string()), 0, DispatchOutcome::Done, &[])
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
    let context = RuntimeRunContext::new().with_commit(commit.clone());
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

#[tokio::test]
async fn send_message_delivers_to_a_threads_parked_run() {
    use awaken_agent_contract::store::thread_reader::ThreadReader;
    use awaken_ext_builtin_tools::MessageSender;
    use awaken_run_ingress::OutboxMessageSender;

    let (runtime, ran) = tool_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store.clone(), commit.clone());

    // A run parks on thread-1, waiting for input.
    assert_eq!(
        ingress
            .submit_background(activation("run-1"))
            .await
            .unwrap(),
        Phase::Waiting
    );

    // The send_message host adapter, addressed by thread, stages a delivery.
    let sender = OutboxMessageSender::new(store.clone(), commit.clone() as Arc<dyn ThreadReader>);
    sender
        .send("thread-1", "hello from another agent")
        .await
        .expect("send to a waiting thread");
    // Sending to a thread with no waiting run is staged unbound (ADR-0021), not
    // an error: it is held for that thread's next run.
    sender
        .send("thread-2", "for later")
        .await
        .expect("idle-thread send is queued");

    // Relaying the outbox resumes the parked run with the message as input.
    let processed = ingress.relay_outbox(0).await.expect("relay");
    assert_eq!(
        processed,
        vec![(
            RunId("run-1".to_string()),
            Phase::Ended(EndCause::NaturalEnd)
        )]
    );
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "an input resume does not run the gated tool"
    );
    assert!(
        commit
            .committed()
            .messages
            .iter()
            .any(|m| m.text_content() == "hello from another agent")
    );
}

#[tokio::test]
async fn priority_dedupe_gc_store_spec() {
    harness::assert_priority_dedupe_gc(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn lease_renewal_store_spec() {
    harness::assert_lease_renewal(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn local_wake_signal_delivers_a_held_hint() {
    use awaken_run_ingress::{LocalWakeSignal, WakeSignal};
    let wake = LocalWakeSignal::new();
    // A hint published before anyone waits is held (one permit), so wait returns.
    wake.publish().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), wake.wait())
        .await
        .expect("the wake hint was delivered");
}

#[tokio::test]
async fn ingress_dead_letter_and_purge_ops() {
    let runtime = text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store.clone(), commit);

    // A crashed run reaped through the ingress API.
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();
    assert!(store.claim("w", 1, 0).await.unwrap().is_some());
    assert_eq!(ingress.reap(0, 100).await.unwrap(), 1);
    assert_eq!(
        ingress.dead_letters().await.unwrap(),
        vec![RunId("run-1".to_string())]
    );

    // Requeue, re-reap, then GC through the ingress API.
    assert!(ingress.requeue(&RunId("run-1".to_string())).await.unwrap());
    assert!(ingress.dead_letters().await.unwrap().is_empty());
    assert!(store.claim("w", 1, 0).await.unwrap().is_some());
    assert_eq!(ingress.reap(0, 200).await.unwrap(), 1);
    assert_eq!(ingress.purge_dead_letters().await.unwrap(), 1);
    assert!(ingress.dead_letters().await.unwrap().is_empty());

    // Cancelling a run that no longer exists is false.
    assert!(
        !ingress
            .cancel_durable(&RunId("run-1".to_string()))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn daemon_performs_a_scheduled_action_to_completion() {
    // RS-SCH-001 over the dispatch: a durably-submitted run whose gate defers the
    // tool parks on a committed ScheduledAction; the worker performs it in-process
    // (no external input) and the run settles Done.
    let (runtime, ran) = schedule_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store.clone(), commit);

    let phase = ingress
        .submit_background(activation("run-1"))
        .await
        .unwrap();
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "the scheduled action ran once"
    );
    assert_eq!(
        store.dispatch_count(),
        0,
        "the run settled Done, not Parked"
    );
}

#[tokio::test]
async fn a_recovered_scheduled_action_is_performed() {
    // RS-SCH-006: a run that committed a ScheduledAction park and then crashed
    // before performing it (dispatch left 'running' with an expired lease) is
    // recovered by another worker and performed from the committed request.
    let (runtime, ran) = schedule_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());

    // The run parked on a committed ScheduledAction (the action has not run).
    let ctx = RuntimeRunContext::new().with_commit(commit.clone());
    let phase = runtime.execute(activation("run-1"), ctx).await.unwrap();
    assert_eq!(phase, Phase::Waiting);
    assert_eq!(ran.load(Ordering::SeqCst), 0);

    // Its dispatch is a crashed in-flight claim: 'running', lease expired at 10,
    // never settled.
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();
    store.claim("dead-worker", 10, 0).await.unwrap();

    // A live worker recovers it after the lease expires and performs the action.
    let worker = DispatchWorker::new(runtime, store.clone(), commit, "live-worker");
    let processed = worker.tick(100).await.unwrap();
    assert_eq!(
        processed,
        Some((
            RunId("run-1".to_string()),
            Phase::Ended(EndCause::NaturalEnd)
        ))
    );
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "the recovered action ran once"
    );
    assert_eq!(store.dispatch_count(), 0, "settled Done after recovery");
}

#[tokio::test]
async fn send_message_to_an_idle_thread_feeds_the_next_run() {
    // ADR-0021: a message to a thread with no parked run is queued unbound, then
    // consumed by the thread's next run as new input.
    let runtime = input_echo_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());

    // Agent A messages a thread with no run in flight: it is staged unbound.
    let sender = OutboxMessageSender::new(store.clone(), commit.clone());
    sender
        .send(THREAD, "hello from A")
        .await
        .expect("send to idle thread");

    let ingress = DurableRunIngress::new(runtime, store.clone(), commit.clone());
    ingress.relay_outbox(0).await.expect("relay");
    let listed = store.list(&ThreadId(THREAD.to_string())).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert!(
        listed[0].input.run_id.0.is_empty(),
        "queued input is unbound"
    );

    // The thread's next run consumes the queued message as new input.
    let phase = ingress
        .submit_background(activation("run-1"))
        .await
        .expect("submit");
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
    let assistant = commit
        .committed()
        .messages
        .iter()
        .find(|m| m.role == Role::Assistant)
        .expect("assistant reply")
        .clone();
    assert!(
        assistant.text_content().contains("hello from A"),
        "the idle-thread message reached the run input"
    );
    assert!(
        store
            .list(&ThreadId(THREAD.to_string()))
            .await
            .unwrap()
            .is_empty(),
        "the unbound input is consumed"
    );
}

#[tokio::test]
async fn supersession_store_spec() {
    harness::assert_supersession(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn settle_fences_stale_epoch_store_spec() {
    harness::assert_settle_fences_stale_epoch(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn concurrent_recovery_yields_one_winner_on_memory() {
    harness::assert_concurrent_recovery_yields_one_winner(std::sync::Arc::new(
        MemoryDispatchStore::new(),
    ))
    .await;
}

#[tokio::test]
async fn parked_settle_fences_stale_epoch_store_spec() {
    harness::assert_parked_settle_fences_stale_epoch(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn submit_superseding_abandons_prior_thread_work() {
    // ADR-0022 at the ingress: a superseding submit supersedes the thread's
    // parked run; only the newest run stays live.
    let (runtime, _ran) = tool_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store.clone(), commit.clone());

    // An older run parks on the thread.
    assert_eq!(
        ingress.submit_background(activation("old")).await.unwrap(),
        Phase::Waiting
    );
    // A superseding submit on the same thread abandons the parked run.
    ingress
        .submit_superseding(activation("new"))
        .await
        .expect("superseding submit");
    assert_eq!(
        ingress.superseded().await.unwrap(),
        vec![RunId("old".to_string())],
        "the prior parked run is superseded"
    );
    // The superseded run is never woken again: recovery does not process it.
    assert!(
        !ingress
            .recover(0)
            .await
            .unwrap()
            .iter()
            .any(|(run, _)| run.0 == "old"),
        "a superseded run is not claimable"
    );
}

#[tokio::test]
async fn dead_letter_ttl_gc_store_spec() {
    harness::assert_dead_letter_ttl_gc(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn renew_owned_leases_store_spec() {
    harness::assert_renew_owned_leases(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn renew_skips_far_from_expiry_store_spec() {
    harness::assert_renew_skips_far_from_expiry(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn list_dispatches_store_spec() {
    harness::assert_list_dispatches(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn dedupe_ignores_dead_lettered_store_spec() {
    harness::assert_dedupe_ignores_dead_lettered(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn wake_suppressed_while_thread_running_store_spec() {
    harness::assert_wake_suppressed_while_thread_running(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn ingress_lists_dispatches_and_purges_aged_dead_letters() {
    // Covers the DurableRunIngress query + time-windowed GC wrappers.
    let runtime = text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ingress = DurableRunIngress::new(runtime, store.clone(), commit);

    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();
    let listed = ingress.list_dispatches().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].run_id, RunId("run-1".to_string()));

    // Dead-letter it at t=1000, then age it out through the ingress.
    assert!(store.claim("w", 1, 0).await.unwrap().is_some());
    assert_eq!(ingress.reap(0, 1_000).await.unwrap(), 1);
    assert_eq!(ingress.purge_dead_letters_before(999).await.unwrap(), 0);
    assert_eq!(ingress.purge_dead_letters_before(1_000).await.unwrap(), 1);
    assert!(ingress.list_dispatches().await.unwrap().is_empty());
}
