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
//!  1. a bound pending input for a parked run survives a claim-without-settle and
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

use std::sync::Arc;
use std::sync::atomic::Ordering;

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::commit::coordinator::Coordinator as CommitCoordinator;
use awaken_agent_contract::commit::staged::ThreadCommit;
use awaken_agent_contract::fact::run::Fact as RunFact;
use awaken_agent_contract::store::run_store::RunStore;
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_run_ingress::{
    Dispatch, DispatchOutcome, DispatchQueue, DispatchWorker, Inbox, MemoryDispatchStore,
    RunExecutionRequest, SqliteDispatchStore,
};
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime_contract::resume::ResumeResult;
use awaken_store_sqlite::SqliteCommitCoordinator;

use harness::{THREAD, TICKET, activation, input_echo_runtime, pending, tool_runtime};

const LEASE: u64 = 1_000;

fn allow() -> ResumeResult {
    ResumeResult::Decision {
        allow: true,
        note: None,
    }
}

// --- 1. Bound pending survives a crash-before-settle and re-delivers once -----

#[tokio::test]
async fn crash_before_settle_re_delivers_bound_pending_exactly_once() {
    // A run parks on a gate holding a committed waiting ticket. Its correctly
    // correlated pending input arrives. An owner claims the woken run (the store
    // HANDS the pending) but crashes before settling — modeled by dropping the
    // claim. The pending must survive (consumed on settle, not on read). The
    // recovering owner then reclaims after the lease lapses, delivers the SAME
    // pending exactly once (the gated tool runs once), and only THEN is the pending
    // consumed. It is never delivered a third time.
    let (runtime, ran) = tool_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let run = RunId("run-1".to_string());

    let worker =
        DispatchWorker::new(runtime, store.clone(), commit.clone(), "w").with_lease_ms(LEASE);

    // The fresh run parks on the gate; the gated tool has not run.
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();
    assert_eq!(
        worker.tick(0).await.unwrap(),
        Some((run.clone(), Phase::Waiting)),
        "the fresh run parks on the gate ticket"
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
        .claim("crasher", LEASE, 10)
        .await
        .unwrap()
        .expect("the parked run with due input is claimable");
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
    let recovered = worker.tick(10 + LEASE + 1).await.unwrap();
    assert_eq!(
        recovered,
        Some((run.clone(), Phase::Ended(EndCause::NaturalEnd))),
        "the recovering owner delivers the re-delivered input and drives to Ended"
    );
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "the re-delivered input drove the gated tool exactly once"
    );

    // Consumed only after settle, and never re-delivered a third time.
    assert_eq!(
        store.pending_count(&run),
        0,
        "the pending is consumed only after the settling drive"
    );
    assert_eq!(store.dispatch_count(), 0, "the settled dispatch is gone");
    assert!(
        worker.tick(10 + 2 * LEASE).await.unwrap().is_none(),
        "nothing is left to drive"
    );
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "and the tool never ran again"
    );
}

/// Store-level spec (both backends must match): a claim HANDS a run's bound pending
/// but does not remove it, so an owner that never settles (a crash) leaves the
/// pending for the next claim; only a settle that NAMES it consumes it.
async fn assert_bound_pending_survives_claim_without_settle<S: Dispatch>(store: &S) {
    let run = RunId("run-1".to_string());

    // Park the run so a wake claim will hand its bound pending.
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();
    assert!(store.claim("w", LEASE, 0).await.unwrap().is_some());
    store
        .settle(&run, DispatchOutcome::Parked, &[])
        .await
        .unwrap();

    // Its bound pending arrives.
    store
        .append(pending(
            "good",
            "run-1",
            TICKET,
            ResumeResult::Input("hi".to_string()),
        ))
        .await
        .unwrap();

    // An owner wakes the run (claim hands the pending) then crashes before settle.
    let crashed = store
        .claim("crasher", LEASE, 10)
        .await
        .unwrap()
        .expect("wake claim");
    assert!(
        crashed.pending.iter().any(|p| p.message_id == "good"),
        "the claim hands the bound pending"
    );
    drop(crashed);

    // Consumed on settle, not on read: a re-claim after the lease lapses re-hands
    // the SAME pending input.
    let recovered = store
        .claim("w2", LEASE, 10 + LEASE + 1)
        .await
        .unwrap()
        .expect("recovery re-claim");
    assert!(
        recovered.pending.iter().any(|p| p.message_id == "good"),
        "the crash left the pending to be re-delivered on the next claim"
    );

    // Only a settle that names it consumes it.
    store
        .settle(&run, DispatchOutcome::Done, &["good".to_string()])
        .await
        .unwrap();
    assert!(
        store
            .list(&ThreadId(THREAD.to_string()))
            .await
            .unwrap()
            .is_empty(),
        "the pending is removed only by the settle that consumes it"
    );
}

#[tokio::test]
async fn bound_pending_survives_claim_without_settle_memory() {
    assert_bound_pending_survives_claim_without_settle(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn bound_pending_survives_claim_without_settle_sqlite() {
    assert_bound_pending_survives_claim_without_settle(
        &SqliteDispatchStore::open_in_memory().expect("open"),
    )
    .await;
}

// --- 2. Unbound idle-thread input consumed on settle, not on read -------------

#[tokio::test]
async fn crash_before_settle_re_delivers_unbound_inbox_input_exactly_once() {
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
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();
    let crashed = store
        .claim("crasher", LEASE, 0)
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
    let recovered = worker.tick(LEASE + 1).await.unwrap();
    assert_eq!(
        recovered,
        Some((
            RunId("run-1".to_string()),
            Phase::Ended(EndCause::NaturalEnd)
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
        .enqueue(RunExecutionRequest::new(harness::activation("run-2")))
        .await
        .unwrap();
    let next = worker.tick(LEASE + 2).await.unwrap();
    assert_eq!(
        next,
        Some((
            RunId("run-2".to_string()),
            Phase::Ended(EndCause::NaturalEnd)
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
    // Regression: the harder crash window. A fresh run drains unbound idle-thread
    // input into its activation, COMMITS its terminal record, then crashes BEFORE
    // settle. On recovery the worker sees a terminal committed record and settles
    // Done from committed truth. It must ALSO consume the unbound inbox rows that the
    // crashed attempt already folded into the committed transcript — otherwise those
    // rows linger and a later fresh run on the thread drains the SAME user input a
    // second time (a duplicate delivery across runs). Committed truth is authority:
    // a row IS consumed iff its message id appears in the committed transcript.
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
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();
    let _crashed = store
        .claim("crasher", LEASE, 0)
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
    let phase =
        awaken_runtime_contract::execution::RunExecutor::execute(runtime.as_ref(), act, ctx)
            .await
            .unwrap();
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
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

    // Recovery: the worker reclaims the now-terminal run, settles Done, and consumes
    // the delivered unbound input from committed truth.
    let worker = DispatchWorker::new(runtime.clone(), store.clone(), commit.clone(), "w")
        .with_lease_ms(LEASE);
    let recovered = worker.tick(LEASE + 1).await.unwrap();
    assert_eq!(
        recovered,
        Some((run.clone(), Phase::Ended(EndCause::NaturalEnd))),
        "the terminal record is settled Done without re-execution"
    );
    assert!(
        !list_has_u1(store.as_ref(), &thread).await,
        "the delivered unbound input is consumed on the recovery settle, not orphaned"
    );

    // A later fresh run on the same thread does NOT drain u1 again: exactly one
    // committed delivery of the user's input across the whole thread.
    store
        .enqueue(RunExecutionRequest::new(harness::activation("run-2")))
        .await
        .unwrap();
    assert_eq!(
        worker.tick(LEASE + 2).await.unwrap(),
        Some((
            RunId("run-2".to_string()),
            Phase::Ended(EndCause::NaturalEnd)
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
}

#[tokio::test]
async fn recovering_a_terminal_run_keeps_undelivered_unbound_input() {
    // The dual of the fix: an unbound row that the crashed attempt did NOT deliver
    // (its message id is absent from the committed transcript — it arrived after the
    // terminal commit) must be LEFT in the inbox on recovery, not swept away. No loss.
    let runtime = input_echo_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let thread = ThreadId(THREAD.to_string());
    let run = RunId("run-1".to_string());

    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();
    let _crashed = store
        .claim("crasher", LEASE, 0)
        .await
        .unwrap()
        .expect("claim");
    // The crashed attempt commits a terminal record that does NOT contain u1.
    let ctx = awaken_runtime_contract::runtime_context::RuntimeRunContext::new()
        .with_commit(commit.clone());
    let phase = awaken_runtime_contract::execution::RunExecutor::execute(
        runtime.as_ref(),
        activation("run-1"),
        ctx,
    )
    .await
    .unwrap();
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
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
        worker.tick(LEASE + 1).await.unwrap(),
        Some((run, Phase::Ended(EndCause::NaturalEnd)))
    );
    assert!(
        list_has_u1(store.as_ref(), &thread).await,
        "an unbound input that was never delivered survives recovery for a future run"
    );
}

/// Store-level spec (both backends must match): unbound idle-thread input is
/// consumed on settle, not on read. Listing it (the worker's drain READ) never
/// removes it; a settle that does NOT name it (a crash that lost the drain record)
/// leaves it for re-delivery; only a settle that NAMES it consumes it.
async fn assert_unbound_consumed_on_settle_not_on_read<S: Dispatch>(store: &S) {
    let thread = ThreadId(THREAD.to_string());

    store
        .append(pending("u1", "", "", ResumeResult::Input("hi".to_string())))
        .await
        .unwrap();

    // A read (list) does not consume: listing twice leaves it both times.
    assert!(list_has_u1(store, &thread).await, "listed once");
    assert!(
        list_has_u1(store, &thread).await,
        "a read does not consume the unbound input"
    );

    // A settle that does NOT name u1 leaves it — this is the crash case, where the
    // owner drained u1 in memory but died before recording it in the settle.
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();
    assert!(store.claim("w", LEASE, 0).await.unwrap().is_some());
    store
        .settle(&RunId("run-1".to_string()), DispatchOutcome::Done, &[])
        .await
        .unwrap();
    assert!(
        list_has_u1(store, &thread).await,
        "a settle that does not name the unbound input leaves it for re-delivery"
    );

    // Only a settle that NAMES it consumes it.
    store
        .enqueue(RunExecutionRequest::new(harness::activation("run-2")))
        .await
        .unwrap();
    assert!(store.claim("w", LEASE, 0).await.unwrap().is_some());
    store
        .settle(
            &RunId("run-2".to_string()),
            DispatchOutcome::Done,
            &["u1".to_string()],
        )
        .await
        .unwrap();
    assert!(
        !list_has_u1(store, &thread).await,
        "the unbound input is consumed by the settle that names it"
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

#[tokio::test]
async fn unbound_consumed_on_settle_not_on_read_memory() {
    assert_unbound_consumed_on_settle_not_on_read(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn unbound_consumed_on_settle_not_on_read_sqlite() {
    assert_unbound_consumed_on_settle_not_on_read(
        &SqliteDispatchStore::open_in_memory().expect("open"),
    )
    .await;
}

// --- 3. Atomic commit — no torn commit --------------------------------------

#[tokio::test]
async fn commit_is_atomic_and_survives_replay_with_no_orphans() {
    // A commit writes messages + state + events + run-fact + ticket in ONE SQLite
    // transaction (`write_commit`). We drive a fully-embedded durable run (SQLite
    // dispatch queue AND SQLite commit boundary) through park -> resume -> end over
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
    let (fence, messages, phase) = {
        let (runtime, ran) = tool_runtime();
        let store = Arc::new(SqliteDispatchStore::open_in_memory().expect("dispatch"));
        let commit = Arc::new(SqliteCommitCoordinator::open(&path).expect("commit"));
        let worker =
            DispatchWorker::new(runtime, store.clone(), commit.clone(), "w").with_lease_ms(LEASE);

        store
            .enqueue(RunExecutionRequest::new(activation("run-1")))
            .await
            .unwrap();
        assert_eq!(
            worker.tick(0).await.unwrap(),
            Some((run.clone(), Phase::Waiting)),
            "the fresh run parks on the gate"
        );
        // While parked, the ticket is committed durably.
        assert!(
            commit.waiting_for(&run).is_some(),
            "the waiting ticket is committed while parked"
        );

        store
            .append(pending(
                "good",
                "run-1",
                TICKET,
                ResumeResult::Decision {
                    allow: true,
                    note: None,
                },
            ))
            .await
            .unwrap();
        assert_eq!(
            worker.tick(1).await.unwrap(),
            Some((run.clone(), Phase::Ended(EndCause::NaturalEnd))),
            "the correctly-correlated input resumes the run to completion"
        );
        assert_eq!(ran.load(Ordering::SeqCst), 1, "the gated tool ran once");

        // The terminal-is-final fence: a post-terminal commit for the same run is
        // REJECTED and must add NO rows.
        let fence_before = commit.commit_count();
        let msgs_before = ThreadReader::committed_messages(commit.as_ref(), &thread).len();
        let rejected = CommitCoordinator::commit(
            commit.as_ref(),
            ThreadCommit {
                thread_id: thread.clone(),
                run_fact: RunFact {
                    run_id: run.clone(),
                    phase: Phase::Ended(EndCause::NaturalEnd),
                },
                messages: vec![Message::text(
                    MessageId("x1".to_string()),
                    Role::Assistant,
                    "SHOULD NOT PERSIST",
                )],
                state: Vec::new(),
                events: Vec::new(),
                outbox: Vec::new(),
                waiting: None,
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
            ThreadReader::committed_messages(commit.as_ref(), &thread).len(),
            msgs_before,
            "a rejected commit adds no message rows (no partial write)"
        );

        // Ticket cleared on the terminal resume; the run-fact is terminal.
        assert!(
            commit.waiting_for(&run).is_none(),
            "the ticket is cleared atomically with the terminal commit"
        );
        let record = RunStore::get(commit.as_ref(), &run).expect("run record");
        assert_eq!(record.phase, Phase::Ended(EndCause::NaturalEnd));

        (
            commit.commit_count(),
            ThreadReader::committed_messages(commit.as_ref(), &thread),
            record.phase,
        )
    };

    assert!(
        fence >= 2,
        "at least a park commit and a resume commit landed"
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
    let replayed = ThreadReader::committed_messages(&reopened, &thread);
    assert_eq!(
        replayed, messages,
        "every committed message survives replay from the durable log"
    );
    let record = RunStore::get(&reopened, &run).expect("run-fact present after replay");
    assert_eq!(
        record.phase, phase,
        "the run-fact is present and terminal after replay — no orphan message without its run-fact"
    );
    assert!(
        reopened.waiting_for(&run).is_none(),
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
