//! G1 / G13 commit boundary conformance tests (memory coordinator).
//!
//! G1: durable runtime writes go through `CommitCoordinator`; `ThreadCommit`
//! validates the commit plan; projections are after-commit.
//!
//! G13: store truth is single-source within a commit; projection visible ONLY
//! after `commit()` returns `Ok`; no partial state observable during in-flight
//! or failed commits.

use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::agent::waiting::{WaitingReason, WaitingTicket};
use awaken_agent_contract::commit::coordinator::Coordinator;
use awaken_agent_contract::commit::staged::ThreadCommit;
use awaken_agent_contract::fact::run::Fact as RunFact;
use awaken_agent_contract::store::run_store::RunStore;
use awaken_runtime::memory::MemoryCommitCoordinator;

fn ended(run: &str) -> RunFact {
    RunFact {
        run_id: RunId(run.to_string()),
        phase: Phase::Ended(EndCause::NaturalEnd),
    }
}

fn ticket(run: &str, thread: &str) -> WaitingTicket {
    WaitingTicket {
        correlation_id: "corr-1".to_string(),
        run_id: RunId(run.to_string()),
        thread_id: ThreadId(thread.to_string()),
        snapshot_id: "snap-1".to_string(),
        catalog_fingerprint: "fp-1".to_string(),
        reason: WaitingReason::ToolPermission,
        call_id: Some("call-1".to_string()),
        pending_tool: None,
        deadline_ms: None,
    }
}

// G13: before any commit the projection has no truth — no partial state is
// pre-visible from a staged but uncommitted plan.
#[tokio::test]
async fn g13_projection_absent_before_first_commit() {
    let store = MemoryCommitCoordinator::new();
    assert!(
        store.get(&RunId("run-1".to_string())).is_none(),
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

    assert!(store.get(&run).is_none(), "projection absent before commit");

    store
        .commit(ThreadCommit {
            thread_id: ThreadId("thread-1".to_string()),
            run_fact: ended("run-1"),
            messages: vec![],
            state: vec![],
            events: vec![],
            outbox: Vec::new(),
            waiting: None,
        })
        .await
        .expect("commit ok");

    let record = store.get(&run).expect("projection visible after commit");
    assert_eq!(record.phase, Phase::Ended(EndCause::NaturalEnd));
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
            run_fact: ended("run-1"),
            messages: vec![],
            state: vec![],
            events: vec![],
            outbox: Vec::new(),
            waiting: None,
        })
        .await;
    assert!(err.is_err(), "invalid plan must be rejected");

    assert!(
        store.get(&run).is_none(),
        "no partial state after failed commit"
    );
    assert_eq!(
        store.commit_count(),
        0,
        "fence unchanged after rejected commit"
    );
}

// G1: `ThreadCommit::validate` rejects an empty `thread_id` before any store
// write, so a malformed plan never reaches the durable boundary.
#[test]
fn g1_validate_rejects_empty_thread_id() {
    let commit = ThreadCommit {
        thread_id: ThreadId(String::new()),
        run_fact: ended("run-1"),
        messages: vec![],
        state: vec![],
        events: vec![],
        outbox: Vec::new(),
        waiting: None,
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
        run_fact: RunFact {
            run_id: RunId(String::new()),
            phase: Phase::Ended(EndCause::NaturalEnd),
        },
        messages: vec![],
        state: vec![],
        events: vec![],
        outbox: Vec::new(),
        waiting: None,
    };
    assert!(
        commit.validate().is_err(),
        "empty run_id must fail validation"
    );
}

// G1: `ThreadCommit::validate` rejects a waiting ticket whose `run_id` does
// not match the commit's own `run_id` — a mismatched ticket would park the
// wrong run or leave an orphaned ticket.
#[test]
fn g1_validate_rejects_mismatched_waiting_ticket_run_id() {
    let commit = ThreadCommit {
        thread_id: ThreadId("thread-1".to_string()),
        run_fact: RunFact {
            run_id: RunId("run-1".to_string()),
            phase: Phase::Waiting,
        },
        messages: vec![],
        state: vec![],
        events: vec![],
        outbox: Vec::new(),
        waiting: Some(ticket("run-2", "thread-1")),
    };
    assert!(
        commit.validate().is_err(),
        "mismatched waiting ticket run_id must fail validation"
    );
}

// G1: a well-formed commit plan with a consistent waiting ticket passes validate.
#[test]
fn g1_validate_accepts_consistent_waiting_ticket() {
    let commit = ThreadCommit {
        thread_id: ThreadId("thread-1".to_string()),
        run_fact: RunFact {
            run_id: RunId("run-1".to_string()),
            phase: Phase::Waiting,
        },
        messages: vec![],
        state: vec![],
        events: vec![],
        outbox: Vec::new(),
        waiting: Some(ticket("run-1", "thread-1")),
    };
    assert!(
        commit.validate().is_ok(),
        "consistent waiting ticket must pass validation"
    );
}
