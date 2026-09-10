//! Integration tests for the durable worker's CRASH-BEFORE-SETTLE recovery.
//!
//! The load-bearing claim in `worker.rs` (and mirrored by every store) is that
//! *pending input is consumed on settle, not on read*. `claim` HANDS the run's
//! pending input to the worker but never removes it; only `settle` removes exactly
//! what the worker reports it consumed. So a crash in the window between claim and
//! settle re-delivers the input exactly once on the next claim — no loss (it
//! survived the crash) and no duplicate (a settle that names it removes it, and it
//! is never handed a third time).
//!
//! These pin three behaviors:
//!  1. a bound pending input for an awaiting run survives a claim-without-settle and
//!     is re-delivered and driven exactly once by the recovering owner;
//!  2. unbound idle-thread input drained into a fresh run is consumed on settle,
//!     not on read, so a crash before settle leaves it for re-delivery — and it is
//!     NOT re-delivered to a later run once a settle has consumed it;
//!  3. the commit boundary is atomic: a commit writes messages + run-fact + events
//!     and a ticket in one transaction, so after a successful commit exactly the
//!     expected rows exist and survive a fresh hydrate (projection == replay, no
//!     orphan message without its run-fact); a rejected commit adds no rows.
//!
//! The crash is modeled the way the task blesses: an owner claims (or the store
//! hands the input) and never settles — a dropped claim stands in for the process
//! that died before `settle`.

mod harness;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::coordinator::Coordinator as CommitCoordinator;
use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_agent_contract::thread::read::lifecycle::{
    CheckpointRunLifecycleFeed, RunLifecycleCursor, RunLifecycleEventKind, RunLifecycleFeed,
};
use awaken_dispatch::{
    Dispatch, DispatchOutcome, DispatchQueue, DispatchWorker, Inbox, ManualClock,
    MemoryDispatchStore, RunDispatch, SettleOutcome, SqliteDispatchStore,
};
use awaken_runtime_contract::resume::ResumeResult;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::terminal::{
    CommittedTerminalRun, RunTerminalObserver, RunTerminalObserverError,
};
use awaken_store_inmem::MemoryCommitCoordinator;
use awaken_store_sqlite::SqliteCommitCoordinator;

use harness::{THREAD, TICKET, activation, input_echo_runtime, pending, tool_runtime};

const LEASE: u64 = 1_000;
const SQLITE_CRASH_MODE: &str = "AWAKEN_PENDING_INPUT_CRASH_ROOT";

#[derive(Default)]
struct TerminalCount {
    attempts: AtomicUsize,
    successes: AtomicUsize,
    span_names: Mutex<Vec<Option<&'static str>>>,
}

#[async_trait::async_trait]
impl RunTerminalObserver for TerminalCount {
    fn observer_id(&self) -> &str {
        "durable-recovery-test"
    }

    async fn observe(
        &self,
        _terminal: &CommittedTerminalRun,
    ) -> Result<(), RunTerminalObserverError> {
        self.span_names
            .lock()
            .unwrap()
            .push(tracing::Span::current().metadata().map(|meta| meta.name()));
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(RunTerminalObserverError(
                "injected first delivery failure".to_string(),
            ));
        }
        self.successes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn allow() -> ResumeResult {
    ResumeResult::allow()
}

// --- 1. Bound pending survives a crash-before-settle and re-delivers once -----

#[tokio::test]
async fn crash_before_settle_re_delivers_bound_pending_exactly_once() {
    // A run awaits on a gate holding a committed awaiting ticket. Its correctly
    // correlated pending input arrives. An owner claims the woken run (the store
    // HANDS the pending) but crashes before settling — modeled by dropping the
    // claim. The pending must survive (consumed on settle, not on read). The
    // recovering owner then reclaims after the lease lapses, delivers the SAME
    // pending exactly once (the gated tool runs once), and only THEN is the pending
    // consumed. It is never delivered a third time.
    //
    // Reschedule cause/effect graph: C1 the wake claim expires without settle;
    // C2 its replacement claim is recovered; C3 the exact pending reply makes
    // that replacement execute; C4 the completed claim settles. Effects: E1 the
    // pending is delivered exactly once; E2 the existing lifecycle log records
    // Rescheduled before the resumed attempt's ordinary Running checkpoints and
    // terminal completion; E3 a later tick has no dispatch and cannot duplicate
    // the fact.
    //
    // | Rule | C1 | C2 | C3 | C4 | Effects |
    // |---|---|---|---|---|---|
    // | R1 | T | T | T | T | E1,E2,E3 |
    // Constraint/Invariant: input is consumed only by a fenced successful
    // settlement, while lifecycle remains the single retry fact log. Decision rule:
    // R1 exercises the crash window and proves one delivery, ordered
    // reschedule, and no third claim.
    let (runtime, ran) = tool_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let run = RunId("run-1".to_string());

    let worker =
        DispatchWorker::new(runtime, store.clone(), commit.clone(), "w").with_lease_ms(LEASE);

    // The fresh run awaits on the gate; the gated tool has not run.
    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    assert_eq!(
        worker.tick(harness::clock(0)).await.unwrap(),
        Some((run.clone(), RunState::Awaiting)),
        "the fresh run awaits on the gate ticket"
    );
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "the gated tool has not run yet"
    );

    // The correctly-correlated input arrives.
    store
        .append(pending("good", "run-1", TICKET, allow()))
        .await
        .unwrap();
    assert_eq!(store.pending_count(&run), 1);

    // An owner claims the woken run (a wake pick hands the pending) then CRASHES
    // before settling: dropping the claim leaves the run leased-but-unsettled.
    let crashed = store
        .claim("crasher", LEASE, 10, &Default::default())
        .await
        .unwrap()
        .expect("the awaiting run with due input is claimable");
    assert!(
        crashed.pending.iter().any(|p| p.message_id == "good"),
        "the store hands the pending input to the claiming owner"
    );
    drop(crashed); // crash: no settle, nothing consumed.

    // No loss: the pending survived the crash — it is consumed on settle, not on
    // read/claim.
    assert_eq!(
        store.pending_count(&run),
        1,
        "a claim without settle does not consume the pending input"
    );
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "the crashed owner drove nothing"
    );

    // Recovery: after the lease lapses, the worker reclaims and delivers the SAME
    // pending exactly once, driving the run to completion.
    let recovered = worker.tick(harness::clock(10 + LEASE + 1)).await.unwrap();
    assert_eq!(
        recovered,
        Some((run.clone(), RunState::Ended(EndCause::NaturalEnd))),
        "the recovering owner delivers the re-delivered input and drives to Ended"
    );
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "the re-delivered input drove the gated tool exactly once"
    );
    let lifecycle = CheckpointRunLifecycleFeed::new(commit.clone())
        .events_after(RunLifecycleCursor::default(), 20)
        .await
        .expect("R1 lifecycle feed");
    assert_eq!(
        lifecycle
            .events
            .iter()
            .map(|event| event.kind)
            .collect::<Vec<_>>(),
        vec![
            RunLifecycleEventKind::Running,
            RunLifecycleEventKind::Awaiting,
            RunLifecycleEventKind::Rescheduled,
            RunLifecycleEventKind::Resumed,
            RunLifecycleEventKind::Running,
            RunLifecycleEventKind::Completed,
        ],
        "R1/E2"
    );

    // Consumed only after settle, and never re-delivered a third time.
    assert_eq!(
        store.pending_count(&run),
        0,
        "the pending is consumed only after the settling drive"
    );
    assert_eq!(store.dispatch_count(), 0, "the settled dispatch is gone");
    assert!(
        worker
            .tick(harness::clock(10 + 2 * LEASE))
            .await
            .unwrap()
            .is_none(),
        "nothing is left to drive"
    );
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "and the tool never ran again"
    );
    let replay = CheckpointRunLifecycleFeed::new(commit)
        .events_after(lifecycle.next_cursor, 20)
        .await
        .expect("R1 replay after terminal cursor");
    assert!(replay.events.is_empty(), "R1/E3");
}

#[tokio::test]
async fn sqlite_process_crash_before_settle_replays_and_consumes_exact_input_once() {
    // Cause/effect decision table. C1 a file-backed SQLite Awaiting row and its
    // bound input are committed; C2 the exact claimant is SIGKILLed before
    // settle; C3 a fresh adapter reopens after lease expiry; C4 the old/new
    // epochs each attempt to name the input; C5 a second fresh adapter reopens.
    // E1 C2 preserves the exact row; E2 C3 re-hands it under epoch+1; E3 C4(old)
    // is fenced and preserves it; E4 C4(new) consumes it; E5 C5 observes no
    // pending row and no third wake. Constraint: the marker identifies only the
    // post-claim kill boundary; all persistence evidence comes from independently
    // opened production adapters. Rules: PCR1=C1+C2+C3 -> E1+E2;
    // PCR2=PCR1+C4(old/new) -> E3+E4; PCR3=PCR2+C5 -> E5.
    if let Ok(root) = std::env::var(SQLITE_CRASH_MODE) {
        let root = std::path::Path::new(&root);
        let clock = Arc::new(ManualClock::new(0));
        let store = SqliteDispatchStore::open(root.join("dispatch.db").to_str().unwrap())
            .expect("PCR1 open child store")
            .with_clock(clock);
        let run = RunId("run-process-crash".to_string());
        store
            .enqueue(RunDispatch::new(activation("run-process-crash")))
            .await
            .expect("PCR1 enqueue");
        let initial = store
            .claim_run(&run, "initial", LEASE, 0, &Default::default())
            .await
            .expect("PCR1 initial claim")
            .expect("PCR1 fresh Run is claimable");
        assert_eq!(
            store
                .settle(&run, initial.lease.epoch, DispatchOutcome::Awaiting, &[],)
                .await
                .expect("PCR1 establish Awaiting"),
            SettleOutcome::Applied
        );
        let input = pending(
            "process-crash-input",
            "run-process-crash",
            TICKET,
            ResumeResult::Input("persist me".to_string()),
        );
        assert!(store.append(input.clone()).await.expect("PCR1 append"));
        let crashed = store
            .claim_run(&run, "crashed", LEASE, 0, &Default::default())
            .await
            .expect("PCR1 crash-window claim")
            .expect("PCR1 pending input wakes Awaiting Run");
        assert_eq!(crashed.pending, vec![input], "PCR1/E1 exact child handoff");
        std::fs::write(
            root.join("claimed.marker"),
            crashed.lease.epoch.to_string().as_bytes(),
        )
        .expect("PCR1 publish post-claim marker");
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        panic!("parent failed to kill pending-input crash child");
    }

    let root = tempfile::tempdir().expect("PCR1 temp root");
    let marker = root.path().join("claimed.marker");
    let status = awaken_reliability_testkit::CrashProcess::new(
        "sqlite_process_crash_before_settle_replays_and_consumes_exact_input_once",
        &marker,
    )
    .env(SQLITE_CRASH_MODE, root.path())
    .run()
    .expect("PCR1 child reaches claim and is killed");
    assert!(!status.success(), "PCR1/C2 child was terminated");
    let crashed_epoch = std::fs::read_to_string(&marker)
        .expect("PCR1 read claimed epoch")
        .parse::<u64>()
        .expect("PCR1 marker is an epoch");
    let run = RunId("run-process-crash".to_string());
    let thread = ThreadId(THREAD.to_string());
    let expected = pending(
        "process-crash-input",
        "run-process-crash",
        TICKET,
        ResumeResult::Input("persist me".to_string()),
    );
    let clock = Arc::new(ManualClock::new(LEASE + 1));
    {
        let reopened = SqliteDispatchStore::open(root.path().join("dispatch.db").to_str().unwrap())
            .expect("PCR1 reopen after crash")
            .with_clock(clock.clone());
        let recovered = reopened
            .claim_run(&run, "recovered", LEASE, LEASE + 1, &Default::default())
            .await
            .expect("PCR1 recovery claim")
            .expect("PCR1 crashed lease is reclaimable");
        assert_eq!(recovered.lease.epoch, crashed_epoch + 1, "PCR1/E2");
        assert_eq!(recovered.pending, vec![expected.clone()], "PCR1/E1+E2");
        assert_eq!(
            reopened
                .settle(
                    &run,
                    crashed_epoch,
                    DispatchOutcome::Awaiting,
                    std::slice::from_ref(&expected.message_id),
                )
                .await
                .expect("PCR2 stale settle"),
            SettleOutcome::Fenced,
            "PCR2/E3"
        );
        let after_stale = reopened.list(&thread).await.expect("PCR2 inspect pending");
        assert_eq!(after_stale.len(), 1, "PCR2/E3 one exact row remains");
        assert_eq!(after_stale[0].input, expected, "PCR2/E3 stale is inert");
        assert_eq!(
            reopened
                .settle(
                    &run,
                    recovered.lease.epoch,
                    DispatchOutcome::Awaiting,
                    &["process-crash-input".to_string()],
                )
                .await
                .expect("PCR2 current settle"),
            SettleOutcome::Applied,
            "PCR2/E4"
        );
    }
    let reopened = SqliteDispatchStore::open(root.path().join("dispatch.db").to_str().unwrap())
        .expect("PCR3 second reopen")
        .with_clock(clock);
    assert!(
        reopened
            .list(&thread)
            .await
            .expect("PCR3 pending read")
            .is_empty(),
        "PCR3/E5"
    );
    assert!(
        reopened
            .claim_run(&run, "third", LEASE, LEASE + 1, &Default::default())
            .await
            .expect("PCR3 duplicate wake probe")
            .is_none(),
        "PCR3/E5 consumed input cannot wake a third delivery"
    );
}

// --- 2. Unbound idle-thread input consumed on settle, not on read -------------

#[tokio::test]
async fn crash_before_settle_re_delivers_unbound_inbox_input_exactly_once() {
    // Test design. Causes: C1 idle-thread input is unbound; C2 a fresh claim is
    // dropped before drain/settle; C3 its lease expires and recovery claims it;
    // C4 a later fresh Run uses the same Thread. Effects: E1 C2 does not consume
    // input; E2 C3 delivers and consumes it once; E3 C4 cannot redeliver it.
    // Constraint/Invariant: claims only read input; exact settlement owns
    // consumption. Decision rule: exercise C1+C2+C3, then C4 as the duplicate
    // probe and require one transcript occurrence.
    // Unbound idle-thread input (empty run id) is drained into a FRESH run by the
    // worker and recorded consumed only via that run's settle. An owner claims the
    // fresh run but crashes before it drains/settles — modeled by a dropped claim.
    // The unbound input survives (a claim does not consume it), the recovering
    // owner drains and delivers it exactly once, and it is NOT re-delivered to a
    // later fresh run on the same thread once a settle has consumed it.
    let runtime = input_echo_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let thread = ThreadId(THREAD.to_string());

    let worker =
        DispatchWorker::new(runtime, store.clone(), commit.clone(), "w").with_lease_ms(LEASE);

    // An unbound idle-thread message waits for the thread (empty run + correlation).
    store
        .append(pending(
            "u1",
            "",
            "",
            ResumeResult::Input("from-inbox".to_string()),
        ))
        .await
        .unwrap();

    // A fresh run is enqueued. An owner claims it but crashes before draining or
    // settling — the unbound input is a worker-time drain, so a bare claim never
    // touches it.
    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    let crashed = store
        .claim("crasher", LEASE, 0, &Default::default())
        .await
        .unwrap()
        .expect("the fresh run is claimable");
    assert!(
        crashed.pending.is_empty(),
        "unbound input is not handed at claim — it is drained by the worker at drive time"
    );
    drop(crashed);

    // No loss: the unbound input survived the crash (reads do not consume it).
    assert!(
        store
            .list(&thread)
            .await
            .unwrap()
            .iter()
            .any(|r| r.input.message_id == "u1"),
        "the unbound input survives a claim without settle"
    );

    // Recovery: the worker reclaims the fresh run after the lease lapses, drains the
    // unbound input into the run's activation, delivers it exactly once, and settles.
    let recovered = worker.tick(harness::clock(LEASE + 1)).await.unwrap();
    assert_eq!(
        recovered,
        Some((
            RunId("run-1".to_string()),
            RunState::Ended(EndCause::NaturalEnd)
        ))
    );
    let assistant = commit
        .committed()
        .messages
        .iter()
        .find(|m| m.role == Role::Assistant)
        .expect("assistant reply")
        .clone();
    assert!(
        assistant.text_content().contains("from-inbox"),
        "the re-delivered unbound input reached the recovering run (got {:?})",
        assistant.text_content()
    );

    // Consumed on settle.
    assert!(
        store.list(&thread).await.unwrap().is_empty(),
        "the unbound input is consumed on settle"
    );

    // No duplicate: a later fresh run on the same thread does NOT drain the consumed
    // input again. The drained unbound input enters the transcript as a User message
    // carrying its own message id ("u1"); a re-delivery would insert a SECOND such
    // message. (Its text may still appear in a later run's reply because the thread's
    // committed history is replayed as context — that is history, not re-delivery —
    // so we assert on the message id, not the echoed text.)
    store
        .enqueue(RunDispatch::new(harness::activation("run-2")))
        .await
        .unwrap();
    let next = worker.tick(harness::clock(LEASE + 2)).await.unwrap();
    assert_eq!(
        next,
        Some((
            RunId("run-2".to_string()),
            RunState::Ended(EndCause::NaturalEnd)
        ))
    );
    let u1_deliveries = commit
        .committed()
        .messages
        .iter()
        .filter(|m| m.id.0 == "u1")
        .count();
    assert_eq!(
        u1_deliveries, 1,
        "the unbound input was drained into the transcript exactly once — not re-delivered to a later run"
    );
}

#[tokio::test]
async fn recovering_a_terminal_run_consumes_its_delivered_unbound_input() {
    // Test design.
    // C: C1 a fresh attempt drains unbound input, commits Ended, then crashes
    // before settle; C2 the first terminal-observer delivery fails; C3 its lease
    // expires and replay succeeds; C4 a later ordinary durable Run also ends; C5
    // each claim carries the persisted durable causal context.
    // E: E1 C2 leaves committed Ended authoritative but withholds Done and inbox
    // consumption; E2 C3 publishes the terminal fact successfully exactly once,
    // then settles Done and consumes only committed transcript ids; E3 C4 uses
    // the same Worker-owned delivery once, without a parallel Runtime delivery;
    // E4 all fresh/replayed deliveries execute under the claim's sole
    // `wake.dispatch` continuation, so detached auxiliary work can inherit it.
    // K: committed Run truth is never rewritten by observer failure; the leased
    // dispatch is the sole replay authority, and only successful delivery permits
    // terminal settlement and consumption.
    // D: R1=(C1,C2) => Ended + Leased + input retained + zero successes;
    // R2=(C1,C3) => Ended + Done + input consumed + one success;
    // R3=(C4,C5) => Done + one additional attempt and success + E4, proving one
    // owner and one durable trace relay.
    // Regression: the harder crash window. A fresh run drains unbound idle-thread
    // input into its activation, COMMITS its terminal record, then crashes BEFORE
    // settle. On recovery the worker sees a terminal committed record and settles
    // Done from committed truth. It must ALSO consume the unbound inbox rows that the
    // crashed attempt already folded into the committed transcript — otherwise those
    // rows linger and a later fresh run on the thread drains the SAME user input a
    // second time (a duplicate delivery across runs). Committed truth is authority:
    // a row IS consumed iff its message id appears in the committed transcript.
    awaken_observability::init(&awaken_observability::ObservabilityConfig::default());
    let runtime = input_echo_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let thread = ThreadId(THREAD.to_string());
    let run = RunId("run-1".to_string());

    // An unbound idle-thread message waits, plus an unbound message that will NOT be
    // delivered by the crashed attempt (it stands in for input that arrived after the
    // terminal commit) — it must survive recovery, proving no over-consumption/loss.
    store
        .append(pending(
            "u1",
            "",
            "",
            ResumeResult::Input("from-inbox".to_string()),
        ))
        .await
        .unwrap();

    // Model the crashed attempt: an owner claims the fresh run, the worker drains u1
    // into the activation and executes it to a committed terminal record, then the
    // process dies before settle (we commit by hand, exactly as the worker would,
    // and never settle).
    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    let _crashed = store
        .claim("crasher", LEASE, 0, &Default::default())
        .await
        .unwrap()
        .expect("fresh claim");
    let mut act = activation("run-1");
    act.input.insert(
        0,
        Message::new(
            MessageId("u1".to_string()),
            Role::User,
            vec![awaken_agent_contract::agent::content::ContentBlock::text(
                "from-inbox",
            )],
        ),
    );
    let ctx = awaken_runtime_contract::runtime_context::RuntimeRunContext::new()
        .with_commit(commit.clone());
    let state =
        awaken_runtime_contract::execution::RunExecutor::execute(runtime.as_ref(), act, ctx)
            .await
            .unwrap();
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    // The crashed attempt delivered u1 into the committed transcript, but the inbox
    // still lists it (never consumed — the settle never ran).
    assert_eq!(
        commit
            .committed()
            .messages
            .iter()
            .filter(|m| m.id.0 == "u1")
            .count(),
        1,
        "the crashed attempt committed u1 into the transcript"
    );
    assert!(
        list_has_u1(store.as_ref(), &thread).await,
        "u1 still lingers in the inbox"
    );

    // The first recovery delivery fails after Ended is already committed. It must
    // not settle Done or consume the input needed by the same dispatch's replay.
    let observer = Arc::new(TerminalCount::default());
    let worker = DispatchWorker::new(runtime.clone(), store.clone(), commit.clone(), "w")
        .with_context(RuntimeRunContext::new().with_terminal_observer(observer.clone()))
        .with_lease_ms(LEASE);
    let delivery_error = worker
        .tick(harness::clock(LEASE + 1))
        .await
        .expect_err("the first terminal-observer delivery is injected to fail");
    assert!(
        matches!(
            delivery_error,
            awaken_dispatch::Error::Execution(
                awaken_runtime_contract::execution::Error::Execution(message)
            ) if message.contains("durable-recovery-test")
                && message.contains("dispatch settlement withheld")
        ),
        "observer delivery failure uses the existing execution-error seam"
    );
    assert_eq!(
        commit.run_state(&run),
        Some(RunState::Ended(EndCause::NaturalEnd)),
        "post-commit observer failure cannot rewrite the Run result"
    );
    let rows = store.list_dispatches().await.unwrap();
    assert_eq!(rows.len(), 1, "the failed delivery retains its dispatch");
    assert_eq!(
        rows[0].state,
        awaken_dispatch::DispatchState::Leased,
        "observer failure withholds Done until lease-expiry recovery"
    );
    assert!(
        store
            .completion_events_after(0, 10)
            .await
            .unwrap()
            .is_empty(),
        "no Done tombstone is published before observer success"
    );
    assert!(
        list_has_u1(store.as_ref(), &thread).await,
        "settlement-gated inbox consumption is also withheld"
    );
    assert_eq!(observer.attempts.load(Ordering::SeqCst), 1);
    assert_eq!(observer.successes.load(Ordering::SeqCst), 0);

    // Once that failed claim's lease expires, committed truth is redelivered. Only
    // the successful replay may settle Done and consume the delivered input.
    let recovered = worker.tick(harness::clock(2 * LEASE + 2)).await.unwrap();
    assert_eq!(
        recovered,
        Some((run.clone(), RunState::Ended(EndCause::NaturalEnd))),
        "the terminal record is settled Done without re-execution"
    );
    assert!(
        !list_has_u1(store.as_ref(), &thread).await,
        "the delivered unbound input is consumed on the recovery settle, not orphaned"
    );
    assert_eq!(
        observer.attempts.load(Ordering::SeqCst),
        2,
        "recovery makes one failed delivery and one lease-expiry replay"
    );
    assert_eq!(
        observer.successes.load(Ordering::SeqCst),
        1,
        "the recovered terminal fact has exactly one successful publication"
    );
    let completions = store.completion_events_after(0, 10).await.unwrap();
    assert_eq!(
        completions.len(),
        1,
        "successful replay settles exactly once"
    );
    assert_eq!(completions[0].run_id, run);
    assert!(
        store.list_dispatches().await.unwrap().is_empty(),
        "Done is represented by the completion tombstone, not a live row"
    );

    // A later fresh run on the same thread does NOT drain u1 again: exactly one
    // committed delivery of the user's input across the whole thread.
    store
        .enqueue(RunDispatch::new(harness::activation("run-2")))
        .await
        .unwrap();
    assert_eq!(
        worker.tick(harness::clock(2 * LEASE + 3)).await.unwrap(),
        Some((
            RunId("run-2".to_string()),
            RunState::Ended(EndCause::NaturalEnd)
        ))
    );
    assert_eq!(
        commit
            .committed()
            .messages
            .iter()
            .filter(|m| m.id.0 == "u1")
            .count(),
        1,
        "the user's unbound input drove exactly one run — no duplicate re-delivery"
    );
    assert_eq!(
        observer.attempts.load(Ordering::SeqCst),
        3,
        "ordinary durable execution adds one Worker-owned observer attempt"
    );
    assert_eq!(
        observer.successes.load(Ordering::SeqCst),
        2,
        "ordinary durable execution adds one success without Runtime duplication"
    );
    assert_eq!(
        observer.span_names.lock().unwrap().as_slice(),
        [
            Some("wake.dispatch"),
            Some("wake.dispatch"),
            Some("wake.dispatch")
        ],
        "R1-R3/E4 every Worker-owned terminal delivery stays inside its claim relay"
    );
}

#[tokio::test]
async fn recovering_a_terminal_run_keeps_undelivered_unbound_input() {
    // Test design. Causes: C1 terminal committed truth omits an unbound message
    // that arrived after the attempt; C2 recovery settles the terminal Run.
    // Effects: E1 C2 does not consume the omitted row; E2 a later Run receives it.
    // Constraint/Invariant: terminal recovery may consume only input evidenced in
    // the committed transcript. Decision rule: exercise the absent-id partition
    // and require later delivery, complementing the present-id test above.
    // The dual of the fix: an unbound row that the crashed attempt did NOT deliver
    // (its message id is absent from the committed transcript — it arrived after the
    // terminal commit) must be LEFT in the inbox on recovery, not swept away. No loss.
    let runtime = input_echo_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let thread = ThreadId(THREAD.to_string());
    let run = RunId("run-1".to_string());

    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    let _crashed = store
        .claim("crasher", LEASE, 0, &Default::default())
        .await
        .unwrap()
        .expect("claim");
    // The crashed attempt commits a terminal record that does NOT contain u1.
    let ctx = awaken_runtime_contract::runtime_context::RuntimeRunContext::new()
        .with_commit(commit.clone());
    let state = awaken_runtime_contract::execution::RunExecutor::execute(
        runtime.as_ref(),
        activation("run-1"),
        ctx,
    )
    .await
    .unwrap();
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    // u1 arrives AFTER the commit (never delivered to run-1).
    store
        .append(pending(
            "u1",
            "",
            "",
            ResumeResult::Input("late".to_string()),
        ))
        .await
        .unwrap();

    // Recovery settles the terminal run Done but must not consume the undelivered u1.
    let worker =
        DispatchWorker::new(runtime, store.clone(), commit.clone(), "w").with_lease_ms(LEASE);
    assert_eq!(
        worker.tick(harness::clock(LEASE + 1)).await.unwrap(),
        Some((run, RunState::Ended(EndCause::NaturalEnd)))
    );
    assert!(
        list_has_u1(store.as_ref(), &thread).await,
        "an unbound input that was never delivered survives recovery for a future run"
    );
}

/// Whether the unbound `u1` input is still listed for the thread.
async fn list_has_u1<S: Dispatch>(store: &S, thread: &ThreadId) -> bool {
    store
        .list(thread)
        .await
        .unwrap()
        .iter()
        .any(|r| r.input.message_id == "u1")
}

// --- 3. Atomic commit — no torn commit --------------------------------------

#[tokio::test]
async fn commit_is_atomic_and_survives_replay_with_no_orphans() {
    // Test design. Causes: C1 a file-backed SQLite Run commits await, resume, and
    // terminal deltas; C2 the store is dropped/reopened; C3 a later commit is
    // rejected by terminal-finality. Effects: E1 C1 is one atomic projection;
    // E2 C2 rebuilds identical truth; E3 C3 adds no partial rows. Constraint/
    // Invariant: messages, state, events, run fact, ticket, and fence commit or
    // roll back together. Decision rule: cover successful replay and an inducible
    // rejected commit because the public API cannot tear a transaction mid-write.
    // A commit writes messages + state + events + run-fact + ticket in ONE SQLite
    // transaction (`write_commit`). We drive a fully-embedded durable run (SQLite
    // dispatch queue AND SQLite commit boundary) through await -> resume -> end over
    // a FILE-backed store, then re-open the store so the read projection is rebuilt
    // purely from the durable log. The re-opened truth must equal the pre-drop
    // truth: same commit fence, same messages, the run-fact present and terminal,
    // and the ticket cleared — i.e. projection == replay, with no orphan message
    // left without its run-fact. That is the atomic-success invariant.
    //
    // A genuine MID-transaction tear cannot be induced through the public API: the
    // whole commit is a single `BEGIN IMMEDIATE` transaction, so any failure rolls
    // the whole thing back by construction. We therefore pin the atomic-success +
    // durable-replay invariant, and separately show that a REJECTED commit (the
    // terminal-is-final fence) adds zero rows — a real, inducible failed commit
    // that leaves no partial state.
    let path = std::env::temp_dir().join(format!(
        "awaken_crash_settle_commit_{}_{}.db",
        std::process::id(),
        now_nanos()
    ));
    let path = path.to_str().unwrap().to_string();
    let _ = std::fs::remove_file(&path);
    let thread = ThreadId(THREAD.to_string());
    let run = RunId("run-1".to_string());

    // Drive the run and capture the committed truth, then drop everything.
    let (fence, messages, state) = {
        let (runtime, ran) = tool_runtime();
        let store = Arc::new(SqliteDispatchStore::open_in_memory().expect("dispatch"));
        let commit = Arc::new(SqliteCommitCoordinator::open(&path).expect("commit"));
        let worker =
            DispatchWorker::new(runtime, store.clone(), commit.clone(), "w").with_lease_ms(LEASE);

        store
            .enqueue(RunDispatch::new(activation("run-1")))
            .await
            .unwrap();
        assert_eq!(
            worker.tick(harness::clock(0)).await.unwrap(),
            Some((run.clone(), RunState::Awaiting)),
            "the fresh run awaits on the gate"
        );
        // While awaiting, the ticket is committed durably.
        assert!(
            commit.resume_ticket_for(&run).is_some(),
            "the awaiting ticket is committed while awaiting"
        );

        store
            .append(pending("good", "run-1", TICKET, ResumeResult::allow()))
            .await
            .unwrap();
        assert_eq!(
            worker.tick(harness::clock(1)).await.unwrap(),
            Some((run.clone(), RunState::Ended(EndCause::NaturalEnd))),
            "the correctly-correlated input resumes the run to completion"
        );
        assert_eq!(ran.load(Ordering::SeqCst), 1, "the gated tool ran once");

        // The terminal-is-final fence: a post-terminal commit for the same run is
        // REJECTED and must add NO rows.
        let fence_before = commit.commit_count();
        let msgs_before = CommittedThreadView::committed_messages(commit.as_ref(), &thread).len();
        let rejected = CommitCoordinator::commit(
            commit.as_ref(),
            ThreadCommit {
                thread_id: thread.clone(),
                run: RunDisposition::ended(run.clone(), EndCause::NaturalEnd),
                messages: vec![Message::text(
                    MessageId("x1".to_string()),
                    Role::Assistant,
                    "SHOULD NOT PERSIST",
                )],
                state: Vec::new(),
                events: Vec::new(),
            },
        )
        .await;
        assert!(
            rejected.is_err(),
            "a post-terminal commit is rejected (terminal-is-final)"
        );
        assert_eq!(
            commit.commit_count(),
            fence_before,
            "a rejected commit does not advance the fence"
        );
        assert_eq!(
            CommittedThreadView::committed_messages(commit.as_ref(), &thread).len(),
            msgs_before,
            "a rejected commit adds no message rows (no partial write)"
        );

        // Ticket cleared on the terminal resume; the run-fact is terminal.
        assert!(
            commit.resume_ticket_for(&run).is_none(),
            "the ticket is cleared atomically with the terminal commit"
        );
        let record = CommittedThreadView::run(commit.as_ref(), &run).expect("run record");
        assert_eq!(record.state, RunState::Ended(EndCause::NaturalEnd));

        (
            commit.commit_count(),
            CommittedThreadView::committed_messages(commit.as_ref(), &thread),
            record.state,
        )
    };

    assert!(
        fence >= 2,
        "at least an await commit and a resume commit landed"
    );
    assert!(
        !messages.is_empty(),
        "the run committed a transcript (user + assistant + tool messages)"
    );

    // Re-open from the same file: the projection is rebuilt PURELY from the durable
    // log. If the commit were torn, the re-hydrated truth would diverge.
    let reopened = SqliteCommitCoordinator::open(&path).expect("re-open");
    assert_eq!(
        reopened.commit_count(),
        fence,
        "the durable fence survives a fresh hydrate (no lost or partial commit)"
    );
    let replayed = CommittedThreadView::committed_messages(&reopened, &thread);
    assert_eq!(
        replayed, messages,
        "every committed message survives replay from the durable log"
    );
    let record = CommittedThreadView::run(&reopened, &run).expect("run-fact present after replay");
    assert_eq!(
        record.state, state,
        "the run-fact is present and terminal after replay — no orphan message without its run-fact"
    );
    assert!(
        reopened.resume_ticket_for(&run).is_none(),
        "the cleared ticket stays cleared after replay (ticket cleared atomically)"
    );

    let _ = std::fs::remove_file(&path);
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}
