//! Negative-path / edge-case correctness tests for the durable dispatch worker.
//!
//! These drive `DispatchWorker` directly (claim + drive + settle) rather than
//! through the higher-level ingress, so they pin the worker's own decisions at the
//! claim/drive/settle seam:
//!
//! 2. a duplicate submit (same run id) is deduped — one dispatch row, one drive;
//!    and the durable dedupe index (V0008) blocks a duplicate dedupe key;
//! 3. unbound idle-thread input is drained into a fresh run exactly once and
//!    consumed on settle (a crash before settle would re-deliver) — including the
//!    regression that a non-`Input` unbound row never desyncs the drain and panics;
//! 4. two Run-bound fresh continuations on one Thread are serialized and each
//!    drive receives only its own new input;
//! 5. input whose correlation does not match the committed awaiting ticket is
//!    dropped without delivery and the run is left awaiting.
//!
//! Behavior 1 (an illegal non-settled `Running` executor result must fail loudly)
//! is unit-tested at its decision point in `worker.rs::tests` — the worker binds a
//! concrete `Runtime` (it routes a run to its thread's runtime), and a real runtime
//! never returns `Running`, so the invariant is asserted where the state is mapped.

mod harness;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use awaken_agent_contract::agent::message::Role;
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_run_ingress::{
    ContinuationAdmission, DispatchQueue, DispatchWorker, Inbox, MemoryDispatchStore, Outbox,
    RunDispatch, SqliteDispatchStore, SubmitOptions,
};
use awaken_runtime_contract::resume::ResumeResult;
use awaken_store_inmem::MemoryCommitCoordinator;

use harness::{
    THREAD, TICKET, activation, counting_text_runtime, input_echo_runtime, tool_runtime,
};

const LEASE: u64 = 1_000;

fn allow() -> ResumeResult {
    ResumeResult::allow()
}

// --- 2. Duplicate submit idempotency ---------------------------------------

#[tokio::test]
async fn duplicate_submit_same_run_id_drives_exactly_once() {
    // Test design. Causes: C1 the same canonical Run id/payload is submitted
    // twice; C2 the Worker drives available work. Effects: E1 C1 creates one row;
    // E2 C2 infers and commits once. Constraint/Invariant: exact Run-id replay is
    // idempotent and cannot fork live work. Decision rule: submit twice before one
    // drive, then assert row, inference, and terminal cardinality.
    // Submitting the SAME run id twice must not create two live runs or double-drive:
    // the second enqueue is a no-op, so exactly one dispatch row exists and the
    // model is driven exactly once.
    let (runtime, infers) = counting_text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());

    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    assert_eq!(
        store.dispatch_count(),
        1,
        "a duplicate run id does not create a second dispatch row"
    );

    let worker = DispatchWorker::new(runtime, store.clone(), commit, "solo").with_lease_ms(LEASE);
    let processed = worker.tick(harness::clock(0)).await.unwrap();
    assert_eq!(
        processed,
        Some((
            RunId("run-1".to_string()),
            RunState::Ended(EndCause::NaturalEnd)
        )),
        "the single dispatch drives to completion"
    );
    assert_eq!(
        infers.load(Ordering::SeqCst),
        1,
        "the deduped run drove exactly once"
    );

    // Nothing is left to drive — no second live run was created.
    assert!(
        worker.tick(harness::clock(1)).await.unwrap().is_none(),
        "no second live run exists to drive"
    );
    assert_eq!(infers.load(Ordering::SeqCst), 1, "and never re-executed");
    assert_eq!(store.dispatch_count(), 0, "the settled dispatch is gone");
}

#[tokio::test]
async fn dedupe_key_blocks_a_duplicate_dispatch_on_sqlite() {
    // Prove the durable dedupe index (V0008) holds: two DISTINCT run ids sharing a
    // live dedupe key yield only ONE claimable dispatch — the second submit is a
    // no-op while the first is live.
    let store = SqliteDispatchStore::open_in_memory().expect("open");
    let key = SubmitOptions {
        dedupe_key: Some("k".to_string()),
        ..Default::default()
    };

    // Distinct threads so single-writer-per-thread (ADR-0022) does not itself fence
    // the second claim — only the dedupe key should.
    store
        .enqueue_with(
            RunDispatch::new(harness::activation_on("d1", "t1")),
            key.clone(),
        )
        .await
        .unwrap();
    store
        .enqueue_with(RunDispatch::new(harness::activation_on("d2", "t2")), key)
        .await
        .unwrap();

    let first = store
        .claim("w", LEASE, 0, &Default::default())
        .await
        .unwrap()
        .expect("first");
    assert_eq!(first.request.run_id().0, "d1");
    assert!(
        store
            .claim("w", LEASE, 0, &Default::default())
            .await
            .unwrap()
            .is_none(),
        "the duplicate dedupe key was never enqueued (V0008)"
    );
}

// --- 3. Orphan / unbound thread-inbox drain --------------------------------

#[tokio::test]
async fn unbound_inbox_input_is_delivered_once_and_consumed_on_settle() {
    // Test design. Causes: C1 idle-Thread input has no Run/correlation; C2 the
    // next fresh Run claims that Thread; C3 it settles. Effects: E1 C2 freezes and
    // delivers input once; E2 C3 consumes it. Constraint/Invariant: reading alone
    // never consumes, preserving crash redelivery. Decision rule: exercise the
    // normal claim+settle branch and require one transcript copy plus empty inbox.
    // Input addressed to a thread with no run yet (empty run id) is drained into a
    // fresh run's activation, reaches the model, and is removed on settle. It is
    // consumed on settle (not on read), so a crash before settle would re-deliver.
    let runtime = input_echo_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());

    // An unbound idle-thread message (empty run + correlation) waits for the thread.
    store
        .append(harness::pending(
            "u1",
            "",
            "",
            ResumeResult::Input("from-inbox".to_string()),
        ))
        .await
        .unwrap();

    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    let worker =
        DispatchWorker::new(runtime, store.clone(), commit.clone(), "solo").with_lease_ms(LEASE);
    let processed = worker.tick(harness::clock(0)).await.unwrap();
    assert_eq!(
        processed,
        Some((
            RunId("run-1".to_string()),
            RunState::Ended(EndCause::NaturalEnd)
        ))
    );

    // The echo model replies with the user input it saw — proving the unbound
    // message reached the fresh run exactly once.
    let assistant = commit
        .committed()
        .messages
        .iter()
        .find(|m| m.role == Role::Assistant)
        .expect("assistant reply")
        .clone();
    assert!(
        assistant.text_content().contains("from-inbox"),
        "the unbound inbox input reached the fresh run (got {:?})",
        assistant.text_content()
    );

    // It is consumed on settle: the thread inbox no longer lists it.
    assert!(
        store
            .list(&ThreadId(THREAD.to_string()))
            .await
            .unwrap()
            .is_empty(),
        "the unbound input is consumed on settle, not left to re-deliver"
    );
}

#[tokio::test]
async fn a_non_input_unbound_row_does_not_desync_the_drain() {
    // Test design. Causes: C1 skipped non-Input unbound rows precede C2 one valid
    // unbound Input. Effects: E1 C1 is ignored without moving the delivered-input
    // insertion index; E2 C2 reaches the activation without panic. Constraint/
    // Invariant: the index counts delivered inputs, not scanned rows. Decision rule:
    // place two skipped rows before one valid row and require normal settle.
    // Regression: the drain inserts each delivered unbound *input* at a running
    // position, so a non-`Input` unbound row (skipped, not delivered) never shifts
    // the insert index past the activation's end. Before the fix, two skipped rows
    // ahead of an input pushed `Vec::insert` out of bounds and panicked.
    let runtime = input_echo_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());

    // Two non-Input unbound rows precede one unbound Input, all on the idle thread.
    store
        .append(harness::pending("d0", "", "", allow()))
        .await
        .unwrap();
    store
        .append(harness::pending("d1", "", "", allow()))
        .await
        .unwrap();
    store
        .append(harness::pending(
            "i2",
            "",
            "",
            ResumeResult::Input("keep".to_string()),
        ))
        .await
        .unwrap();

    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    let worker =
        DispatchWorker::new(runtime, store.clone(), commit.clone(), "solo").with_lease_ms(LEASE);

    // The drive completes (no panic) and delivers only the Input row.
    let processed = worker.tick(harness::clock(0)).await.unwrap();
    assert_eq!(
        processed,
        Some((
            RunId("run-1".to_string()),
            RunState::Ended(EndCause::NaturalEnd)
        )),
        "the fresh run drove to completion without a panic"
    );
    let assistant = commit
        .committed()
        .messages
        .iter()
        .find(|m| m.role == Role::Assistant)
        .expect("assistant reply")
        .clone();
    assert!(
        assistant.text_content().contains("keep"),
        "the unbound Input still reached the run"
    );

    // Every drained row — delivered or skipped — is consumed on settle.
    assert!(
        store
            .list(&ThreadId(THREAD.to_string()))
            .await
            .unwrap()
            .is_empty(),
        "all drained unbound rows are consumed on settle"
    );
}

// --- 4. Exact fresh-Run continuation binding -------------------------------

#[tokio::test]
async fn fresh_continuations_on_one_thread_receive_only_their_bound_input() {
    // Cause/effect graph: C1 input is generic Thread-unbound or bound to a fresh
    // Run; C2 one/two fresh Runs share the Thread; C3 the earlier Run is
    // pending/running/done. Effects: E1 generic unbound remains eligible for the
    // next fresh Run; E2 Run-bound input reaches only its named Run; E3 the
    // same-Thread writer fence serializes Runs; E4 settle consumes only the
    // driven Run's input.
    //
    // Decision table: R1 unbound+one fresh => E1 (covered by the preceding
    // `unbound_inbox_input...` test); R2 two bound inputs+two pending Runs =>
    // first drive gets first only, E2+E3; R3 first done+second pending => second
    // drive gets second only, E2+E4. This test owns R2/R3 and prevents the old
    // Thread-wide unbound drain from coalescing both messages into R2.
    // Constraint/Invariant: each Run-bound input is eligible only for its named
    // Run while the shared Thread fence serializes writers. Decision rule: execute
    // R2 then R3; R1 remains owned by the preceding unbound-input test.
    let runtime = input_echo_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());

    let mut first = RunDispatch::new(activation("run-bound-first"));
    first.activation.input.clear();
    let mut second = RunDispatch::new(activation("run-bound-second"));
    second.activation.input.clear();
    let first_input = harness::pending(
        "bound-first",
        "run-bound-first",
        "",
        ResumeResult::Input("first".to_string()),
    );
    let second_input = harness::pending(
        "bound-second",
        "run-bound-second",
        "",
        ResumeResult::Input("second".to_string()),
    );
    store
        .relay_and_enqueue(first_input, first, ContinuationAdmission::Root)
        .await
        .expect("admit first bound continuation");
    store
        .relay_and_enqueue(second_input, second, ContinuationAdmission::Root)
        .await
        .expect("admit second bound continuation");

    let worker =
        DispatchWorker::new(runtime, store.clone(), commit.clone(), "solo").with_lease_ms(LEASE);
    assert_eq!(
        worker.tick(harness::clock(0)).await.expect("drive first"),
        Some((
            RunId("run-bound-first".to_string()),
            RunState::Ended(EndCause::NaturalEnd),
        )),
        "R2 the first same-Thread continuation runs first"
    );
    let first_assistant = commit
        .committed()
        .messages
        .iter()
        .filter(|message| message.role == Role::Assistant)
        .map(|message| message.text_content())
        .collect::<Vec<_>>();
    assert_eq!(
        first_assistant,
        vec!["first"],
        "R2/E2 the first Run cannot drain the second Run's bound input"
    );
    let remaining = store
        .list(&ThreadId(THREAD.to_string()))
        .await
        .expect("list remaining bound input");
    assert_eq!(
        remaining
            .iter()
            .map(|record| record.input.message_id.as_str())
            .collect::<Vec<_>>(),
        vec!["bound-second"],
        "R2/E4 settling the first Run preserves the second Run's input"
    );

    assert_eq!(
        worker.tick(harness::clock(1)).await.expect("drive second"),
        Some((
            RunId("run-bound-second".to_string()),
            RunState::Ended(EndCause::NaturalEnd),
        )),
        "R3 the second continuation becomes runnable after the first settles"
    );
    let assistants = commit
        .committed()
        .messages
        .iter()
        .filter(|message| message.role == Role::Assistant)
        .map(|message| message.text_content())
        .collect::<Vec<_>>();
    assert_eq!(
        assistants,
        vec!["first", "first|second"],
        "R3/E2 the second Run sees persistent Thread history plus exactly its own new input"
    );
    assert!(
        store
            .list(&ThreadId(THREAD.to_string()))
            .await
            .expect("list consumed inputs")
            .is_empty(),
        "R3/E4 both inputs are consumed by their named Runs"
    );
}

// --- 5. Stale / superseded ticket input dropped ----------------------------

#[tokio::test]
async fn stale_correlation_input_is_dropped_and_the_run_stays_awaiting() {
    // Test design. Causes: C1 the Run owns committed correlation A; C2 input uses
    // stale correlation B; C3 exact A follows. Effects: E1 C2 is dropped and the
    // tool remains idle; E2 the Run stays Awaiting; E3 C3 resumes once.
    // Constraint/Invariant: correlation equality is required at delivery time.
    // Decision rule: exercise B then A and assert both state transitions.
    // A awaiting run holds a committed awaiting ticket. Input whose correlation does
    // NOT match that ticket is dropped without delivery — the tool never runs and
    // the run stays awaiting — until the correctly-correlated input arrives.
    let (runtime, ran) = tool_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let thread = ThreadId(THREAD.to_string());
    let run = RunId("run-1".to_string());

    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    let worker =
        DispatchWorker::new(runtime, store.clone(), commit.clone(), "solo").with_lease_ms(LEASE);

    // The fresh run awaits on the gate's ticket; the gated tool has not run.
    let awaiting = worker.tick(harness::clock(0)).await.unwrap();
    assert_eq!(awaiting, Some((run.clone(), RunState::Awaiting)));
    assert_eq!(ran.load(Ordering::SeqCst), 0, "the gated tool has not run");

    // Deliver input with the WRONG correlation. The worker wakes, finds no input
    // answering the committed ticket, and re-awaits without applying it.
    store
        .append(harness::pending(
            "stale",
            "run-1",
            "wrong-correlation",
            allow(),
        ))
        .await
        .unwrap();
    let after_stale = worker.tick(harness::clock(1)).await.unwrap();
    assert_eq!(
        after_stale,
        Some((run.clone(), RunState::Awaiting)),
        "a stale input leaves the run awaiting"
    );
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "the stale input did not drive the tool"
    );
    assert_eq!(
        store.awaiting_run(&thread).await.unwrap(),
        Some(run.clone()),
        "the run is still awaiting on its thread"
    );
    assert_eq!(
        store.pending_count(&run),
        0,
        "the stale input was dropped (consumed without delivery)"
    );

    // The correctly-correlated input finally resumes the run to completion.
    store
        .append(harness::pending("good", "run-1", TICKET, allow()))
        .await
        .unwrap();
    let resumed = worker.tick(harness::clock(2)).await.unwrap();
    assert_eq!(resumed, Some((run, RunState::Ended(EndCause::NaturalEnd))));
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "the matching input drove the pending tool exactly once"
    );
}
