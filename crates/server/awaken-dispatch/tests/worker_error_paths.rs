//! Genuine-fault and scheduled-action drive paths of the dispatch worker.
//!
//! `worker_edge_cases.rs` pins the worker's *committed-truth* decisions; this suite
//! pins what happens when a drive genuinely *fails* (a real storage/commit fault),
//! and the in-process performance of ScheduledActions:
//!
//! 1. a genuine drive failure whose run is NOT terminal is re-raised (never swallowed
//!    as already-done) and the dispatch is left un-settled, so a later claim retries;
//! 2. a failing `perform_scheduled` routes through the same terminal-or-raise fork;
//! 3. a chain of consecutive ScheduledActions is performed to completion in ONE
//!    `drive_claimed`, via the worker's `while state == Awaiting` loop.

mod harness;

use awaken_dispatch_contract::ControlDispatchQueue;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_dispatch::{DispatchQueue, DispatchWorker, Error, MemoryDispatchStore, RunDispatch};
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_store_inmem::MemoryCommitCoordinator;

use harness::{FailingCommit, activation, schedule_n_runtime, text_runtime};

const LEASE: u64 = 1_000;

struct UncertainScheduledExecutor(Arc<awaken_runtime::Runtime>);

#[async_trait::async_trait]
impl RunExecutor for UncertainScheduledExecutor {
    async fn execute(
        &self,
        activation: awaken_runtime_contract::RunActivation,
        context: RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<RunState> {
        let state = self.0.execute(activation, context.clone()).await?;
        context.physical_quiescence.mark_uncertain();
        Ok(state)
    }
}

#[async_trait::async_trait]
impl awaken_runtime_contract::execution::RunAttemptExecutor for UncertainScheduledExecutor {
    async fn resume(
        &self,
        _activation: awaken_runtime_contract::RunActivation,
        _command: awaken_runtime_contract::resume::ResumeCommand,
        _context: RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<RunState> {
        panic!("this fresh-admission test never resumes externally");
    }
}

#[tokio::test]
async fn uncertain_scheduled_stage_neither_launches_its_successor_nor_acknowledges() {
    // C1 a real committed ScheduledAction; C2 the initial executor returns but
    // reports an unconfirmed participant. R1 C1+C2 => no scheduled tool entry,
    // no ACK, and no settlement. This kills resetting the uncertainty latch or
    // retaining the old outer schedule loop after moving the physical boundary.
    let (runtime, ran) = schedule_n_runtime(1);
    let store = Arc::new(harness::FlakyDispatchStore::new(
        0,
        awaken_lease_contract::system_clock(),
    ));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let worker = DispatchWorker::new(runtime.clone(), store.clone(), commit, "uncertain");
    worker.install_attempt_executor(Arc::new(UncertainScheduledExecutor(runtime)));
    store
        .enqueue(RunDispatch::new(activation("uncertain-schedule")))
        .await
        .unwrap();
    let _outcome = worker.tick(harness::clock(0)).await;
    assert_eq!(ran.load(Ordering::SeqCst), 0, "no next-stage effect");
    assert!(store.finish_claims().is_empty(), "no ACK");
    assert_eq!(store.dispatch_count(), 1, "no settlement");
}

#[tokio::test]
async fn failed_retry_exhaustion_commit_is_reclaimable_after_its_exact_lease() {
    // Cause/effect decision table: T1 exhausted+expired and commit fails => keep
    // the newly claimed row leased, publish no terminal/completion/quarantine;
    // T2 retry at exact expiry => strict lease fence returns no claim; T3 retry
    // after expiry, even though the special claim raised attempts above max =>
    // reclaim, commit Indeterminate, settle Done, publish one tombstone.
    // Causes: retry exhaustion, exact lease time, post-expiry time, and injected
    // commit failure select T1-T3. Effects: T1 retains evidence, T2 fences, T3
    // completes once. Constraint/Invariant: failed terminal commit cannot consume
    // or quarantine the claim. Decision rule: execute T1, T2, then T3 in order.
    let runtime = text_runtime();
    let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
    let inner = Arc::new(MemoryCommitCoordinator::new());
    let commit = Arc::new(FailingCommit::new(inner.clone(), true));
    store
        .enqueue(RunDispatch::new(activation("exhausted")))
        .await
        .unwrap();
    clock.set(0);
    store
        .claim("crashed", 100, &Default::default())
        .await
        .unwrap()
        .unwrap();
    clock.set(200);
    store
        .claim("crashed", 100, &Default::default())
        .await
        .unwrap()
        .unwrap();

    let worker = DispatchWorker::new(runtime, store.clone(), commit.clone(), "resolver")
        .with_lease_ms(LEASE);
    // The store's own clock decides retry-exhaustion claim liveness, so the
    // decision table's time coordinates are set on that one clock.
    clock.set(400);
    assert!(worker.resolve_one_retry_exhausted(1).await.is_err(), "T1");
    assert!(inner.committed().latest_run.is_none(), "T1");
    assert!(store.dead_letters().await.unwrap().is_empty(), "T1");
    assert!(
        store
            .completion_events_after(0, 10)
            .await
            .unwrap()
            .is_empty(),
        "T1"
    );

    commit.set_failing(false);
    clock.set(1_400);
    assert!(!worker.resolve_one_retry_exhausted(1).await.unwrap(), "T2");
    clock.set(1_401);
    assert!(worker.resolve_one_retry_exhausted(1).await.unwrap(), "T3");
    assert_eq!(
        inner.committed().latest_run.unwrap().state,
        RunState::Ended(EndCause::Indeterminate),
        "T3"
    );
    assert_eq!(store.dispatch_count(), 0, "T3");
    assert_eq!(
        store.completion_events_after(0, 10).await.unwrap().len(),
        1,
        "T3"
    );
}

// --- 1. Genuine drive failure is re-raised, not swallowed -------------------

#[tokio::test]
async fn a_genuine_drive_failure_is_reraised_and_the_dispatch_is_left_unsettled() {
    // Test design. Causes: C1 a fresh Run hits an injected commit fault; C2 no
    // terminal committed truth exists. Effects: E1 the Worker returns the error;
    // E2 the dispatch remains leased/reclaimable; E3 it is not misclassified Done.
    // Constraint/Invariant: only terminal committed truth may convert a drive
    // error into benign settlement. Decision rule: force C1+C2, then prove a
    // later post-expiry claim exists.
    // A fresh run whose runtime cannot commit (an injected storage fault) fails to
    // drive. Because committed truth shows the run is NOT terminal, the worker must
    // re-raise the error — never mistake it for the benign lost-race "already done"
    // — and must NOT settle the dispatch, so a later claim re-runs it.
    let runtime = text_runtime();
    let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
    let inner = Arc::new(MemoryCommitCoordinator::new());
    let commit = Arc::new(FailingCommit::new(inner.clone(), true));

    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    let worker = DispatchWorker::new(runtime, store.clone(), commit, "solo").with_lease_ms(LEASE);

    let err = worker
        .tick(harness::at(&clock, 0))
        .await
        .expect_err("a genuine commit fault must propagate, not settle as done");
    assert!(
        matches!(err, Error::Execution(_)),
        "a failed runtime drive is an execution error, got {err:?}"
    );

    // Committed truth never reached a terminal record.
    assert!(
        !matches!(
            inner.committed().latest_run.map(|r| r.state),
            Some(RunState::Ended(_))
        ),
        "the run never committed a terminal record"
    );

    // The dispatch was left un-settled: it is still present and reclaimable once the
    // lease lapses, so the fault is retried rather than silently dropped.
    assert_eq!(
        store.dispatch_count(),
        1,
        "a failed drive does not settle the dispatch"
    );
    clock.set(LEASE + 1);
    assert!(
        store
            .claim("recovery", LEASE, &Default::default())
            .await
            .unwrap()
            .is_some(),
        "the un-settled dispatch is reclaimable for a retry after the lease expires"
    );
}

// --- 2. A failing scheduled action routes through terminal-or-raise ---------

#[tokio::test]
async fn a_failing_scheduled_action_is_reraised_and_left_unsettled() {
    // Test design. Causes: C1 a committed ScheduledAction awaits; C2 performing
    // it hits an injected commit fault; C3 committed Run remains nonterminal.
    // Effects: E1 the error is reraised; E2 the dispatch remains reclaimable.
    // Constraint/Invariant: scheduled action, execute, and resume share the same
    // terminal-or-raise decision. Decision rule: force C1+C2+C3 and require E1/E2.
    // A run awaiting on a committed ScheduledAction is driven by a worker whose commit
    // fails: performing the deferred action errors. The run is still non-terminal, so
    // the worker re-raises and leaves the dispatch un-settled — the scheduled path
    // obeys the same fork as the execute/resume paths.
    let (runtime, _ran) = schedule_n_runtime(1);
    let store = Arc::new(MemoryDispatchStore::new(
        awaken_lease_contract::system_clock(),
    ));
    let inner = Arc::new(MemoryCommitCoordinator::new());

    // Await the run on a ScheduledAction with a healthy commit boundary first.
    let ctx = RuntimeRunContext::new().with_commit(inner.clone());
    let awaiting = runtime.execute(activation("run-1"), ctx).await.unwrap();
    assert_eq!(
        awaiting,
        RunState::Awaiting,
        "the run awaiting on a ScheduledAction"
    );

    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();

    // Now drive it with a worker whose commit fails, so performing the action errors.
    let commit = Arc::new(FailingCommit::new(inner.clone(), true));
    let worker = DispatchWorker::new(runtime, store.clone(), commit, "live").with_lease_ms(LEASE);

    let err = worker
        .tick(harness::clock(100))
        .await
        .expect_err("a failing perform_scheduled must propagate");
    assert!(
        matches!(err, Error::Execution(_)),
        "a failed scheduled action is an execution error, got {err:?}"
    );

    // Still non-terminal and still un-settled — the dispatch is left for recovery.
    assert!(
        matches!(
            inner.committed().latest_run.map(|r| r.state),
            Some(RunState::Awaiting)
        ),
        "the run stays awaiting (non-terminal) after the failed action"
    );
    assert_eq!(
        store.dispatch_count(),
        1,
        "a failed scheduled action does not settle the dispatch"
    );
}

// --- 3. A chain of scheduled actions is performed in one drive --------------

#[tokio::test]
async fn a_chain_of_scheduled_actions_is_performed_to_completion_in_one_drive() {
    // Test design. Causes: C1 a Run commits two consecutive ScheduledActions;
    // C2 one Worker drive owns the claim. Effects: E1 both actions perform in
    // order; E2 the Run ends and settles without intermediate Awaiting settlement.
    // Constraint/Invariant: the current claim remains authority across the
    // in-process schedule chain. Decision rule: schedule exactly two actions and
    // require two effects plus one terminal drive result.
    // A run that commits two consecutive ScheduledActions (schedule → perform →
    // schedule → perform → end) must be driven to an end within a SINGLE
    // drive_claimed: the worker's `while state == Awaiting` loop keeps performing the
    // next committed action in-process until the run ends, never settling Awaiting
    // between hops.
    // R1 normal completion -> one ACK for the whole chain; R2 ACK response
    // lost -> retry that ACK only, never any scheduled effect.
    for lost_response in [false, true] {
        let (runtime, ran) = schedule_n_runtime(2);
        let store = harness::FlakyDispatchStore::new(0, awaken_lease_contract::system_clock());
        let store = Arc::new(if lost_response {
            store.with_finish_fault(harness::FinishFault::LostResponse)
        } else {
            store
        });
        let commit = Arc::new(MemoryCommitCoordinator::new());

        store
            .enqueue(RunDispatch::new(activation("run-1")))
            .await
            .unwrap();
        let worker =
            DispatchWorker::new(runtime, store.clone(), commit, "solo").with_lease_ms(LEASE);

        let processed = worker.tick(harness::clock(0)).await.unwrap();
        assert_eq!(
            processed,
            Some((
                RunId("run-1".to_string()),
                RunState::Ended(EndCause::NaturalEnd)
            )),
            "the chain drove to a natural end in one drive"
        );
        assert_eq!(
            ran.load(Ordering::SeqCst),
            2,
            "both scheduled actions were performed in the one drive"
        );
        assert_eq!(
            store.dispatch_count(),
            0,
            "the run settled Done after the whole chain, not Awaiting mid-chain"
        );
        let acknowledgements = store.finish_claims();
        assert_eq!(acknowledgements.len(), if lost_response { 2 } else { 1 });
        assert!(
            acknowledgements
                .iter()
                .all(|claim| claim == &acknowledgements[0])
        );
    }
}
