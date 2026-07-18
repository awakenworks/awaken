//! Two durability properties the shared conformance suite (which uses a single,
//! live store) does not exercise against the fs store: multi-thread isolation
//! ACROSS a drop + reopen, and awaiting-ticket durability across a reopen.
//!
//! The fs backend reuses the thread-keyed in-memory reference as its read model
//! (`awaken-store-inmem`) and rebuilds it from the append-only log on open, so two
//! threads committed to one store stay isolated — and that isolation is durable: a
//! fresh instance over the same log replays each thread's own transcript. (The live,
//! single-instance isolation invariant is covered by the shared
//! `two_threads_in_one_store_are_isolated` conformance case in `conformance.rs`.)

use awaken_agent_contract::agent::awaiting::{AwaitReason, ResumeTicket};
use awaken_agent_contract::agent::message::{Id as MsgId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::coordinator::Coordinator;
use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use awaken_agent_contract::thread::read::checkpoint::CheckpointReader;
use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
use awaken_store_fs::FsCommitCoordinator;

fn ended(thread: &str, run: &str, text: &str) -> ThreadCommit {
    ThreadCommit {
        thread_id: ThreadId(thread.to_string()),
        run: RunDisposition::ended(RunId(run.to_string()), EndCause::NaturalEnd),
        messages: vec![Message::text(
            MsgId(format!("m-{text}")),
            Role::Assistant,
            text,
        )],
        state: Vec::new(),
        events: Vec::new(),
    }
}

// Two threads committed to one store stay isolated even across a drop + reopen: the
// append-only log records both threads' commits, and replay rebuilds the thread-keyed
// read model, so each thread reads only its OWN transcript and latest run from a fresh
// instance. (The live-instance isolation invariant is the shared conformance case; this
// pins that durability adds nothing that would re-flatten the threads on replay.)
#[tokio::test]
async fn two_threads_stay_isolated_across_reopen() {
    let dir = std::env::temp_dir().join("awaken_store_fs_iso_reopen");
    let _ = std::fs::remove_dir_all(&dir);
    let ta = ThreadId("t-a".to_string());
    let tb = ThreadId("t-b".to_string());

    {
        let store = FsCommitCoordinator::open(&dir).await.expect("open");
        store.commit(ended("t-a", "r-a", "alpha")).await.expect("A");
        store.commit(ended("t-b", "r-b", "beta")).await.expect("B");
        // store dropped — the in-memory read model is gone; only the log remains
    }

    let reopened = FsCommitCoordinator::open(&dir).await.expect("reopen");

    // Each thread reads exactly its own message — no loss, no leak — after replay.
    let a = reopened.committed_messages(&ta);
    assert_eq!(
        a.len(),
        1,
        "thread A reads only its own message after reopen"
    );
    assert_eq!(a[0].text_content(), "alpha");
    let b = reopened.committed_messages(&tb);
    assert_eq!(
        b.len(),
        1,
        "thread B reads only its own message after reopen"
    );
    assert_eq!(b[0].text_content(), "beta");

    // Each thread's latest run is its own, not shadowed by the other.
    assert_eq!(
        reopened.latest_run(&ta).map(|r| r.id),
        Some(RunId("r-a".to_string()))
    );
    assert_eq!(
        reopened.latest_run(&tb).map(|r| r.id),
        Some(RunId("r-b".to_string()))
    );

    let _ = std::fs::remove_dir_all(&dir);
}

fn ticket(thread: &str, run: &str) -> ResumeTicket {
    ResumeTicket {
        correlation_id: "corr-1".to_string(),
        run_id: RunId(run.to_string()),
        thread_id: ThreadId(thread.to_string()),
        snapshot_id: "snap-1".to_string(),
        catalog_fingerprint: "fp-1".to_string(),
        reason: AwaitReason::ToolPermission,
        call_id: Some("call-1".to_string()),
        pending_tool: None,
        deadline_ms: None,
    }
}

// The fs conformance run only ever commits `awaiting: None`; this pins that an awaiting
// `Awaiting` ticket is durable — it survives a drop + reopen because the append-only
// log records the awaiting commit and replay re-awaits it (ADR-0039 D4 / G5).
#[tokio::test]
async fn resume_ticket_is_durable_across_reopen() {
    let dir = std::env::temp_dir().join("awaken_store_fs_wait_durable");
    let _ = std::fs::remove_dir_all(&dir);
    let run = RunId("r1".to_string());

    {
        let store = FsCommitCoordinator::open(&dir).await.expect("open");
        store
            .commit(ThreadCommit {
                thread_id: ThreadId("t1".to_string()),
                run: RunDisposition::awaiting(ticket("t1", "r1")),
                messages: vec![Message::text(
                    MsgId("m1".to_string()),
                    Role::Assistant,
                    "awaiting",
                )],
                state: Vec::new(),
                events: Vec::new(),
            })
            .await
            .expect("await");
        assert!(
            store.resume_ticket(&run).is_some(),
            "awaiting before restart"
        );
        // store dropped — the in-memory read model is gone; only the log remains
    }

    let reopened = FsCommitCoordinator::open(&dir).await.expect("reopen");
    assert!(
        reopened.resume_ticket(&run).is_some(),
        "the active awaiting ticket rehydrated from the durable log"
    );
    assert_eq!(
        reopened.resume_ticket(&run).map(|t| t.correlation_id),
        Some("corr-1".to_string()),
        "the rehydrated ticket carries its correlation"
    );

    // And a later resume/terminal commit clears it durably, too.
    reopened
        .commit(ThreadCommit {
            thread_id: ThreadId("t1".to_string()),
            run: RunDisposition::ended(run.clone(), EndCause::NaturalEnd),
            messages: Vec::new(),
            state: Vec::new(),
            events: Vec::new(),
        })
        .await
        .expect("resume to terminal");
    assert!(
        reopened.resume_ticket(&run).is_none(),
        "a terminal run clears its ticket (fail closed)"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
