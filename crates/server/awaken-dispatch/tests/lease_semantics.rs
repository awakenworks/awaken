// @formal evidence checkpoint.recovery_never_precedes_commit
// @formal evidence ingress.epoch_reclaim
// @formal evidence remote_worker.committed_terminal_recovery_skips_execution_and_settles_dispatch
// @formal evidence session_execution.one_physical_attempt_per_thread
// @formal evidence session_execution.pre_start_cancel_ack_requires_fresh_slot_install
//! Focused integration tests for the durable dispatch worker's LEASE semantics —
//! the fence that makes durable recovery safe under a multi-owner fleet.
//!
//! The claim/lease contract is: a claimed dispatch is *owned* for `lease_ms`; a
//! second owner cannot claim it until the lease expires (crash recovery), and the
//! owner keeps it alive by renewing. These tests pin the six load-bearing rules:
//!
//! 1. a live lease fences a second claim (no two owners drive one run at once);
//! 2. an expired lease is reclaimed, then driven-and-settled exactly once;
//! 3. a *terminal committed record* short-circuits a stale reclaim — the worker
//!    settles Done without re-executing (committed truth is authority);
//! 4. lease renewal keeps a slow-but-alive owner from being stolen across many
//!    lease periods (the exact fence that stops a fleet double-drive); and
//! 5. a run reclaimed after a process-bound non-recoverable tool has quiesced
//!    does NOT replay the tool: its committed Executing phase becomes an
//!    Indeterminate result, while the stale owner's mutation remains fenced.
//! 6. a reclaimed opaque ACP Run is terminally Indeterminate and its prompt is
//!    never sent again without an official idempotent receipt.

use awaken_dispatch::DispatchAttemptRecordReads as _;

mod harness;

use awaken_dispatch::ControlDispatch;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_dispatch::{
    AttemptAdmission, DispatchOutcome, DispatchWorker, MemoryDispatchStore, RunClaim, RunDispatch,
    SqliteDispatchStore,
};
use awaken_runtime_contract::execution::{AttemptRecoveryMode, RunAttemptExecutor, RunExecutor};
use awaken_runtime_contract::resume::ResumeCommand;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_store_inmem::MemoryCommitCoordinator;

use awaken_dispatch_testkit::ConformanceClock as _;
use harness::{activation, blocking_tool_runtime, counting_text_runtime, text_runtime};

const LEASE: u64 = 1_000;

#[tokio::test]
async fn native_worker_freezes_binding_before_tool_entry_and_clears_on_return() {
    // B18 registered exact binding + Native drive => the admitted slot retains
    // the coordinate while a real Runtime tool is blocked, then the ordinary
    // Worker ACK clears it. B19 wrong incarnation => zero tool entry, no slot.
    // Provider truth and independent post-crash stopping are not asserted here.
    for valid in [false, true] {
        let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
        let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
        let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
        let (directory, identity) =
            awaken_dispatch_testkit::worker_http::ready_worker("binding-worker").await;
        let snapshot = directory
            .current(&identity.worker_id)
            .await
            .unwrap()
            .unwrap()
            .snapshot;
        store
            .enqueue(
                RunDispatch::new(activation("worker-binding"))
                    .with_placement(awaken_dispatch::PlacementRequirements::remote_required())
                    .with_worker_codec_requirements(),
            )
            .await
            .unwrap();
        clock.set(0);
        let claimed = store
            .claim_compatible(&snapshot, 60_000)
            .await
            .unwrap()
            .unwrap();
        let claim = RunClaim::from(&claimed.lease);
        let binding = awaken_dispatch::AttemptExecutionBinding::new(
            if valid {
                identity.clone()
            } else {
                awaken_dispatch::WorkerIdentity::new("foreign", "boot", 1)
            },
            "cluster-a",
            "pod-a",
        )
        .unwrap();
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let (runtime, calls, entered) =
            harness::blocking_tool_runtime_with_entry_signal(release.clone());
        let worker = DispatchWorker::new(runtime, store.clone(), commit, identity.lease_owner())
            .with_execution_binding(binding.clone());
        let drive =
            tokio::spawn(
                async move { worker.drive_claimed(claimed, harness::at(&clock, 0)).await },
            );
        if valid {
            tokio::time::timeout(Duration::from_secs(5), entered.notified())
                .await
                .expect("B18 tool entry");
            assert_eq!(
                store.load_physical_attempt(&claim.run_id).await.unwrap(),
                Some(awaken_dispatch::PhysicalAttempt {
                    closing: false,
                    claim: claim.clone(),
                    binding: Some(binding),
                    thread_id: Some(ThreadId(harness::THREAD.into())),
                    session_thread_id: None,
                })
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            release.add_permits(1);
            tokio::time::timeout(Duration::from_secs(5), drive)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        } else {
            assert!(
                tokio::time::timeout(Duration::from_secs(5), drive)
                    .await
                    .unwrap()
                    .unwrap()
                    .is_err()
            );
            assert_eq!(calls.load(Ordering::SeqCst), 0, "B19 no external effect");
        }
        assert_eq!(
            store.load_physical_attempt(&claim.run_id).await.unwrap(),
            None
        );
    }
}

struct ParentReturnsBeforeChild {
    release: Arc<tokio::sync::Semaphore>,
    child: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    returned: tokio::sync::Notify,
    evidence: std::sync::Mutex<Option<awaken_runtime_contract::runtime_context::AttemptQuiescence>>,
}

#[async_trait::async_trait]
impl RunExecutor for ParentReturnsBeforeChild {
    async fn execute(
        &self,
        activation: awaken_runtime_contract::RunActivation,
        context: RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<RunState> {
        let entered = Arc::new(tokio::sync::Notify::new());
        let child_entered = entered.clone();
        let release = self.release.clone();
        let quiescence = context.physical_quiescence.clone();
        *self.evidence.lock().unwrap() = Some(quiescence.clone());
        *self.child.lock().unwrap() = Some(tokio::spawn(async move {
            quiescence
                .observe_return(async {
                    child_entered.notify_one();
                    release.acquire().await.unwrap().forget();
                })
                .await;
        }));
        entered.notified().await;
        awaken_agent_contract::thread::commit::commit_run(
            context.commit.as_deref().unwrap(),
            &activation.thread_id,
            awaken_agent_contract::thread::commit::RunDisposition::ended(
                activation.run_id,
                EndCause::Indeterminate,
            ),
            Vec::new(),
            Vec::new(),
        )
        .await
        .map_err(|error| awaken_runtime_contract::execution::Error::Commit(error.to_string()))?;
        self.returned.notify_one();
        Ok(RunState::Ended(EndCause::Indeterminate))
    }
}

#[async_trait::async_trait]
impl RunAttemptExecutor for ParentReturnsBeforeChild {
    async fn resume(
        &self,
        activation: awaken_runtime_contract::RunActivation,
        _command: ResumeCommand,
        context: RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<RunState> {
        self.execute(activation, context).await
    }
}

async fn assert_parent_return_waits_for_all_participants<
    S: ControlDispatch + awaken_dispatch::DispatchReads + 'static,
>(
    store: Arc<S>,
    dropped: bool,
) {
    // Cause/effect graph: C1 exact physical attempt admitted; C2 parent commits
    // Ended(Indeterminate) and returns; C3 inline child still active; C4 child
    // returns OR its owned future drops. R1=C1+C2+C3 => no physical ACK and no
    // Session settlement callback. R2=R1+returned child => exact slot retired,
    // Done tombstone and callback once; original Indeterminate remains intact.
    // R3=R1+dropped child => Unknown, retained slot, no callback or replay.
    // The production Worker is used with both canonical backends; tests hold
    // actual owned futures, not a fabricated receipt or private state mutation.
    let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let executor = Arc::new(ParentReturnsBeforeChild {
        release: Arc::new(tokio::sync::Semaphore::new(0)),
        child: Default::default(),
        returned: Default::default(),
        evidence: Default::default(),
    });
    let observer = Arc::new(RecoverySettlementObserver {
        calls: AtomicUsize::new(0),
        store: store.clone(),
    });
    store
        .enqueue_session_child(
            RunDispatch::new(activation("parent-before-child"))
                .for_session(ThreadId("parent".into()))
                .with_session_activity_epoch(1),
            awaken_dispatch::SessionChildAdmission::new(25, Vec::new()),
        )
        .await
        .unwrap();
    let claimed = store
        .claim("owner-a", LEASE, &Default::default())
        .await
        .unwrap()
        .unwrap();
    let claim = RunClaim::from(&claimed.lease);
    let worker = DispatchWorker::new(text_runtime(), store.clone(), commit.clone(), "owner-a")
        .with_settlement_observer(observer.clone());
    worker.install_attempt_executor(executor.clone());
    let mut drive =
        tokio::spawn(async move { worker.drive_claimed(claimed, harness::clock(0)).await });
    tokio::time::timeout(Duration::from_secs(5), executor.returned.notified())
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut drive)
            .await
            .is_err(),
        "R1 active child keeps the owned drive"
    );
    assert!(
        store
            .load_physical_attempt(&claim.run_id)
            .await
            .unwrap()
            .is_some(),
        "R1 occupied"
    );
    assert_eq!(
        observer.calls.load(Ordering::SeqCst),
        0,
        "R1 callback withheld"
    );
    let evidence = executor.evidence.lock().unwrap().clone().unwrap();
    assert!(
        !evidence.permits_execution(),
        "R1 closed domain rejects delayed work"
    );
    let child = executor.child.lock().unwrap().take().unwrap();
    if dropped {
        child.abort();
        assert!(child.await.unwrap_err().is_cancelled());
    } else {
        executor.release.add_permits(1);
        child.await.unwrap();
    }
    let result = tokio::time::timeout(Duration::from_secs(5), drive)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result.is_some(), !dropped, "R2/R3 logical settlement");
    assert_eq!(
        store
            .load_physical_attempt(&claim.run_id)
            .await
            .unwrap()
            .is_none(),
        !dropped,
        "R2/R3 exact retirement"
    );
    assert_eq!(
        observer.calls.load(Ordering::SeqCst),
        usize::from(!dropped),
        "R2/R3 callback count"
    );
    assert_eq!(
        commit.run(&claim.run_id).unwrap().state,
        RunState::Ended(EndCause::Indeterminate),
        "R2/R3 no success fabrication"
    );
}

#[tokio::test]
async fn physical_ack_waits_for_all_participants_memory() {
    for dropped in [false, true] {
        let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
        assert_parent_return_waits_for_all_participants(
            Arc::new(MemoryDispatchStore::new(clock.clock())),
            dropped,
        )
        .await;
    }
}

#[tokio::test]
async fn physical_ack_waits_for_all_participants_sqlite() {
    for dropped in [false, true] {
        let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
        assert_parent_return_waits_for_all_participants(
            Arc::new(
                SqliteDispatchStore::open_in_memory(clock.clock())
                    .await
                    .unwrap(),
            ),
            dropped,
        )
        .await;
    }
}

struct ContinuationOnlyAttemptExecutor {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl RunExecutor for ContinuationOnlyAttemptExecutor {
    async fn execute(
        &self,
        _activation: awaken_runtime_contract::RunActivation,
        _context: RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<RunState> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(RunState::Ended(EndCause::NaturalEnd))
    }
}

#[async_trait::async_trait]
impl RunAttemptExecutor for ContinuationOnlyAttemptExecutor {
    fn physical_attempt_recovery(
        &self,
        _activation: &awaken_runtime_contract::RunActivation,
        _context: &RuntimeRunContext,
    ) -> AttemptRecoveryMode {
        AttemptRecoveryMode::ContinueDurableRemoteOperation
    }

    async fn resume(
        &self,
        activation: awaken_runtime_contract::RunActivation,
        _command: ResumeCommand,
        context: RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<RunState> {
        self.execute(activation, context).await
    }
}

#[tokio::test]
async fn pre_start_cancellation_acknowledges_only_a_freshly_installed_slot() {
    // Cause/effect graph: C1 cancellation is already observable before the
    // executor Future is built; C2 admission installs an empty slot, stutters an
    // exact response-loss retry, or replaces a predecessor through continuation
    // handoff. Effects: E1 no executor call; E2 fresh install is cleared as a
    // known no-effect attempt; E3 stutter/replacement retains the inherited
    // obligation and reports Indeterminate. Decision table: R1=C1+Install ->
    // E1+E2. R2=C1+Stutter -> E1+E3. R3=C1+Replace -> E1+E3.
    let fresh_token = tokio_util::sync::CancellationToken::new();
    fresh_token.cancel();
    let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let fresh_store = Arc::new(MemoryDispatchStore::new(clock.clone()));
    let fresh_commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let fresh_executor = Arc::new(ContinuationOnlyAttemptExecutor {
        calls: AtomicUsize::new(0),
    });
    let fresh_worker = DispatchWorker::new(
        text_runtime(),
        fresh_store.clone(),
        fresh_commit,
        "fresh-owner",
    )
    .with_context(RuntimeRunContext::new().with_cancellation(fresh_token));
    fresh_worker.install_attempt_executor(fresh_executor.clone());
    let fresh_error = fresh_worker
        .start_run(
            RunDispatch::new(activation("pre-start-fresh")),
            harness::at(&clock, 0),
        )
        .await
        .expect_err("R1 cancellation prevents executor entry");
    assert!(
        fresh_error
            .to_string()
            .contains("lost before physical attempt start"),
        "R1 returns the pre-start authority-loss cause: {fresh_error}"
    );
    assert_eq!(fresh_executor.calls.load(Ordering::SeqCst), 0, "R1/E1");
    assert!(
        !fresh_store.list_dispatches().await.unwrap()[0].physical_attempt_active,
        "R1/E2 the exact fresh no-effect slot is acknowledged"
    );
    // R1 retains only operation-Future provenance. Resolver/prepare IO before
    // this boundary and independent durable tasks are not a stopped-domain
    // proof. The same factual kind must be absent from stutter/replacement.
    let local = fresh_store
        .attempt_closure_records_after(
            awaken_dispatch::DispatchCursor::default(),
            usize::MAX,
            Some(&RunId("pre-start-fresh".into())),
        )
        .await
        .unwrap();
    assert_eq!(local.events.len(), 1, "R1 one original ACK observation");
    let awaken_dispatch::AttemptPhysicalFact::Acknowledgement { record } = &local.events[0].record
    else {
        panic!("R1 no independent termination kind")
    };
    assert_eq!(
        record.cause(),
        awaken_dispatch::AttemptReturnCause::OperationFutureNotEntered
    );

    let stutter_token = tokio_util::sync::CancellationToken::new();
    stutter_token.cancel();
    let stutter_store = Arc::new(MemoryDispatchStore::new(clock.clone()));
    let stutter_commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    stutter_store
        .enqueue(RunDispatch::new(activation("pre-start-stutter")))
        .await
        .unwrap();
    clock.set(0);
    let stutter_claimed = stutter_store
        .claim("stutter-owner", LEASE, &Default::default())
        .await
        .unwrap()
        .expect("stutter claim");
    let stutter_claim = RunClaim::from(&stutter_claimed.lease);
    clock.set(0);
    assert_eq!(
        stutter_store
            .begin_attempt(&stutter_claim, None)
            .await
            .unwrap(),
        AttemptAdmission::Applied,
        "R2 fixture models a lost Install response"
    );
    let stutter_executor = Arc::new(ContinuationOnlyAttemptExecutor {
        calls: AtomicUsize::new(0),
    });
    let stutter_worker = DispatchWorker::new(
        text_runtime(),
        stutter_store.clone(),
        stutter_commit.clone(),
        "stutter-owner",
    )
    .with_lease_ms(LEASE)
    .with_context(RuntimeRunContext::new().with_cancellation(stutter_token));
    stutter_worker.install_attempt_executor(stutter_executor.clone());
    let stutter_processed = stutter_worker
        .drive_claimed(stutter_claimed, harness::at(&clock, 0))
        .await
        .expect("R2 commits the existing Indeterminate terminal path");
    assert!(
        stutter_processed.is_none(),
        "R2 occupied slot prevents settlement"
    );
    assert_eq!(stutter_executor.calls.load(Ordering::SeqCst), 0, "R2/E1");
    assert!(
        stutter_store.list_dispatches().await.unwrap()[0].physical_attempt_active,
        "R2/E3 response-loss stutter retains the possible prior effect"
    );
    assert!(
        stutter_store
            .attempt_closure_records_after(
                awaken_dispatch::DispatchCursor::default(),
                usize::MAX,
                Some(&stutter_claim.run_id)
            )
            .await
            .unwrap()
            .events
            .is_empty(),
        "R2 no invented not-entered fact"
    );
    assert_eq!(
        stutter_store
            .finish_attempt(&stutter_claim, None)
            .await
            .unwrap(),
        awaken_dispatch::SettleOutcome::Applied,
        "R2 fixture cleanup uses the exact retained owner"
    );

    let inherited_token = tokio_util::sync::CancellationToken::new();
    inherited_token.cancel();
    let inherited_store = Arc::new(MemoryDispatchStore::new(clock.clone()));
    let inherited_commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    inherited_store
        .enqueue(RunDispatch::new(activation("pre-start-inherited")))
        .await
        .unwrap();
    clock.set(0);
    let predecessor = inherited_store
        .claim("predecessor", LEASE, &Default::default())
        .await
        .unwrap()
        .expect("predecessor claim");
    let predecessor_claim = RunClaim::from(&predecessor.lease);
    clock.set(0);
    assert_eq!(
        inherited_store
            .begin_attempt(&predecessor_claim, None)
            .await
            .unwrap(),
        AttemptAdmission::Applied,
        "R3 fixture installs the predecessor slot"
    );
    clock.set(LEASE + 1);
    let replacement = inherited_store
        .claim("replacement", LEASE, &Default::default())
        .await
        .unwrap()
        .expect("replacement claim");
    let replacement_claim = RunClaim::from(&replacement.lease);
    let inherited_executor = Arc::new(ContinuationOnlyAttemptExecutor {
        calls: AtomicUsize::new(0),
    });
    let inherited_worker = DispatchWorker::new(
        text_runtime(),
        inherited_store.clone(),
        inherited_commit.clone(),
        "replacement",
    )
    .with_lease_ms(LEASE)
    .with_context(RuntimeRunContext::new().with_cancellation(inherited_token));
    inherited_worker.install_attempt_executor(inherited_executor.clone());
    let processed = inherited_worker
        .drive_claimed(replacement, harness::at(&clock, LEASE + 1))
        .await
        .expect("R3 commits the existing Indeterminate terminal path");
    assert!(processed.is_none(), "R3 occupied slot prevents settlement");
    assert_eq!(inherited_executor.calls.load(Ordering::SeqCst), 0, "R3/E1");
    assert!(
        inherited_store.list_dispatches().await.unwrap()[0].physical_attempt_active,
        "R3/E3 inherited responsibility remains visible"
    );
    assert!(
        inherited_store
            .attempt_closure_records_after(
                awaken_dispatch::DispatchCursor::default(),
                usize::MAX,
                Some(&replacement_claim.run_id)
            )
            .await
            .unwrap()
            .events
            .is_empty(),
        "R3 handoff is not new empty-slot provenance"
    );
    assert_eq!(
        CommittedThreadView::run(
            inherited_commit.as_ref(),
            &RunId("pre-start-inherited".into())
        )
        .expect("R3 terminal fact")
        .state,
        RunState::Ended(EndCause::Indeterminate),
        "R3/E3 cancellation cannot be reported as confirmed"
    );
    assert_eq!(
        inherited_store
            .finish_attempt(&predecessor_claim, None)
            .await
            .unwrap(),
        awaken_dispatch::SettleOutcome::Fenced,
        "R3 replacement provenance cannot be erased by the predecessor"
    );
    assert_eq!(
        inherited_store
            .finish_attempt(&replacement_claim, None)
            .await
            .unwrap(),
        awaken_dispatch::SettleOutcome::Applied,
        "R3 fixture cleanup uses only the exact inherited owner"
    );
}

#[tokio::test]
async fn a_returned_unknown_tool_result_cannot_release_the_durable_physical_slot() {
    // U1 the real Native Runtime returns from a tool with typed unknown; U2 its
    // logical Indeterminate is committed; U3 successor claims after expiry.
    // => one effect, retained predecessor slot, and Blocked successor. A logical
    // return/terminal commit is not an ACK. No synthetic latch injection here.
    use awaken_tool_contract::tool::{RawTool, ToolCall, ToolError, ToolOutput};
    struct UnknownTool(Arc<std::sync::atomic::AtomicUsize>);
    #[async_trait::async_trait]
    impl RawTool for UnknownTool {
        fn id(&self) -> &str {
            "echo"
        }
        async fn invoke(&self, _: ToolCall) -> Result<ToolOutput, ToolError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(ToolError::Indeterminate("response lost".into()))
        }
    }
    let effects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let runtime = harness::tool_until_result_runtime(Arc::new(UnknownTool(effects.clone())), 1);
    let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
    let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let run = RunId("returned-unknown".into());
    let worker =
        DispatchWorker::new(runtime, store.clone(), commit.clone(), "owner-a").with_lease_ms(LEASE);
    // Settlement may reject the occupied slot, but must preserve terminal Run
    // truth and never erase physical uncertainty to report a successful Done.
    let _ = worker
        .start_run(RunDispatch::new(activation(&run.0)), harness::at(&clock, 0))
        .await;
    assert_eq!(
        CommittedThreadView::run(commit.as_ref(), &run)
            .unwrap()
            .state,
        RunState::Ended(EndCause::Indeterminate)
    );
    assert!(store.list_dispatches().await.unwrap()[0].physical_attempt_active);
    clock.set(LEASE + 1);
    let replacement = store
        .claim("owner-b", LEASE, &Default::default())
        .await
        .unwrap()
        .unwrap();
    clock.set(LEASE + 1);
    assert_eq!(
        store
            .begin_attempt(&RunClaim::from(&replacement.lease), None)
            .await
            .unwrap(),
        AttemptAdmission::Blocked
    );
    assert_eq!(effects.load(Ordering::SeqCst), 1);
}

async fn assert_native_cancel_observes_history_before_physical_ack<S>(
    store: Arc<S>,
    unknown: bool,
    clock: &dyn awaken_dispatch_testkit::ConformanceClock,
) where
    S: awaken_dispatch::ControlDispatch + awaken_dispatch::DispatchReads + 'static,
{
    use awaken_agent_contract::agent::state::StateKey;
    use awaken_agent_contract::thread::commit::{
        coordinator::Coordinator as CommitCoordinator,
        staged::{RunDisposition, ThreadCommit},
    };
    use awaken_runtime_contract::model_operation::{
        ActiveModelOperation, ModelExecutionEvidence, ModelOperation,
    };
    // C1 a cancellation claims an empty physical slot; C2 committed model
    // evidence is Unknown/Terminated (e.g. a legacy slot already released).
    // R1 Unknown => logical Cancelled but physical slot retained and settlement
    // withheld. R2 Terminated => Cancelled and normal settlement. Both enter the
    // real Native cancel adapter, never a fake executor; neither invokes a model.
    // The later Runtime terminal writer cannot repair an ACK already emitted.
    let request = activation("cancel-evidence");
    let run_id = request.run_id.clone();
    let thread_id = request.thread_id.clone();
    let mut operation = ModelOperation::next(
        None,
        request
            .snapshot
            .resolved_spec
            .model_binding
            .binding()
            .clone(),
    )
    .unwrap();
    operation.execution = if unknown {
        ModelExecutionEvidence::Unknown
    } else {
        ModelExecutionEvidence::Terminated(
            awaken_runtime_contract::model_operation::TerminalReceipt::from_wire_status(Some(
                "completed",
            ))
            .unwrap(),
        )
    };
    let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    commit
        .commit(ThreadCommit::assemble(
            thread_id.clone(),
            RunDisposition::running(run_id.clone()),
            true,
            Vec::new(),
            vec![ActiveModelOperation::write(&Some(operation))],
            Vec::new(),
        ))
        .await
        .unwrap();
    store.enqueue(RunDispatch::new(request)).await.unwrap();
    store.cancel(&run_id).await.unwrap().unwrap();
    let (runtime, calls) = counting_text_runtime();
    let worker = DispatchWorker::new(runtime, store.clone(), commit.clone(), "cancel-owner")
        .with_lease_ms(LEASE);
    let result = worker.tick(harness::clock(0)).await.unwrap();
    assert_eq!(
        commit.run_state(&run_id),
        Some(RunState::Ended(EndCause::Cancelled))
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let rows = store.list_dispatches().await.unwrap();
    if unknown {
        assert!(
            result.is_none(),
            "unknown execution cannot settle after a no-op cancel"
        );
        assert!(
            rows.iter()
                .any(|row| row.run_id == run_id && row.physical_attempt_active)
        );
        clock.set(LEASE + 1);
        let replacement = store
            .claim("replacement", LEASE, &Default::default())
            .await
            .unwrap()
            .unwrap();
        clock.set(LEASE + 1);
        assert_eq!(
            store
                .begin_attempt(&RunClaim::from(&replacement.lease), None)
                .await
                .unwrap(),
            AttemptAdmission::Blocked
        );
    } else {
        assert_eq!(result, Some((run_id, RunState::Ended(EndCause::Cancelled))));
        assert!(rows.iter().all(|row| !row.physical_attempt_active));
    }
}

#[tokio::test]
async fn native_cancel_evidence_precedes_physical_ack_memory() {
    for unknown in [false, true] {
        let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
        assert_native_cancel_observes_history_before_physical_ack(
            Arc::new(MemoryDispatchStore::new(clock.clock())),
            unknown,
            &clock,
        )
        .await;
    }
}

#[tokio::test]
async fn native_cancel_evidence_precedes_physical_ack_sqlite() {
    for unknown in [false, true] {
        let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
        assert_native_cancel_observes_history_before_physical_ack(
            Arc::new(
                SqliteDispatchStore::open_in_memory(clock.clock())
                    .await
                    .unwrap(),
            ),
            unknown,
            &clock,
        )
        .await;
    }
}

// --- 1. A live lease fences a second claim ---------------------------------

/// Store-level spec: while owner A holds a live lease on the only runnable
/// dispatch, a second owner B claiming the same runnable set is handed nothing —
/// the run is not double-owned. Every backend must match, so the body lives once.
async fn assert_live_lease_fences_a_claim<S: ControlDispatch>(
    store: &S,
    clock: &dyn awaken_dispatch_testkit::ConformanceClock,
) {
    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();

    // A claims at t=0 with a lease that expires at LEASE.
    clock.set(0);
    let a = store
        .claim("owner-a", LEASE, &Default::default())
        .await
        .unwrap();
    assert_eq!(
        a.map(|c| c.lease.owner().to_owned()),
        Some("owner-a".to_string()),
        "the fresh run is claimable by A"
    );

    // B claiming mid-lease (t=LEASE/2) is fenced: the run is leased to A.
    clock.set(LEASE / 2);
    assert!(
        store
            .claim("owner-b", LEASE, &Default::default())
            .await
            .unwrap()
            .is_none(),
        "a live lease fences a second owner's claim"
    );
    // Even at the instant of expiry (expires_ms == now is still held) B is fenced.
    clock.set(LEASE - 1);
    assert!(
        store
            .claim("owner-b", LEASE, &Default::default())
            .await
            .unwrap()
            .is_none(),
        "the lease holds right up to its expiry"
    );
}

#[tokio::test]
async fn live_lease_fences_a_claim_memory() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    assert_live_lease_fences_a_claim(&MemoryDispatchStore::new(clock.clock()), &clock).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replacement_claim_defers_until_predecessor_quiescence_before_model_entry() {
    // Cause/effect decision table:
    // W1 expired A claim + A physical slot + replacement B -> B may own the
    // mutation fence but model calls remain zero and the claim is durably
    // deferred; W2 exact A finish ACK + fresh due claim -> B enters once and
    // completes; W3 no A ACK -> the physical slot remains occupied while no
    // Worker task is retained.
    // This test owns W1/W2 at the Worker->model boundary. The shared store spec
    // owns W3 as a durable state transition, avoiding a hanging test while still
    // proving lease expiry alone never clears the slot.
    let (runtime, model_calls) = counting_text_runtime();
    let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
    let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    store
        .enqueue(RunDispatch::new(activation("handoff-run")))
        .await
        .expect("fixture enqueue");
    clock.set(0);
    let first = store
        .claim("owner-a", LEASE, &Default::default())
        .await
        .expect("A query")
        .expect("A claim");
    let claim_a = RunClaim::from(&first.lease);
    clock.set(0);
    assert_eq!(
        store
            .begin_attempt(&claim_a, None)
            .await
            .expect("W1 begin A"),
        AttemptAdmission::Applied
    );
    clock.set(LEASE + 1);
    let replacement = store
        .claim("owner-b", LEASE, &Default::default())
        .await
        .expect("B query")
        .expect("B replacement claim");
    let worker_b = Arc::new(
        DispatchWorker::new(runtime, store.clone(), commit, "owner-b").with_lease_ms(LEASE),
    );
    let processed = tokio::time::timeout(
        Duration::from_secs(5),
        worker_b.drive_claimed(replacement, harness::at(&clock, LEASE + 1)),
    )
    .await
    .expect("W1 blocked replacement releases its task")
    .expect("W1 deferral succeeds");
    assert_eq!(processed, None, "W1 mutation claim was deferred");
    assert_eq!(
        model_calls.load(Ordering::SeqCst),
        0,
        "W1 mutation takeover did not enter the model"
    );
    assert_eq!(
        store
            .finish_attempt(&claim_a, None)
            .await
            .expect("W2 A ACK"),
        awaken_dispatch::SettleOutcome::Applied
    );
    clock.set(LEASE * 2 + 1);
    let replacement = store
        .claim("owner-b", LEASE, &Default::default())
        .await
        .expect("W2 B query")
        .expect("W2 fresh replacement claim");
    let processed = tokio::time::timeout(
        Duration::from_secs(5),
        worker_b.drive_claimed(replacement, harness::at(&clock, LEASE * 2 + 1)),
    )
    .await
    .expect("W2 replacement unblocks")
    .expect("W2 drive succeeds");
    assert_eq!(
        processed,
        Some((
            RunId("handoff-run".into()),
            RunState::Ended(EndCause::NaturalEnd)
        ))
    );
    assert_eq!(model_calls.load(Ordering::SeqCst), 1, "W2 one model entry");
}

#[tokio::test]
async fn live_lease_fences_a_claim_sqlite() {
    // The SQL claim predicate (`lease_until >= now`) must fence exactly as the
    // in-memory reference does.
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    assert_live_lease_fences_a_claim(
        &SqliteDispatchStore::open_in_memory(clock.clock())
            .await
            .expect("open"),
        &clock,
    )
    .await;
}

// --- 2. An expired lease is reclaimed, then driven+settled exactly once -----

#[tokio::test]
async fn expired_lease_is_reclaimed_and_driven_exactly_once() {
    // Test design. Causes: C1 owner A claims a fresh Run and dies before execute;
    // C2 its lease expires; C3 owner B ticks. Effects: E1 C3 reclaims and executes
    // once; E2 the dispatch settles and cannot be claimed again. Constraint/
    // Invariant: recovery preserves Run identity and the lease epoch fences A.
    // Decision rule: compare before-expiry no-claim with after-expiry C3 and
    // require one inference plus an empty queue.
    // A claims D at t=0 and dies without settling (simulated: claim, never drive).
    // After the lease expires, B MUST reclaim, drive, and settle exactly once.
    let (runtime, infers) = counting_text_runtime();
    let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
    let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));

    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    clock.set(0);
    assert!(
        store
            .claim("owner-a", LEASE, &Default::default())
            .await
            .unwrap()
            .is_some(),
        "A claims the fresh run"
    );
    assert_eq!(infers.load(Ordering::SeqCst), 0, "A died before executing");

    // B's worker reclaims after expiry, drives to Done, and settles.
    let worker_b =
        DispatchWorker::new(runtime, store.clone(), commit.clone(), "owner-b").with_lease_ms(LEASE);
    let processed = worker_b.tick(harness::at(&clock, LEASE + 1)).await.unwrap();
    assert_eq!(
        processed,
        Some((
            RunId("run-1".to_string()),
            RunState::Ended(EndCause::NaturalEnd)
        )),
        "B reclaims the expired lease and drives the run to completion"
    );
    assert_eq!(
        infers.load(Ordering::SeqCst),
        1,
        "the reclaimed run executed exactly once"
    );

    // The settled dispatch is gone and not claimable again.
    assert_eq!(store.dispatch_count(), 0, "D is settled");
    clock.set(LEASE + 2);
    assert!(
        store
            .claim("owner-b", LEASE, &Default::default())
            .await
            .unwrap()
            .is_none(),
        "a settled dispatch is not claimable again"
    );
    assert_eq!(infers.load(Ordering::SeqCst), 1, "and never re-executed");
}

#[tokio::test]
async fn expired_opaque_acp_claim_is_indeterminate_without_prompt_replay() {
    // ACP recovery cause/effect graph:
    // C1=expired claim is reclaimed; C2=backend is opaque ACP/native;
    // C3=committed terminal truth already exists. Effects: E1=commit the
    // existing Indeterminate terminal and settle Done; E2=ordinary committed-
    // truth recovery; E3=execute the recoverable native attempt. Constraints:
    // C3 dominates C2, and only C1+ACP+!C3 reaches E1.
    //
    // Decision table:
    // | Rule | C1 recovered | backend | C3 terminal | Effect |
    // | A1 | yes | ACP | no | E1, zero executor calls |
    // | A2 | yes | any | yes | E2 (covered below by stale terminal) |
    // | A3 | yes | Native | no | E3 (expired_lease... test above) |
    // | A4 | no | ACP | no | normal ACP execution (executor suite owner) |
    //
    // FMECA: replay after an unobserved ACP/MCP dispatch can duplicate an
    // external mutation (S5/O2/D5). Conservative Indeterminate reduces the
    // effect to an explicit unknown terminal; no guessed protocol stage or
    // second effect journal is introduced.
    // Decision rule: execute A1 here; A2 and A3 are the adjacent terminal/native
    // recovery tests, while A4 remains the executor suite's fresh-run owner.
    let (runtime, infers) = counting_text_runtime();
    let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
    let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let run = RunId("run-acp".to_string());
    let mut acp = activation("run-acp");
    let (provider, model, _) = acp.snapshot.resolved_spec.model_binding.identity();
    let binding =
        awaken_runtime_contract::resolved::ModelBinding::parse(provider, model, "acp:claude")
            .unwrap();
    acp.snapshot.resolved_spec.model_binding =
        awaken_runtime_contract::resolved::ResolvedModelCandidate::host(binding);

    store
        .enqueue(RunDispatch::new(acp))
        .await
        .expect("A1 enqueue ACP dispatch");
    clock.set(0);
    assert!(
        store
            .claim("owner-a", LEASE, &Default::default())
            .await
            .expect("A1 first claim")
            .is_some(),
        "A1 first owner holds the opaque Run"
    );

    let worker_b =
        DispatchWorker::new(runtime, store.clone(), commit.clone(), "owner-b").with_lease_ms(LEASE);
    assert_eq!(
        worker_b
            .tick(harness::at(&clock, LEASE + 1))
            .await
            .expect("A1 recovery"),
        Some((run.clone(), RunState::Ended(EndCause::Indeterminate))),
        "A1"
    );
    assert_eq!(
        infers.load(Ordering::SeqCst),
        0,
        "A1 prompt was not replayed"
    );
    assert_eq!(
        CommittedThreadView::run(commit.as_ref(), &run)
            .expect("A1 committed terminal")
            .state,
        RunState::Ended(EndCause::Indeterminate),
        "A1 committed truth"
    );
    assert_eq!(store.dispatch_count(), 0, "A1 settled Done");
}

struct RecoverySettlementObserver<S> {
    calls: std::sync::atomic::AtomicUsize,
    store: Arc<S>,
}

#[async_trait::async_trait]
impl<S: awaken_dispatch::ControlDispatch + awaken_dispatch::DispatchReads + 'static>
    awaken_dispatch::DispatchSettlementObserver for RecoverySettlementObserver<S>
{
    async fn before_settle(
        &self,
        _: &RunDispatch,
        _: &RunClaim,
        _: &RunState,
        _: bool,
    ) -> Result<(), awaken_dispatch::DispatchSettlementError> {
        // R6 a Session callback may re-enter Dispatch to report/interrupt.
        // Holding its aggregate lock across this callback deadlocks. The outer
        // bounded tick checks completion, while this query exercises that edge.
        assert_eq!(self.store.list_dispatches().await.unwrap().len(), 1);
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

// Cause/effect table R2/R4/R5: an actual predecessor slot survives lease
// expiry or cancellation. Opaque recovery, and either backend's cancellation,
// must expose Indeterminate without entering another executor. The committed
// result must not clear the physical slot or consume the dispatch; only the
// exact predecessor ACK permits a later terminal reconciliation. Native ordinary
// recovery remains the adjacent wait-for-quiescence test's responsibility.
async fn assert_occupied_attempt_terminal_recovery<
    S: awaken_dispatch::ControlDispatch + awaken_dispatch::DispatchReads + 'static,
>(
    store: Arc<S>,
    acp_backend: bool,
    cancel: bool,
    coordinated: bool,
    clock: &dyn awaken_dispatch_testkit::ConformanceClock,
) {
    let (runtime, infers) = counting_text_runtime();
    let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let mut request = activation("occupied-recovery");
    let run = request.run_id.clone();
    if acp_backend {
        let (provider, model, _) = request.snapshot.resolved_spec.model_binding.identity();
        let binding =
            awaken_runtime_contract::resolved::ModelBinding::parse(provider, model, "acp:claude")
                .unwrap();
        request.snapshot.resolved_spec.model_binding =
            awaken_runtime_contract::resolved::ResolvedModelCandidate::host(binding);
    }
    let request = RunDispatch::new(request);
    if coordinated {
        store
            .enqueue_session_child(
                request
                    .for_session(ThreadId("parent".into()))
                    .with_session_activity_epoch(1),
                awaken_dispatch::SessionChildAdmission::new(25, Vec::new()),
            )
            .await
            .unwrap();
    } else {
        store.enqueue(request).await.unwrap();
    }
    clock.set(0);
    let first = store
        .claim("owner-a", LEASE, &Default::default())
        .await
        .unwrap()
        .unwrap();
    let predecessor = RunClaim::from(&first.lease);
    clock.set(0);
    assert_eq!(
        store.begin_attempt(&predecessor, None).await.unwrap(),
        AttemptAdmission::Applied
    );
    if cancel {
        store
            .cancel(&run)
            .await
            .unwrap()
            .expect("durable cancellation");
    }
    let observer = Arc::new(RecoverySettlementObserver {
        calls: std::sync::atomic::AtomicUsize::new(0),
        store: store.clone(),
    });
    let worker = DispatchWorker::new(runtime, store.clone(), commit.clone(), "owner-b")
        .with_lease_ms(LEASE)
        .with_settlement_observer(observer.clone());
    clock.set(LEASE + 1);
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(2),
            worker.tick(harness::clock(LEASE + 1))
        )
        .await
        .expect("R2/R4 logical outcome cannot wait for physical ACK")
        .expect("terminal commit"),
        None,
        "physical settlement remains pending"
    );
    assert_eq!(
        CommittedThreadView::run(commit.as_ref(), &run)
            .unwrap()
            .state,
        RunState::Ended(EndCause::Indeterminate)
    );
    assert_eq!(
        infers.load(Ordering::SeqCst),
        0,
        "no prompt or cancel replay"
    );
    assert_eq!(
        store.list_dispatches().await.unwrap().len(),
        1,
        "retain physical authority"
    );
    assert_eq!(
        observer.calls.load(Ordering::SeqCst),
        0,
        "R2 no premature Session boundary"
    );
    assert_eq!(
        store.finish_attempt(&predecessor, None).await.unwrap(),
        awaken_dispatch::SettleOutcome::Applied
    );
    clock.set(3 * LEASE);
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(2),
            worker.tick(harness::clock(3 * LEASE))
        )
        .await
        .expect("R6 Session callback may re-enter Dispatch")
        .unwrap(),
        Some((run, RunState::Ended(EndCause::Indeterminate)))
    );
    assert!(store.list_dispatches().await.unwrap().is_empty());
    assert_eq!(
        infers.load(Ordering::SeqCst),
        0,
        "R5 terminal repair never executes"
    );
    assert_eq!(
        observer.calls.load(Ordering::SeqCst),
        usize::from(coordinated)
    );
}

#[tokio::test]
async fn occupied_attempt_recovery_memory() {
    // Backend row M: run every legal R2/R4 combination through the same oracle.
    for (acp, cancel) in [(true, false), (true, true), (false, true)] {
        for coordinated in [false, true] {
            let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
            assert_occupied_attempt_terminal_recovery(
                Arc::new(MemoryDispatchStore::new(clock.clock())),
                acp,
                cancel,
                coordinated,
                &clock,
            )
            .await;
        }
    }
}

#[tokio::test]
async fn occupied_attempt_recovery_sqlite() {
    // Backend row S: SQL persistence must preserve the same logical/physical split.
    for (acp, cancel) in [(true, false), (true, true), (false, true)] {
        for coordinated in [false, true] {
            let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
            assert_occupied_attempt_terminal_recovery(
                Arc::new(
                    SqliteDispatchStore::open_in_memory(clock.clock())
                        .await
                        .unwrap(),
                ),
                acp,
                cancel,
                coordinated,
                &clock,
            )
            .await;
        }
    }
}

#[tokio::test]
async fn committed_terminal_truth_dominates_opaque_acp_recovery() {
    // This is decision-table rule A2 from the preceding cause/effect graph:
    // C1=recovered claim, C2=opaque ACP backend, C3=committed terminal exists.
    // C3 dominates C2, so E2 settles the exact existing terminal without a new
    // inference and without replacing it by Indeterminate. FMECA: ordering the
    // ACP guard first would corrupt known truth and bypass shared inbox cleanup.
    // Constraint/Invariant: committed terminal truth precedes backend replay
    // policy. Decision rule: execute A2 and assert the exact terminal remains,
    // no inference occurs, and the dispatch settles.
    let (runtime, infers) = counting_text_runtime();
    let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
    let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let mut acp = activation("run-acp-committed");
    let (provider, model, _) = acp.snapshot.resolved_spec.model_binding.identity();
    let binding =
        awaken_runtime_contract::resolved::ModelBinding::parse(provider, model, "acp:claude")
            .unwrap();
    acp.snapshot.resolved_spec.model_binding =
        awaken_runtime_contract::resolved::ResolvedModelCandidate::host(binding);

    store
        .enqueue(RunDispatch::new(acp.clone()))
        .await
        .expect("A2 enqueue");
    clock.set(0);
    assert!(
        store
            .claim("owner-a", LEASE, &Default::default())
            .await
            .expect("A2 claim")
            .is_some()
    );
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    assert_eq!(
        runtime
            .execute(acp, context)
            .await
            .expect("A2 commit truth"),
        RunState::Ended(EndCause::NaturalEnd)
    );
    assert_eq!(infers.load(Ordering::SeqCst), 1);

    let worker_b =
        DispatchWorker::new(runtime, store.clone(), commit, "owner-b").with_lease_ms(LEASE);
    assert_eq!(
        worker_b
            .tick(harness::at(&clock, LEASE + 1))
            .await
            .expect("A2 recovery"),
        Some((
            RunId("run-acp-committed".to_string()),
            RunState::Ended(EndCause::NaturalEnd)
        )),
        "A2 committed terminal remains authoritative"
    );
    assert_eq!(infers.load(Ordering::SeqCst), 1, "A2 never re-executes");
    assert_eq!(store.dispatch_count(), 0, "A2 settles Done");
}

// --- 3. A terminal committed record short-circuits a stale reclaim ----------

#[tokio::test]
async fn stale_reclaim_of_a_completed_run_settles_without_re_executing() {
    // Test design. Causes: C1 owner A commits terminal truth; C2 it crashes before
    // queue settlement; C3 owner B reclaims after lease expiry. Effects: E1 C3
    // settles Done from the committed fact; E2 inference count does not increase.
    // Constraint/Invariant: terminal committed truth dominates stale leased state.
    // Decision rule: reproduce C1+C2+C3 and compare inference count before/after.
    // A claims D, drives it to a committed terminal record, then dies BEFORE it
    // settles the dispatch (the crash window between commit and settle). A stale
    // reclaim by B must settle Done from committed truth WITHOUT re-executing.
    let (runtime, infers) = counting_text_runtime();
    let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
    let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));

    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    // A takes the lease, then commits the run to a terminal Ended record by hand —
    // exactly what its worker would have committed — but never calls settle.
    clock.set(0);
    assert!(
        store
            .claim("owner-a", LEASE, &Default::default())
            .await
            .unwrap()
            .is_some()
    );
    let ctx = RuntimeRunContext::new().with_commit(commit.clone());
    let state = runtime.execute(activation("run-1"), ctx).await.unwrap();
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(infers.load(Ordering::SeqCst), 1, "A executed the run once");
    let record =
        CommittedThreadView::run(commit.as_ref(), &RunId("run-1".to_string())).expect("record");
    assert_eq!(record.state, RunState::Ended(EndCause::NaturalEnd));

    // B reclaims the still-Running dispatch (lease expired). The committed terminal
    // record is authority: the worker settles Done and does not re-run the model.
    let worker_b =
        DispatchWorker::new(runtime, store.clone(), commit.clone(), "owner-b").with_lease_ms(LEASE);
    let processed = worker_b.tick(harness::at(&clock, LEASE + 1)).await.unwrap();
    assert_eq!(
        processed,
        Some((
            RunId("run-1".to_string()),
            RunState::Ended(EndCause::NaturalEnd)
        )),
        "B settles the completed run from committed truth"
    );
    assert_eq!(
        infers.load(Ordering::SeqCst),
        1,
        "the model ran exactly once — the terminal record fenced re-execution"
    );
    assert_eq!(
        store.dispatch_count(),
        0,
        "the completed dispatch is removed"
    );
}

// --- 4. Lease renewal keeps a slow-but-alive owner across many periods ------

#[tokio::test]
async fn renewal_keeps_a_slow_owner_across_multiple_lease_periods() {
    // Cause/effect decision table for the one exact-claim renewal authority:
    //
    // | exact owner + epoch | renewal active | time | effect |
    // |---|---|---|---|
    // | yes | yes | before renewed deadline | replacement cannot claim |
    // | yes | no | after last deadline | replacement reclaims with new epoch |
    // | stale | any | after reclaim | old claim remains fenced |
    //
    // This deliberately renews a complete `RunClaim`; an owner-wide heartbeat
    // would be a second, weaker ownership authority.
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = MemoryDispatchStore::new(clock.clock());
    let lease = 100u64;
    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    clock.set(0);
    let first = store
        .claim("owner-a", lease, &Default::default())
        .await
        .unwrap()
        .expect("A claims at t=0 (expires 100)");
    let claim = RunClaim::from(&first.lease);

    // Three heartbeats, each firing before the current lease expires (60<100,
    // 120<160, 180<220), each extending the lease a full period ahead.
    for (renew_at, poke_at) in [(60u64, 120u64), (120, 180), (180, 240)] {
        clock.set(renew_at);
        assert_eq!(
            store.renew_lease(&claim, lease).await.unwrap(),
            awaken_dispatch::LeaseRenewalOutcome::Current,
            "the exact claim renewed A's in-flight lease"
        );
        clock.set(poke_at);
        assert!(
            store
                .claim("owner-b", lease, &Default::default())
                .await
                .unwrap()
                .is_none(),
            "a renewed lease is never reclaimable while A stays alive"
        );
    }

    // Renewal stops (A finally dies). The last renewal at t=180 expires at 280,
    // so at t=281 recovery hands the run to B.
    clock.set(281);
    let replacement = store
        .claim("owner-b", lease, &Default::default())
        .await
        .unwrap()
        .expect("once renewal stops, the expired lease is reclaimed");
    assert_eq!(replacement.lease.owner(), "owner-b");
    assert!(replacement.lease.epoch() > claim.epoch);
    clock.set(282);
    assert_eq!(
        store.renew_lease(&claim, lease).await.unwrap(),
        awaken_dispatch::LeaseRenewalOutcome::Lost,
        "the old exact claim remains fenced after recovery",
    );
}

// --- 5. Reclaim after quiescence — the committed LOG stays exactly-once ------

#[tokio::test]
async fn reclaim_after_process_bound_attempt_quiesces_applies_never_replay_policy() {
    // Test design. Causes: C1 owner A commits tool Executing then blocks; C2 its
    // lease expires and B takes the mutation claim; C3 A's owned executor Future
    // cooperatively returns after the release gate opens; C4 A's later commit is
    // fenced. Effects: E1 before C3, B remains physically blocked and no second
    // external effect enters; E2 after C3, `run_physical_attempt` records the one
    // exact quiescence ACK and B applies NeverReplay; E3 one terminal log
    // survives. Constraints: lease expiry is not quiescence, and abort/drop is
    // covered separately by RG10 as a permanently occupied slot. Decision rules:
    // R1=C1+C2+!C3 -> E1; R2=C1+C2+C3+C4 -> E2+E3.
    // Owner A commits Requested -> Executing before entering the blocking tool.
    // When its lease lapses, owner B recovers that durable phase. The descriptor's
    // conservative default is NeverReplay, so B publishes an Indeterminate tool
    // result and continues without entering the external tool a second time.
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (runtime, ran) = blocking_tool_runtime(release.clone());
    let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
    let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let run = RunId("run-1".to_string());

    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();

    // Owner A claims explicitly and drives in the background. The only code
    // allowed to acknowledge its physical return is the production
    // `run_physical_attempt` scope around that drive.
    clock.set(0);
    let claimed_a = store
        .claim("owner-a", LEASE, &Default::default())
        .await
        .expect("A claim query")
        .expect("A claims");
    let claim_a = RunClaim::from(&claimed_a.lease);
    let worker_a = Arc::new(
        DispatchWorker::new(runtime.clone(), store.clone(), commit.clone(), "owner-a")
            .with_lease_ms(LEASE),
    );
    let a_handle = {
        let worker_a = worker_a.clone();
        let drive_clock = harness::at(&clock, 0);
        tokio::spawn(async move { worker_a.drive_claimed(claimed_a, drive_clock).await })
    };

    // Wait until A is frozen inside the tool (its first invocation).
    let frozen = tokio::time::timeout(Duration::from_secs(5), async {
        while ran.load(Ordering::SeqCst) < 1 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(frozen.is_ok(), "A reached and blocked in the tool");
    assert_eq!(ran.load(Ordering::SeqCst), 1, "the tool ran once (owner A)");

    // A is mid-flight: it committed a `Running` fact and awaiting NO awaiting ticket.
    let record = CommittedThreadView::run(commit.as_ref(), &run).expect("record");
    assert_eq!(
        record.state,
        RunState::Running,
        "A committed a mid-flight Running fact"
    );
    assert!(
        commit.resume_ticket_for(&run).is_none(),
        "and there is no awaiting ticket — the reclaim hits the no-ticket branch"
    );

    // B may take the expired mutation claim, but its production drive must wait
    // at physical admission while A's executor Future is still blocked.
    clock.set(LEASE + 1);
    let replacement = store
        .claim("owner-b", LEASE, &Default::default())
        .await
        .expect("C2 B claim query")
        .expect("C2 B replacement claim");
    clock.set(LEASE + 1);
    assert_eq!(
        store
            .begin_attempt(&RunClaim::from(&replacement.lease), None)
            .await
            .expect("R1 B physical admission"),
        AttemptAdmission::Blocked,
        "R1/E1 B's exact durable attempt remains blocked before A returns"
    );
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "R1/E1 replacement admission cannot enter a second external effect"
    );

    // Opening the gate makes A's owned executor Future return normally. The
    // production scope records quiescence before its stale commit is fenced;
    // only then may B recover the same Run without replay.
    release.add_permits(1);
    let a_error = tokio::time::timeout(Duration::from_secs(5), a_handle)
        .await
        .expect("C3 A's background task joins")
        .expect("C3 A's task does not panic")
        .expect_err("C4 A's stale commit is fenced");
    assert!(
        a_error.to_string().contains("superseded")
            && a_error.to_string().contains("no longer holds lease epoch"),
        "C4 stale commit has the exact authority-loss cause: {a_error}"
    );
    assert!(
        !store.list_dispatches().await.unwrap()[0].physical_attempt_active,
        "R2/E2 production return recorded A's exact physical quiescence"
    );
    let worker_b =
        DispatchWorker::new(runtime, store.clone(), commit.clone(), "owner-b").with_lease_ms(LEASE);
    let processed = tokio::time::timeout(
        Duration::from_secs(5),
        worker_b.drive_claimed(replacement, harness::at(&clock, LEASE + 1)),
    )
    .await
    .expect("R2 replacement unblocks")
    .expect("R2 replacement drive succeeds");
    assert_eq!(
        processed,
        Some((run.clone(), RunState::Ended(EndCause::NaturalEnd))),
        "B reclaimed the still-running run and drove it to completion"
    );

    assert_eq!(
        store
            .settle(&run, claim_a.epoch, DispatchOutcome::Done, &[])
            .await
            .expect("stale settle query"),
        awaken_dispatch::SettleOutcome::Fenced,
        "E3 the stale predecessor cannot mutate the completed dispatch"
    );

    // The policy guarantee: the unknown external side effect is not repeated.
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "NeverReplay prevents a second external invocation"
    );

    // The committed log is also exactly-once. The recovered run has exactly ONE
    // terminal `Ended` fact and the
    // transcript carries exactly ONE final "all done" assistant message.
    let committed = commit.committed();
    let ended_facts = committed
        .run_facts
        .iter()
        .filter(|fact| fact.run_id == run && matches!(fact.state, RunState::Ended(_)))
        .count();
    assert_eq!(
        ended_facts, 1,
        "recovery produced exactly one terminal Ended fact"
    );
    let all_done = committed
        .messages
        .iter()
        .filter(|message| message_text(message).as_deref() == Some("all done"))
        .count();
    assert_eq!(
        all_done, 1,
        "exactly one final assistant message — no duplicate terminal Step"
    );

    // The reclaimed run's input is committed exactly once: A committed "go" in its
    // first step delta, and B's re-execute seeds it from committed history and
    // drops the activation copy (deduped by stable message id), rather than
    // replaying it. Locks in the idempotent-input fix.
    let go_inputs = committed
        .messages
        .iter()
        .filter(|message| message_text(message).as_deref() == Some("go"))
        .count();
    assert_eq!(
        go_inputs, 1,
        "the reclaimed run's input is committed exactly once, not replayed on re-execute"
    );
}

/// The concatenated text of a message's text blocks, if any — a small test helper
/// for asserting on committed transcript content.
fn message_text(message: &awaken_agent_contract::agent::message::Message) -> Option<String> {
    use awaken_agent_contract::agent::content::ContentBlock;
    let text: String = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    (!text.is_empty()).then_some(text)
}

// --- guard: text_runtime is a real end-to-end fresh run (sanity) ------------

#[tokio::test]
async fn fresh_claim_drives_a_run_to_completion() {
    // Coverage rationale. Causes: one fresh dispatch and one uncontended owner.
    // Effects: the Worker claims, executes, commits NaturalEnd, and settles the
    // row. Constraint/Invariant: this control path contains no recovery or
    // concurrency cause. Decision rule: exercise the single valid fresh-claim
    // partition as the baseline for the recovery tests above.
    // A minimal end-to-end sanity check that the worker under test drives a freshly
    // claimed run to a terminal state on a single owner (no contention).
    let runtime = text_runtime();
    let store = Arc::new(MemoryDispatchStore::new(awaken_time::system_clock()));
    let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    let worker = DispatchWorker::new(runtime, store.clone(), commit, "solo").with_lease_ms(LEASE);
    let processed = worker.tick(harness::clock(0)).await.unwrap();
    assert_eq!(
        processed,
        Some((
            RunId("run-1".to_string()),
            RunState::Ended(EndCause::NaturalEnd)
        ))
    );
    assert_eq!(store.dispatch_count(), 0);
    // Settling makes it non-claimable — the queue is now idle.
    assert!(worker.tick(harness::clock(1)).await.unwrap().is_none());
    let _ = DispatchOutcome::Done;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn killed_process_does_not_fabricate_physical_quiescence_on_sqlite_reopen() {
    // W4 A reaches a real executing tool and then is SIGKILLed (no Drop/ACK).
    // W5 reopen independent dispatch/commit DBs and reclaim the expired logical
    // lease => physical slot remains occupied, no B model/tool entry, unchanged
    // execution output. W6 cancellation of B's wait also cannot clear A's slot.
    // This is the fail-closed outcome: process death alone is not an adapter's
    // durable proof that an arbitrary external operation stopped. The existing
    // cooperative-return test owns the successful quiescence/NeverReplay path.
    const MODE: &str = "AWAKEN_PHYSICAL_CRASH_ROOT";
    if let Ok(root) = std::env::var(MODE) {
        let root = std::path::Path::new(&root);
        let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
        let dispatch = Arc::new(
            SqliteDispatchStore::open(root.join("dispatch.db").to_str().unwrap(), clock.clone())
                .await
                .unwrap(),
        );
        let commit = Arc::new(
            awaken_store_sqlite::SqliteCommitCoordinator::open(
                root.join("commit.db").to_str().unwrap(),
            )
            .await
            .unwrap(),
        );
        let (runtime, calls) = blocking_tool_runtime(Arc::new(tokio::sync::Semaphore::new(0)));
        dispatch
            .enqueue(RunDispatch::new(activation("crash-physical")))
            .await
            .unwrap();
        let worker =
            DispatchWorker::new(runtime, dispatch, commit, "process-a").with_lease_ms(LEASE);
        let driving = tokio::spawn(async move { worker.tick(harness::at(&clock, 0)).await });
        tokio::time::timeout(Duration::from_secs(5), async {
            while calls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!driving.is_finished());
        std::fs::write(root.join("marker"), b"tool-entered").unwrap();
        tokio::time::sleep(Duration::from_secs(30)).await;
        panic!("parent failed to kill child");
    }
    let root = tempfile::tempdir().unwrap();
    let status = awaken_reliability_testkit::CrashProcess::new(
        "killed_process_does_not_fabricate_physical_quiescence_on_sqlite_reopen",
        root.path().join("marker"),
    )
    .env(MODE, root.path())
    .run()
    .unwrap();
    assert!(!status.success());
    let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let dispatch = Arc::new(
        SqliteDispatchStore::open(
            root.path().join("dispatch.db").to_str().unwrap(),
            clock.clone(),
        )
        .await
        .unwrap(),
    );
    let commit = Arc::new(
        awaken_store_sqlite::SqliteCommitCoordinator::open(
            root.path().join("commit.db").to_str().unwrap(),
        )
        .await
        .unwrap(),
    );
    assert!(
        commit.commit_count() > 0,
        "W4 executing prefix survived death"
    );
    let messages =
        CommittedThreadView::committed_messages(commit.as_ref(), &ThreadId(harness::THREAD.into()));
    let row = dispatch.list_dispatches().await.unwrap().remove(0);
    assert!(row.physical_attempt_active);
    clock.set(LEASE + 1);
    let replacement = dispatch
        .claim("process-b", LEASE, &Default::default())
        .await
        .unwrap()
        .unwrap();
    assert!(replacement.recovered);
    clock.set(LEASE + 1);
    assert_eq!(
        dispatch
            .begin_attempt(&RunClaim::from(&replacement.lease), None)
            .await
            .unwrap(),
        AttemptAdmission::Blocked,
        "W5 durable admission stays blocked after reopen"
    );
    let (runtime, model_calls) = counting_text_runtime();
    let worker = DispatchWorker::new(runtime, dispatch.clone(), commit.clone(), "process-b")
        .with_lease_ms(LEASE);
    let driven = tokio::time::timeout(
        Duration::from_secs(5),
        worker.drive_claimed(replacement, harness::at(&clock, LEASE + 1)),
    )
    .await
    .expect("W5 blocked recovery releases its task")
    .expect("W5 deferral succeeds");
    assert_eq!(driven, None, "W5 blocked mutation claim is deferred");
    assert_eq!(model_calls.load(Ordering::SeqCst), 0, "W5");
    assert!(
        dispatch.list_dispatches().await.unwrap()[0].physical_attempt_active,
        "W6"
    );
    // Recovery may append Rescheduled before waiting for physical admission;
    // it must not invent execution output or a terminal Run fact.
    assert_eq!(
        CommittedThreadView::committed_messages(commit.as_ref(), &ThreadId(harness::THREAD.into())),
        messages
    );
    assert_eq!(
        CommittedThreadView::run(commit.as_ref(), &RunId("crash-physical".into()))
            .unwrap()
            .state,
        RunState::Running
    );
}

#[tokio::test]
async fn native_cancelled_external_future_does_not_acknowledge_physical_return() {
    // N1: Native entered an external tool; N2: ownership loss cancels and drops
    // that tool future; N3: Runtime returns a logical error. E1 no late write;
    // E2 the physical slot stays occupied; E3 successor cannot re-enter. This
    // differs from the adjacent cooperative-return case: future Drop is not
    // termination evidence, even when its enclosing Runtime future returned.
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (runtime, calls, entered) =
        harness::blocking_tool_runtime_with_entry_signal(release.clone());
    let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
    let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    store
        .enqueue(RunDispatch::new(activation("native-drop")))
        .await
        .unwrap();
    clock.set(0);
    let claimed = store
        .claim("owner-a", LEASE, &Default::default())
        .await
        .unwrap()
        .unwrap();
    let worker =
        DispatchWorker::new(runtime, store.clone(), commit.clone(), "owner-a").with_lease_ms(LEASE);
    let drive_clock = harness::at(&clock, 0);
    let driven = tokio::spawn(async move { worker.drive_claimed(claimed, drive_clock).await });
    entered.notified().await;
    let committed_before = commit.commit_count();
    clock.set(LEASE + 1);
    let replacement = store
        .claim("owner-b", LEASE, &Default::default())
        .await
        .unwrap()
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(5), driven)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    release.add_permits(1);
    assert_eq!(commit.commit_count(), committed_before, "E1");
    assert!(
        store.list_dispatches().await.unwrap()[0].physical_attempt_active,
        "E2"
    );
    clock.set(LEASE + 1);
    assert_eq!(
        store
            .begin_attempt(&RunClaim::from(&replacement.lease), None)
            .await
            .unwrap(),
        AttemptAdmission::Blocked,
        "E3"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

use awaken_dispatch_contract::{
    DispatchClaims as _, DispatchControlReads as _, DispatchEnqueue as _, DispatchExecution as _,
    DispatchReads as _, DispatchSettlement as _,
};
