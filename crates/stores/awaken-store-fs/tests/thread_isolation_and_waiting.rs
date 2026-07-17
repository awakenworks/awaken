//! Two properties the shared conformance suite does not run against the fs store:
//! multi-thread isolation (which the fs store does NOT hold — a characterized
//! divergence) and waiting-ticket durability across a reopen (which it does).
//!
//! The fs backend reuses the single-thread in-memory reference as its read model
//! (`awaken-store-inmem`), so two threads committed to one store are flattened onto
//! the LAST committed thread — a real divergence from the thread-keyed SQLite /
//! Postgres backends. The first test pins the ACTUAL (buggy) behavior so a fix that
//! isolates threads will make it fail loudly and force this file to be updated.

use awaken_agent_contract::agent::message::{Id as MsgId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::agent::waiting::{WaitingReason, WaitingTicket};
use awaken_agent_contract::thread::commit::RunFact;
use awaken_agent_contract::thread::commit::coordinator::Coordinator;
use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use awaken_agent_contract::thread::read::checkpoint::CheckpointReader;
use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
use awaken_store_fs::FsCommitCoordinator;

async fn fresh(name: &str) -> (FsCommitCoordinator, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("awaken_store_fs_iso_{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    let store = FsCommitCoordinator::open(&dir).await.expect("open");
    (store, dir)
}

fn ended(thread: &str, run: &str, text: &str) -> ThreadCommit {
    ThreadCommit {
        thread_id: ThreadId(thread.to_string()),
        run_fact: RunFact {
            run_id: RunId(run.to_string()),
            phase: Phase::Ended(EndCause::NaturalEnd),
        },
        messages: vec![Message::text(
            MsgId(format!("m-{text}")),
            Role::Assistant,
            text,
        )],
        state: Vec::new(),
        events: Vec::new(),
        waiting: None,
    }
}

// KNOWN BUG (adjudicate): the fs store does NOT isolate threads. It reuses the
// single-thread in-memory reference as its read model, which keeps one `thread_id`
// and one flat message vector, returning a thread's transcript only when that
// thread was the LAST committed. So committing thread B after thread A makes
// thread A's transcript unreadable (lost) and leaks A's message into B's read
// (union). The thread-keyed SQLite / Postgres backends isolate correctly (they run
// the shared `two_threads_in_one_store_are_isolated` conformance case); the fs and
// in-memory backends are single-thread by construction. This test pins the current
// behavior so the divergence is visible and a fix trips it.
#[tokio::test]
async fn two_threads_in_one_store_are_flattened_not_isolated() {
    let (store, dir) = fresh("flatten").await;
    let ta = ThreadId("t-a".to_string());
    let tb = ThreadId("t-b".to_string());

    store.commit(ended("t-a", "r-a", "alpha")).await.expect("A");
    store.commit(ended("t-b", "r-b", "beta")).await.expect("B");

    // Thread A's transcript is LOST after thread B commits (flattening to the last
    // thread), rather than isolated as its own single message.
    assert!(
        store.committed_messages(&ta).is_empty(),
        "KNOWN BUG: thread A's transcript is lost, not isolated"
    );
    // Thread B's read LEAKS thread A's message (the flat vector is returned whole).
    let b = store.committed_messages(&tb);
    assert_eq!(
        b.len(),
        2,
        "KNOWN BUG: thread B leaks thread A's message (both alpha+beta)"
    );
    // latest_run for thread A is likewise unreadable (the single latest slot holds B).
    assert!(
        store.latest_run(&ta).is_none(),
        "KNOWN BUG: thread A's latest run is shadowed by thread B"
    );
    assert_eq!(
        store.latest_run(&tb).map(|r| r.id),
        Some(RunId("r-b".to_string()))
    );

    let _ = std::fs::remove_dir_all(&dir);
}

fn ticket(thread: &str, run: &str) -> WaitingTicket {
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

// The fs conformance run only ever commits `waiting: None`; this pins that a parked
// `Waiting` ticket is durable — it survives a drop + reopen because the append-only
// log records the parking commit and replay re-parks it (ADR-0039 D4 / G5).
#[tokio::test]
async fn waiting_ticket_is_durable_across_reopen() {
    let dir = std::env::temp_dir().join("awaken_store_fs_wait_durable");
    let _ = std::fs::remove_dir_all(&dir);
    let run = RunId("r1".to_string());

    {
        let store = FsCommitCoordinator::open(&dir).await.expect("open");
        store
            .commit(ThreadCommit {
                thread_id: ThreadId("t1".to_string()),
                run_fact: RunFact {
                    run_id: run.clone(),
                    phase: Phase::Waiting,
                },
                messages: vec![Message::text(
                    MsgId("m1".to_string()),
                    Role::Assistant,
                    "parked",
                )],
                state: Vec::new(),
                events: Vec::new(),
                waiting: Some(ticket("t1", "r1")),
            })
            .await
            .expect("park");
        assert!(
            store.waiting_ticket(&run).is_some(),
            "parked before restart"
        );
        // store dropped — the in-memory read model is gone; only the log remains
    }

    let reopened = FsCommitCoordinator::open(&dir).await.expect("reopen");
    assert!(
        reopened.waiting_ticket(&run).is_some(),
        "the active waiting ticket rehydrated from the durable log"
    );
    assert_eq!(
        reopened.waiting_ticket(&run).map(|t| t.correlation_id),
        Some("corr-1".to_string()),
        "the rehydrated ticket carries its correlation"
    );

    // And a later resume/terminal commit clears it durably, too.
    reopened
        .commit(ThreadCommit {
            thread_id: ThreadId("t1".to_string()),
            run_fact: RunFact {
                run_id: run.clone(),
                phase: Phase::Ended(EndCause::NaturalEnd),
            },
            messages: Vec::new(),
            state: Vec::new(),
            events: Vec::new(),
            waiting: None,
        })
        .await
        .expect("resume to terminal");
    assert!(
        reopened.waiting_ticket(&run).is_none(),
        "a terminal run clears its ticket (fail closed)"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
