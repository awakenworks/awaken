//! G1 / G13 commit boundary conformance tests (memory coordinator).
//!
//! G1: durable runtime writes go through `CommitCoordinator`; `ThreadCommit`
//! validates the commit plan; projections are after-commit.
//!
//! G13: store truth is single-source within a commit; projection visible ONLY
//! after `commit()` returns `Ok`; no partial state observable during in-flight
//! or failed commits.

use awaken_agent_contract::agent::awaiting::{AwaitTarget, PauseReason, ResumeTicket};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::state::{
    Command as StateCommand, Key as StateKey, MergePolicy, Scope, Store,
};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::coordinator::Coordinator;
use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_store_inmem::MemoryCommitCoordinator;

fn ended(run: &str) -> RunDisposition {
    RunDisposition::ended(RunId(run.to_string()), EndCause::NaturalEnd)
}

fn ticket(run: &str, thread: &str) -> ResumeTicket {
    ResumeTicket::new(
        "corr-1",
        RunId(run.to_string()),
        ThreadId(thread.to_string()),
        "snap-1",
        "fp-1",
        AwaitTarget::Pause(PauseReason::Manual),
    )
}

// G13: before any commit the projection has no truth — no partial state is
// pre-visible from a staged but uncommitted plan.
#[tokio::test]
async fn g13_projection_absent_before_first_commit() {
    let store = MemoryCommitCoordinator::new();
    assert!(
        store.run(&RunId("run-1".to_string())).is_none(),
        "projection must be empty before any commit"
    );
    assert_eq!(store.commit_count(), 0);
}

// G13: the run record is visible ONLY after `commit()` returns `Ok`. The same
// shared reference sees absent-before and present-after in one test.
#[tokio::test]
async fn g13_projection_visible_only_after_commit_returns_ok() {
    let store = MemoryCommitCoordinator::new();
    let run = RunId("run-1".to_string());

    assert!(store.run(&run).is_none(), "projection absent before commit");

    store
        .commit(ThreadCommit {
            thread_id: ThreadId("thread-1".to_string()),
            run: ended("run-1"),
            messages: vec![],
            state: vec![],
            events: vec![],
        })
        .await
        .expect("commit ok");

    let record = store.run(&run).expect("projection visible after commit");
    assert_eq!(record.state, RunState::Ended(EndCause::NaturalEnd));
}

// G13: a rejected commit writes nothing — the projection stays empty after a
// failed `commit()` call so no partial state is ever observable.
#[tokio::test]
async fn g13_failed_commit_leaves_no_partial_state() {
    let store = MemoryCommitCoordinator::new();
    let run = RunId("run-1".to_string());

    // Empty thread_id fails validate() before any lock is taken.
    let err = store
        .commit(ThreadCommit {
            thread_id: ThreadId(String::new()),
            run: ended("run-1"),
            messages: vec![],
            state: vec![],
            events: vec![],
        })
        .await;
    assert!(err.is_err(), "invalid plan must be rejected");

    assert!(
        store.run(&run).is_none(),
        "no partial state after failed commit"
    );
    assert_eq!(
        store.commit_count(),
        0,
        "fence unchanged after rejected commit"
    );
}

#[tokio::test]
async fn g13_invalid_state_batch_is_atomic_at_the_real_commit_boundary() {
    // Cause/effect table:
    // R1 exact repeated Exclusive Set in one ThreadCommit -> reject, no Run,
    // state, or sequence mutation. R2 admitted Run/Shared commands -> bind only
    // Run scope, commit once, and rebuild the same materialized Store. Kani and
    // state unit tests cover complementary policy/action combinations.
    let store = MemoryCommitCoordinator::new();
    let thread = ThreadId("state-thread".into());
    let run = RunId("state-run".into());
    let invalid = ThreadCommit::assemble(
        thread.clone(),
        RunDisposition::running(run.clone()),
        true,
        Vec::new(),
        vec![
            StateCommand::set(
                Scope::Thread,
                MergePolicy::Exclusive,
                "lock",
                serde_json::json!(1),
            ),
            StateCommand::set(
                Scope::Thread,
                MergePolicy::Exclusive,
                "lock",
                serde_json::json!(2),
            ),
        ],
        Vec::new(),
    );
    assert!(store.commit(invalid).await.is_err(), "R1 rejects");
    assert_eq!(store.commit_count(), 0, "R1 preserves the sequence fence");
    assert!(store.run(&run).is_none(), "R1 publishes no Run fact");
    assert!(
        store.committed_state(&thread).is_empty(),
        "R1 publishes no state prefix"
    );

    let admitted = ThreadCommit::assemble(
        thread.clone(),
        RunDisposition::running(run.clone()),
        true,
        Vec::new(),
        vec![
            StateCommand::set(
                Scope::Run,
                MergePolicy::Disjoint,
                "run.value",
                serde_json::json!(1),
            ),
            StateCommand::set(
                Scope::Shared,
                MergePolicy::Commutative,
                "shared.value",
                serde_json::json!({"a": 1}),
            ),
        ],
        Vec::new(),
    );
    store.commit(admitted).await.expect("R2 commits");
    let commands = store.committed_state(&thread);
    assert_eq!(commands[0].run_id.as_ref(), Some(&run), "R2 Run binding");
    assert_eq!(commands[1].run_id, None, "R2 Shared remains unbound");
    let rebuilt = Store::rebuild(&commands);
    assert_eq!(
        rebuilt.get(Scope::Run, &StateKey("run.value".into())),
        Some(&serde_json::json!(1)),
        "R2 committed replay retains Run state"
    );
    assert_eq!(
        rebuilt.get(Scope::Shared, &StateKey("shared.value".into())),
        Some(&serde_json::json!({"a": 1})),
        "R2 committed replay retains Shared state"
    );
    assert_eq!(store.commit_count(), 1, "R2 advances exactly once");
}

// G1: `ThreadCommit::validate` rejects an empty `thread_id` before any store
// write, so a malformed plan never reaches the durable boundary.
#[test]
fn g1_validate_rejects_empty_thread_id() {
    let commit = ThreadCommit {
        thread_id: ThreadId(String::new()),
        run: ended("run-1"),
        messages: vec![],
        state: vec![],
        events: vec![],
    };
    assert!(
        commit.validate().is_err(),
        "empty thread_id must fail validation"
    );
}

// G1: `ThreadCommit::validate` rejects an empty `run_id`.
#[test]
fn g1_validate_rejects_empty_run_id() {
    let commit = ThreadCommit {
        thread_id: ThreadId("thread-1".to_string()),
        run: RunDisposition::ended(RunId(String::new()), EndCause::NaturalEnd),
        messages: vec![],
        state: vec![],
        events: vec![],
    };
    assert!(
        commit.validate().is_err(),
        "empty run_id must fail validation"
    );
}

// G1: `ThreadCommit::validate` rejects an awaiting ticket whose `run_id` does
// not match the commit's own `run_id` — a mismatched ticket would await the
// wrong run or leave an orphaned ticket.
#[test]
fn g1_validate_rejects_mismatched_resume_ticket_run_id() {
    let wire = serde_json::json!({
        "thread_id": "thread-1",
        "run_fact": { "run_id": "run-1", "phase": "Awaiting" },
        "messages": [],
        "state": [],
        "events": [],
        "waiting": ticket("run-2", "thread-1"),
    });
    assert!(
        serde_json::from_value::<ThreadCommit>(wire).is_err(),
        "legacy input with a mismatched ticket must fail deserialization"
    );
}

// G1: a well-formed commit plan with a consistent awaiting ticket passes validate.
#[test]
fn g1_validate_accepts_consistent_resume_ticket() {
    let commit = ThreadCommit {
        thread_id: ThreadId("thread-1".to_string()),
        run: RunDisposition::awaiting(ticket("run-1", "thread-1")),
        messages: vec![],
        state: vec![],
        events: vec![],
    };
    assert!(
        commit.validate().is_ok(),
        "consistent awaiting ticket must pass validation"
    );
}
